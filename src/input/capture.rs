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
use crate::hotkey::{HOTKEY_ID_MIRROR, HOTKEY_ID_TRANSFER};
use crate::net::udp;
use crate::state::MasterState;

// 전역 상태 : wnd_proc 안에서 접근해야 하므로 static 사용.
// 스레드는 하나 (메인 스레드가 메시지 루프) → 접근 순서상 안전.
static SEQ: AtomicU32 = AtomicU32::new(1);
static mut G_SEND_SOCK: Option<std::net::UdpSocket> = None;
static mut G_EPOCH: Option<Instant> = None;
static mut G_TCP_TX: Option<TcpTx> = None;

/// 엣지 크로싱 검출용 이전 X 좌표. i32::MIN 은 "아직 초기화 안 됨" 센티넬.
static PREV_EDGE_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);

/// Master 시작 후 grace period. 이 시간 동안엔 엣지 크로싱 감지 안 함.
/// 재시작 시 커서가 우측 끝 근처에 앉아있어서 첫 이동에 실수 트리거 되는 것 방지.
static STARTUP_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
const EDGE_GRACE_MS: u128 = 3000;

pub fn run_with_ctx(ctx: MasterCtx) -> Result<()> {
    let peer: SocketAddr = format!("{}:{}", ctx.cfg.peer_ip, ctx.cfg.udp_port).parse()
        .with_context(|| format!("peer 주소 파싱 실패"))?;
    let sock = udp::bind_send(peer)?;
    unsafe {
        G_SEND_SOCK = Some(sock);
        G_EPOCH = Some(Instant::now());
        G_TCP_TX = Some(ctx.tcp_tx);
    }
    crate::input::hooks::FORWARD_KEYBOARD.store(ctx.cfg.forward_keyboard, Ordering::Release);

    // Startup 시간 기록 → 엣지 크로싱 grace period 용.
    let _ = STARTUP_TIME.set(Instant::now());

    // 크로싱 검출용 이전 X 좌표를 현재 커서 위치로 초기화.
    // 이렇게 안 하면 사용자가 우측 끝까지 한 이벤트에 도달 시 그게 baseline 으로 흡수돼서
    // 이후 어떤 이동에도 트리거 안 되는 UX 후퇴가 발생.
    if let Some(pt) = cursor::get_pos() {
        PREV_EDGE_X.store(pt.x, Ordering::Release);
        tracing::debug!("master: crossing 검출 baseline 초기화 x={}", pt.x);
    }
    tracing::info!("master: UDP 송신 → {} 준비", peer);

    unsafe {
        let hwnd = create_message_window()?;
        register_raw_input(hwnd)?;
        let hk = HotkeySpec {
            toggle: ctx.cfg.hotkey_toggle.clone(),
            mirror: ctx.cfg.hotkey_mirror.clone(),
            transfer: ctx.cfg.hotkey_transfer.clone(),
            return_hotkey: ctx.cfg.hotkey_return.clone(),
        };
        if let Err(e) = hotkey::register_toggle(hwnd, &hk) {
            tracing::warn!("master: hotkey 등록 실패, 계속 진행: {}", e);
        }
        if let Err(e) = hotkey::register_mirror(hwnd, &hk) {
            tracing::warn!("master: mirror hotkey 등록 실패, 계속 진행: {}", e);
        }
        if let Err(e) = hotkey::register_transfer(hwnd, &hk) {
            tracing::warn!("master: transfer hotkey 등록 실패, 계속 진행: {}", e);
        }
        // return hotkey 는 RegisterHotKey 대신 LL 훅에서 감지. VK 를 hooks::RETURN_VK 에 저장.
        if let Some(vk) = hotkey::parse_return_hotkey(&hk) {
            crate::input::hooks::RETURN_VK.store(vk as u32, Ordering::Release);
            tracing::info!("master: return hotkey VK={:#x} 등록 (LL 훅에서 감지)", vk);
        }
        if let Err(e) = crate::tray::add(hwnd, "cursorlink (master)") {
            tracing::warn!("master: 트레이 아이콘 실패, 계속 진행: {}", e);
        }
        // 저수준 훅은 startup 에 설치하지 않음. Local 상태에선 훅 없이 Raw Input 만.
        // 게임 anti-cheat 이 훅 감지해서 방어 반응하는 것을 방지.
        // 훅은 transfer_to_remote / toggle_mirror 에서 필요할 때 install, return_to_local 에서 uninstall.
        tracing::info!("master: Raw Input 등록 완료");
        message_loop();
        crate::input::hooks::uninstall();
    }
    Ok(())
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
                let id = wparam.0 as i32;
                if id == HOTKEY_ID_TOGGLE {
                    let cur = master::SHARED.enabled.load(std::sync::atomic::Ordering::Acquire);
                    master::set_enabled(!cur, G_TCP_TX.as_ref());
                } else if id == HOTKEY_ID_MIRROR {
                    if let Some(tx) = &G_TCP_TX {
                        master::toggle_mirror(tx);
                    }
                } else if id == HOTKEY_ID_TRANSFER {
                    if let Some(tx) = &G_TCP_TX {
                        master::transfer_to_remote_center(tx);
                    }
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
                        master::set_enabled(!cur, G_TCP_TX.as_ref());
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
    // windows-rs 0.58: RIM_TYPEMOUSE/RIM_TYPEKEYBOARD 는 RID_DEVICE_INFO_TYPE(u32) wrapper.
    if ri.header.dwType == RIM_TYPEMOUSE.0 {
        handle_mouse(&ri.data.mouse);
    } else if ri.header.dwType == RIM_TYPEKEYBOARD.0 {
        handle_keyboard(&ri.data.keyboard);
    }
}

unsafe fn handle_mouse(m: &RAWMOUSE) {
    let state = master::SHARED.get();
    let enabled = master::SHARED.enabled.load(Ordering::Acquire);
    if !enabled || state == MasterState::Disconnected { return; }

    let dx = m.lLastX;
    let dy = m.lLastY;
    // windows-rs 0.58: usFlags 는 MOUSE_STATE(u16), MOUSE_MOVE_ABSOLUTE 도 동일 wrapper.
    let is_relative = (m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) == 0;

    // usButtonFlags: u16 (RI_MOUSE_BUTTON_* 비트 or), usButtonData: u16 (실은 signed short)
    let btn_flags: u16 = m.Anonymous.Anonymous.usButtonFlags;
    let button_data: i16 = m.Anonymous.Anonymous.usButtonData as i16;

    let mirror = master::SHARED.mirror.load(Ordering::Acquire);

    match state {
        MasterState::Local => {
            if mirror {
                // Mirror 모드: 현재 커서 위치를 % 로 Slave 에 전송 (커서 락 X).
                // 이동만 있든, 버튼/휠만 있든, 이벤트 발생 시 반드시 위치 sync 먼저 → 그 다음 액션.
                // (Raw Input 이 버튼/휠을 dx=dy=0 인 별도 이벤트로 전달하므로 델타 체크만으론 부족.)
                let has_motion = is_relative && (dx != 0 || dy != 0);
                let has_action = btn_flags != 0;
                if has_motion || has_action {
                    send_mouse_pos_from_current();
                }
                forward_buttons_and_wheel(btn_flags, button_data);
            } else {
                // 일반 Local: 엣지 감지 (안쪽 → 우측 끝 크로싱 시에만 트리거).
                if is_relative && (dx != 0 || dy != 0) {
                    check_edge_and_transfer();
                }
            }
        }
        MasterState::Remote => {
            // Remote 상태: 델타/버튼/휠 모두 slave 로 전송.
            if is_relative && (dx != 0 || dy != 0) {
                let sdx = clamp_i32_to_i16(dx);
                let sdy = clamp_i32_to_i16(dy);
                send_move(sdx, sdy);
            }
            forward_buttons_and_wheel(btn_flags, button_data);
        }
        MasterState::Disconnected => {}
    }
}

unsafe fn forward_buttons_and_wheel(btn_flags: u16, button_data: i16) {
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
    if btn_flags & (RI_MOUSE_WHEEL  as u16) != 0 { send_wheel(0, button_data); }
    if btn_flags & (RI_MOUSE_HWHEEL as u16) != 0 { send_wheel(button_data, 0); }
}

/// Mirror 모드에서 현재 커서 위치를 화면 % 로 계산해서 Slave 에 전송.
fn send_mouse_pos_from_current() {
    let pt = match cursor::get_pos() { Some(p) => p, None => return };
    let scr = cursor::primary_screen();
    let w = scr.width().max(1) as i64;
    let h = scr.height().max(1) as i64;
    let x_ppm = (((pt.x - scr.left) as i64 * 10000) / w).clamp(0, 10000) as i16;
    let y_ppm = (((pt.y - scr.top)  as i64 * 10000) / h).clamp(0, 10000) as i16;
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MousePos, flags: 0, button: 0,
        dx: x_ppm, dy: y_ppm, wheel_dx: 0, wheel_dy: 0,
    });
}

/// Local 상태에서 커서가 오른쪽 끝을 크로싱 (안쪽에서 → 끝으로 진입) 하면 Remote 로 전환.
/// 크로싱 검출: 이전 X 좌표가 edge 미만이었고, 지금 X 가 edge 이상이면 트리거.
/// 이렇게 하면 커서가 처음부터 우측 끝에 앉아있는 상황에서 아무 이동에도 튀는 문제 해결.
unsafe fn check_edge_and_transfer() {
    // Mirror 상태면 Remote 로 안 넘어감.
    if master::SHARED.mirror.load(Ordering::Acquire) { return; }

    // Startup grace period: 재시작 시 커서가 우측 끝 근처에 있으면 첫 이동에 실수 트리거되는 것 방지.
    if let Some(start) = STARTUP_TIME.get() {
        if start.elapsed().as_millis() < EDGE_GRACE_MS {
            return;
        }
    }

    let pt = match cursor::get_pos() { Some(p) => p, None => return };
    let scr = cursor::primary_screen();
    let edge = scr.right - 1;

    // 이전 X 좌표를 스왑해서 가져옴 (동시에 새 값으로 갱신).
    let prev = PREV_EDGE_X.swap(pt.x, Ordering::AcqRel);

    // 첫 호출은 baseline 설정만 하고 트리거 안 함.
    if prev == i32::MIN { return; }

    // 크로싱: 이전엔 edge 미만이었는데 지금 edge 이상이면 실제 우측 진입임.
    // 커서가 이미 edge 에 앉아있는 상태에서 rightward 클램프된 이벤트가 계속 와도 무시.
    if prev < edge && pt.x >= edge {
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
    // config 로 키보드 전송 끌 수 있음. false 면 Slave 는 자체 키보드로 조작.
    if !crate::input::hooks::FORWARD_KEYBOARD.load(Ordering::Acquire) { return; }

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

/// LL 훅에서 Slave 로 TCP frame 보내야 할 때 (예: hotkey_return 감지) 접근용.
/// 훅 스레드는 메인 스레드와 같으므로 static 접근 안전.
pub unsafe fn tcp_tx_ref() -> Option<&'static TcpTx> {
    G_TCP_TX.as_ref()
}

pub fn send_key(scan: u16, down: bool, ext: bool) {
    let mut flags = 0u8;
    if down { flags |= FLAG_BTN_DOWN; }
    if ext  { flags |= FLAG_KEY_EXT; }
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::KeyEvent, flags,
        button: scan,
        dx: 0, dy: 0, wheel_dx: 0, wheel_dy: 0,
    });
}
