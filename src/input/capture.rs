// Master 쪽 입력 캡처.
//
// 진입: master::run 에서 MasterCtx 를 만들어 넘겨줌.
//
// 원리:
//  1. HWND_MESSAGE 로 message-only hidden window 생성
//  2. RegisterRawInputDevices 로 마우스/키보드 raw input 등록
//  3. WM_INPUT 수신 → GetRawInputData → 델타/버튼 추출
//  4. master::SHARED.get() 에 따라 라우팅:
//     - Local  : 델타 무시, GetCursorPos 로 엣지 감지 → 오른쪽 끝이면 transfer_to_remote
//     - Remote : 델타를 UDP 로 slave 에 송신
//     - Disconnected: 아무것도 안 함
//
// UDP 는 저지연을 위해 wnd_proc 안에서 직접 송신 (블로킹 소켓).

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
    HIGH_PRIORITY_CLASS, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::hotkey::{self, HotkeySpec, HOTKEY_ID_TOGGLE};
use crate::input::cursor;
use crate::master::{self, MasterCtx, TcpTx};
use crate::net::packet::{
    Kind, Packet, FLAG_BTN_DOWN, FLAG_KEY_EXT,
    MB_LEFT, MB_MIDDLE, MB_RIGHT, MB_X1, MB_X2,
};
use crate::net::udp;
use crate::state::MasterState;

// 전역 상태 : wnd_proc 안에서 접근해야 하므로 static 사용.
// 스레드는 하나 (메인 스레드가 메시지 루프) → 접근 순서상 안전.
static SEQ: AtomicU32 = AtomicU32::new(1);
static mut G_SEND_SOCK: Option<std::net::UdpSocket> = None;
static mut G_EPOCH: Option<Instant> = None;
static mut G_TCP_TX: Option<TcpTx> = None;

pub fn run_with_ctx(ctx: MasterCtx) -> Result<()> {
    unsafe { boost_priority(); }

    let peer: SocketAddr = format!("{}:{}", ctx.cfg.peer_ip, ctx.cfg.udp_port).parse()
        .with_context(|| format!("peer 주소 파싱 실패"))?;
    let sock = udp::bind_send(peer)?;
    unsafe {
        G_SEND_SOCK = Some(sock);
        G_EPOCH = Some(Instant::now());
        G_TCP_TX = Some(ctx.tcp_tx);
    }
    tracing::info!("master: UDP 송신 → {} 준비", peer);

    unsafe {
        let hwnd = create_message_window()?;
        register_raw_input(hwnd)?;
        let hk = HotkeySpec { toggle: ctx.cfg.hotkey_toggle.clone() };
        if let Err(e) = hotkey::register_toggle(hwnd, &hk) {
            tracing::warn!("master: hotkey 등록 실패, 계속 진행: {}", e);
        }
        if let Err(e) = crate::tray::add(hwnd, "cursorlink (master)") {
            tracing::warn!("master: 트레이 아이콘 실패, 계속 진행: {}", e);
        }
        tracing::info!("master: Raw Input 등록 완료");
        message_loop();
    }
    Ok(())
}

unsafe fn boost_priority() {
    let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
}

unsafe fn create_message_window() -> Result<HWND> {
    let hmod = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW 실패")?;
    let hinst = HINSTANCE(hmod.0);
    let class_name: Vec<u16> = "cursorlink_msgwnd\0".encode_utf16().collect();

    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
    };
    if RegisterClassW(&wc) == 0 { anyhow::bail!("RegisterClassW 실패"); }

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(0),
        0, 0, 0, 0,
        HWND_MESSAGE,
        None,
        hinst,
        None,
    ).context("CreateWindowExW 실패")?;
    Ok(hwnd)
}

unsafe fn register_raw_input(hwnd: HWND) -> Result<()> {
    let devs = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage:     0x02, // Mouse
            dwFlags:     RIDEV_INPUTSINK,
            hwndTarget:  hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage:     0x06, // Keyboard
            dwFlags:     RIDEV_INPUTSINK,
            hwndTarget:  hwnd,
        },
    ];
    RegisterRawInputDevices(&devs, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        .context("RegisterRawInputDevices 실패")?;
    Ok(())
}

unsafe fn message_loop() {
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
        if r.0 <= 0 { break; }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_INPUT => {
                handle_raw_input(lparam);
                LRESULT(0)
            }
            WM_HOTKEY => {
                if wparam.0 as i32 == HOTKEY_ID_TOGGLE {
                    let cur = master::SHARED.enabled.load(std::sync::atomic::Ordering::Acquire);
                    master::set_enabled(!cur);
                }
                LRESULT(0)
            }
            m if m == crate::tray::WM_TRAY => {
                let event = crate::tray::tray_event_from_lparam(lparam);
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    let enabled = master::SHARED.enabled.load(std::sync::atomic::Ordering::Acquire);
                    crate::tray::show_context_menu(hwnd, enabled);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match crate::tray::menu_id_from_wparam(wparam) {
                    crate::tray::MENU_TOGGLE => {
                        let cur = master::SHARED.enabled.load(std::sync::atomic::Ordering::Acquire);
                        master::set_enabled(!cur);
                    }
                    crate::tray::MENU_SETTINGS => {
                        crate::tray::open_in_editor(&crate::config::Config::config_path());
                    }
                    crate::tray::MENU_LOG => {
                        let mut p = std::env::current_exe().unwrap_or_default();
                        p.pop(); p.push("cursorlink.log");
                        crate::tray::open_in_editor(&p);
                    }
                    crate::tray::MENU_EXIT => {
                        crate::tray::remove(hwnd);
                        std::process::exit(0);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                crate::tray::remove(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn handle_raw_input(lparam: LPARAM) {
    let hri = HRAWINPUT(lparam.0 as *mut _);
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;

    let mut size: u32 = 0;
    if GetRawInputData(hri, RID_INPUT, None, &mut size, header_size) != 0 { return; }

    let mut buf = vec![0u8; size as usize];
    let got = GetRawInputData(
        hri, RID_INPUT,
        Some(buf.as_mut_ptr() as _),
        &mut size, header_size,
    );
    if got == u32::MAX || got == 0 { return; }

    let ri = &*(buf.as_ptr() as *const RAWINPUT);
    // dwType 는 u32 raw. RIM_TYPEMOUSE/RIM_TYPEKEYBOARD 는 u32 상수 (0/1).
    if ri.header.dwType == RIM_TYPEMOUSE {
        handle_mouse(&ri.data.mouse);
    } else if ri.header.dwType == RIM_TYPEKEYBOARD {
        handle_keyboard(&ri.data.keyboard);
    }
}

unsafe fn handle_mouse(m: &RAWMOUSE) {
    let state = master::SHARED.get();
    let enabled = master::SHARED.enabled.load(Ordering::Acquire);
    if !enabled || state == MasterState::Disconnected { return; }

    let dx = m.lLastX;
    let dy = m.lLastY;
    // RAWMOUSE.usFlags 는 windows 0.58 에서 u16 raw. MOUSE_MOVE_ABSOLUTE 도 u16 상수.
    let is_relative = (m.usFlags & MOUSE_MOVE_ABSOLUTE) == 0;

    // usButtonFlags: u16 (RI_MOUSE_BUTTON_* 비트 or), usButtonData: u16 (실은 signed short)
    let btn_flags: u16 = m.Anonymous.Anonymous.usButtonFlags;
    let button_data: i16 = m.Anonymous.Anonymous.usButtonData as i16;

    match state {
        MasterState::Local => {
            // Local 상태에서는 델타 전송 X. 대신 엣지 감지만.
            if is_relative && (dx != 0 || dy != 0) {
                check_edge_and_transfer();
            }
        }
        MasterState::Remote => {
            // Remote 상태: 델타/버튼/휠 모두 slave 로 전송.
            if is_relative && (dx != 0 || dy != 0) {
                let sdx = clamp_i32_to_i16(dx);
                let sdy = clamp_i32_to_i16(dy);
                send_move(sdx, sdy);
            }
            // 버튼 (RI_MOUSE_* 상수는 u32, 하지만 값은 u16 범위 → as u16 safe)
            if btn_flags & (RI_MOUSE_BUTTON_1_DOWN as u16) != 0 { send_button(MB_LEFT, true); }
            if btn_flags & (RI_MOUSE_BUTTON_1_UP   as u16) != 0 { send_button(MB_LEFT, false); }
            if btn_flags & (RI_MOUSE_BUTTON_2_DOWN as u16) != 0 { send_button(MB_RIGHT, true); }
            if btn_flags & (RI_MOUSE_BUTTON_2_UP   as u16) != 0 { send_button(MB_RIGHT, false); }
            if btn_flags & (RI_MOUSE_BUTTON_3_DOWN as u16) != 0 { send_button(MB_MIDDLE, true); }
            if btn_flags & (RI_MOUSE_BUTTON_3_UP   as u16) != 0 { send_button(MB_MIDDLE, false); }
            if btn_flags & (RI_MOUSE_BUTTON_4_DOWN as u16) != 0 { send_button(MB_X1, true); }
            if btn_flags & (RI_MOUSE_BUTTON_4_UP   as u16) != 0 { send_button(MB_X1, false); }
            if btn_flags & (RI_MOUSE_BUTTON_5_DOWN as u16) != 0 { send_button(MB_X2, true); }
            if btn_flags & (RI_MOUSE_BUTTON_5_UP   as u16) != 0 { send_button(MB_X2, false); }
            // 휠
            if btn_flags & (RI_MOUSE_WHEEL  as u16) != 0 { send_wheel(0, button_data); }
            if btn_flags & (RI_MOUSE_HWHEEL as u16) != 0 { send_wheel(button_data, 0); }
        }
        MasterState::Disconnected => {}
    }
}

/// Local 상태에서 커서가 오른쪽 끝에 닿았는지 체크. 닿았으면 Remote 로 전환.
unsafe fn check_edge_and_transfer() {
    let pt = match cursor::get_pos() { Some(p) => p, None => return };
    let scr = cursor::primary_screen();
    // 오른쪽 끝 임계값: 마지막 픽셀 (right-1). Windows 는 커서를 right-1 까지 허용.
    if pt.x >= scr.right - 1 {
        let entry_y_pct = if scr.height() > 0 {
            (((pt.y - scr.top) as i64 * 100) / scr.height() as i64).clamp(0, 100) as u8
        } else { 50 };
        if let Some(tx) = &G_TCP_TX {
            master::transfer_to_remote(tx, entry_y_pct);
        }
    }
}

unsafe fn handle_keyboard(k: &RAWKEYBOARD) {
    let state = master::SHARED.get();
    let enabled = master::SHARED.enabled.load(Ordering::Acquire);
    // Remote 상태에서만 키 전달. Local 이면 이 PC 가 정상 처리.
    if !enabled || state != MasterState::Remote { return; }

    // 눌림/떼짐 : Message == WM_KEYDOWN/WM_SYSKEYDOWN 이면 down.
    let is_key_up = (k.Flags & RI_KEY_BREAK as u16) != 0;
    let is_e0     = (k.Flags & RI_KEY_E0 as u16) != 0;
    let scan_code = k.MakeCode;

    // scan_code == 0 이면 유의미한 이벤트 아님 (예: 확장키의 escape sequence prefix).
    if scan_code == 0 { return; }

    send_key(scan_code, !is_key_up, is_e0);
}

fn clamp_i32_to_i16(v: i32) -> i16 {
    if v > i16::MAX as i32 { i16::MAX }
    else if v < i16::MIN as i32 { i16::MIN }
    else { v as i16 }
}

fn now_us() -> u32 {
    unsafe {
        match G_EPOCH {
            Some(e) => Instant::now().duration_since(e).as_micros() as u32,
            None => 0,
        }
    }
}

fn next_seq() -> u32 {
    // 0 은 slave 의 "미초기화" 마커로 예약 → 1부터
    let v = SEQ.fetch_add(1, Ordering::Relaxed);
    if v == 0 {
        SEQ.fetch_add(1, Ordering::Relaxed);
        1
    } else { v }
}

fn send_packet(p: Packet) {
    let bytes = p.encode();
    unsafe {
        if let Some(sock) = &G_SEND_SOCK {
            let _ = sock.send(&bytes);
        }
    }
}

fn send_move(dx: i16, dy: i16) {
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseMove, flags: 0, button: 0,
        dx, dy, wheel_dx: 0, wheel_dy: 0,
    });
}

fn send_button(button: u16, down: bool) {
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseButton,
        flags: if down { FLAG_BTN_DOWN } else { 0 },
        button, dx: 0, dy: 0, wheel_dx: 0, wheel_dy: 0,
    });
}

fn send_wheel(hx: i16, vy: i16) {
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseWheel, flags: 0, button: 0,
        dx: 0, dy: 0, wheel_dx: hx, wheel_dy: vy,
    });
}

fn send_key(scan: u16, down: bool, ext: bool) {
    let mut flags = 0u8;
    if down { flags |= FLAG_BTN_DOWN; }
    if ext  { flags |= FLAG_KEY_EXT; }
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::KeyEvent, flags,
        button: scan, // button 필드에 scan code
        dx: 0, dy: 0, wheel_dx: 0, wheel_dy: 0,
    });
}
