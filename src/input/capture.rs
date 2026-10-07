// Master 쪽 입력 캡처.
//
// 진입: master::run 이 슬레이브 연결 스레드를 띄운 뒤 run(cfg) 호출 (블로킹).
//
// 원리:
//  1. HWND_MESSAGE 로 message-only hidden window 생성
//  2. RegisterRawInputDevices 로 마우스/키보드 raw input 등록
//  3. WM_INPUT 수신 → GetRawInputData → 델타/버튼 추출
//  4. master::SHARED.get() 에 따라 라우팅:
//     - Local  : 델타 무시, GetCursorPos 로 엣지 감지 → 오른쪽/왼쪽 끝이면 그 쪽 슬레이브로 전환
//     - Remote : 델타를 UDP 로 제어 중인 slave 에 송신
//
// UDP 는 저지연을 위해 wnd_proc 안에서 직접 송신 (블로킹 소켓).
// TCP 스레드 이벤트 (master::WM_PEER_*) 도 이 창으로 PostMessage 돼서 메인 스레드에서 처리.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::hotkey::{
    self, HOTKEY_ID_EDGE, HOTKEY_ID_EDGE_LEFT, HOTKEY_ID_EDGE_RIGHT, HOTKEY_ID_MIRROR,
    HOTKEY_ID_TOGGLE, HOTKEY_ID_TRANSFER, HOTKEY_ID_TRANSFER_LEFT,
};
use crate::input::cursor;
use crate::input::hooks::{self, HookAction};
use crate::master;
use crate::net::packet::{
    Kind, Packet, FLAG_BTN_DOWN, FLAG_KEY_EXT,
    MB_LEFT, MB_MIDDLE, MB_RIGHT, MB_X1, MB_X2,
};
use crate::state::{MasterState, Side};

// 전역 상태 : wnd_proc 안에서 접근해야 하므로 static 사용.
// 스레드는 하나 (메인 스레드가 메시지 루프) → 접근 순서상 안전.
static SEQ: AtomicU32 = AtomicU32::new(1);
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// 엣지 크로싱 검출용 이전 X 좌표. i32::MIN 은 "아직 초기화 안 됨" 센티넬.
static PREV_EDGE_X: AtomicI32 = AtomicI32::new(i32::MIN);

/// Master 시작 후 grace period. 이 시간 동안엔 엣지 크로싱 감지 안 함.
/// 재시작 시 커서가 화면 끝 근처에 앉아있어서 첫 이동에 실수 트리거 되는 것 방지.
static STARTUP_TIME: OnceLock<Instant> = OnceLock::new();
const EDGE_GRACE_MS: u128 = 3000;

/// Slave 로 눌림을 보냈는데 아직 안 뗀 키 (bit = scan + ext*256) / 마우스 버튼 (bit = MB_*).
/// 슬레이브를 떠날 때 (복귀/미러 해제) key-up 을 보내 stuck key 방지.
static HELD_KEYS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static HELD_BTNS: AtomicU32 = AtomicU32::new(0);

pub fn run(cfg: Config) -> Result<()> {
    let _ = EPOCH.set(Instant::now());
    hooks::FORWARD_KEYBOARD.store(cfg.forward_keyboard, Ordering::Release);

    // Startup 시간 기록 → 엣지 크로싱 grace period 용.
    let _ = STARTUP_TIME.set(Instant::now());

    // 크로싱 검출용 이전 X 좌표를 현재 커서 위치로 초기화.
    // 이렇게 안 하면 사용자가 화면 끝까지 한 이벤트에 도달 시 그게 baseline 으로 흡수돼서
    // 이후 어떤 이동에도 트리거 안 되는 UX 후퇴가 발생.
    if let Some(pt) = cursor::get_pos() {
        PREV_EDGE_X.store(pt.x, Ordering::Release);
        tracing::debug!("master: crossing 검출 baseline 초기화 x={}", pt.x);
    }

    unsafe {
        let hwnd = create_message_window()?;
        master::set_main_hwnd(hwnd);
        register_raw_input(hwnd)?;
        register_hotkeys(hwnd, &cfg);
        if let Err(e) = crate::tray::add(hwnd, "cursorlink (master)") {
            tracing::warn!("master: 트레이 아이콘 실패, 계속 진행: {}", e);
        }
        // 저수준 훅은 startup 에 설치하지 않음. Local 상태에선 훅 없이 Raw Input 만.
        // 게임 anti-cheat 이 훅 감지해서 방어 반응하는 것을 방지.
        // 훅은 Remote / Mirror 진입 시 install, Local 복귀 시 uninstall (master.rs).
        tracing::info!("master: Raw Input 등록 완료");
        message_loop();
        hooks::uninstall();
    }
    Ok(())
}

/// RegisterHotKey (Local / Mirror 용) + 같은 키를 훅 테이블에도 등록 (Remote 용).
unsafe fn register_hotkeys(hwnd: HWND, cfg: &Config) {
    let list = [
        (HOTKEY_ID_TOGGLE,        HookAction::Toggle, "toggle",            &cfg.hotkey_toggle),
        (HOTKEY_ID_MIRROR,        HookAction::Mirror, "mirror",            &cfg.hotkey_mirror),
        (HOTKEY_ID_TRANSFER,      HookAction::Right,  "→ 오른쪽",          &cfg.hotkey_transfer),
        (HOTKEY_ID_TRANSFER_LEFT, HookAction::Left,   "→ 왼쪽",            &cfg.hotkey_transfer_left),
        (HOTKEY_ID_EDGE,          HookAction::Edge,   "쓸어넘기기 양쪽 on/off", &cfg.hotkey_edge_toggle),
        (HOTKEY_ID_EDGE_LEFT,     HookAction::EdgeLeft,  "쓸어넘기기 왼쪽 on/off",   &cfg.hotkey_edge_left),
        (HOTKEY_ID_EDGE_RIGHT,    HookAction::EdgeRight, "쓸어넘기기 오른쪽 on/off", &cfg.hotkey_edge_right),
    ];
    for (id, action, name, spec) in list {
        if let Err(e) = hotkey::register_optional(hwnd, id, name, spec) {
            tracing::warn!("master: {} 단축키 등록 실패, 계속 진행: {}", name, e);
        }
        hooks::set_hotkey(action, hotkey::pack_for_hook(spec));
    }
    // 아래 키들은 특정 상태에서만 의미 있음 → RegisterHotKey 안 하고 훅에서만 감지.
    // 그 상태가 아닐 땐 일반 키로 그대로 쓸 수 있음.
    //   return       : 슬레이브 쓰는 중에만 (마스터로 복귀)
    //   mirror_left/right : 미러 중에만 (그 슬레이브 미러에서 빼기/넣기)
    let hook_only = [
        (HookAction::Return,      "복귀 (슬레이브 쓰는 중)",        &cfg.hotkey_return),
        (HookAction::MirrorLeft,  "미러 왼쪽 빼기/넣기 (미러 중)",   &cfg.hotkey_mirror_left),
        (HookAction::MirrorRight, "미러 오른쪽 빼기/넣기 (미러 중)", &cfg.hotkey_mirror_right),
    ];
    for (action, name, spec) in hook_only {
        let packed = hotkey::pack_for_hook(spec);
        hooks::set_hotkey(action, packed);
        if packed != 0 {
            tracing::info!("master: {} 단축키 {} 등록 (LL 훅에서 감지)", name, spec);
        }
    }
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
                match wparam.0 as i32 {
                    HOTKEY_ID_TOGGLE        => master::toggle_enabled(),
                    HOTKEY_ID_MIRROR        => master::toggle_mirror(),
                    HOTKEY_ID_TRANSFER      => master::on_side_hotkey(Side::Right),
                    HOTKEY_ID_TRANSFER_LEFT => master::on_side_hotkey(Side::Left),
                    HOTKEY_ID_EDGE          => master::toggle_edge(),
                    HOTKEY_ID_EDGE_LEFT     => master::toggle_edge_side(Side::Left),
                    HOTKEY_ID_EDGE_RIGHT    => master::toggle_edge_side(Side::Right),
                    _ => {}
                }
                LRESULT(0)
            }
            m if m == master::WM_PEER_UP => {
                master::on_peer_up(Side::from_u8(wparam.0 as u8));
                LRESULT(0)
            }
            m if m == master::WM_PEER_DOWN => {
                master::on_peer_down(Side::from_u8(wparam.0 as u8));
                LRESULT(0)
            }
            m if m == master::WM_PEER_RETURN => {
                master::on_peer_return(Side::from_u8(wparam.0 as u8));
                LRESULT(0)
            }
            m if m == master::WM_SHOW_NOTE => {
                master::show_pending_note();
                LRESULT(0)
            }
            m if m == crate::tray::WM_TRAY => {
                let event = crate::tray::tray_event_from_lparam(lparam);
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    let enabled = master::SHARED.enabled.load(Ordering::Acquire);
                    crate::tray::show_context_menu(hwnd, enabled, &master::tray_items());
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match crate::tray::menu_id_from_wparam(wparam) {
                    crate::tray::MENU_TOGGLE => master::toggle_enabled(),
                    crate::tray::MENU_EDGE_LEFT => master::toggle_edge_side(Side::Left),
                    crate::tray::MENU_EDGE_RIGHT => master::toggle_edge_side(Side::Right),
                    crate::tray::MENU_MIRROR_LEFT => master::toggle_mirror_target(Side::Left),
                    crate::tray::MENU_MIRROR_RIGHT => master::toggle_mirror_target(Side::Right),
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
    if !enabled { return; }

    let dx = m.lLastX;
    let dy = m.lLastY;
    // windows-rs 0.58: usFlags 는 MOUSE_STATE(u16), MOUSE_MOVE_ABSOLUTE 도 동일 wrapper.
    let is_relative = (m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) == 0;

    // usButtonFlags: u16 (RI_MOUSE_BUTTON_* 비트 or), usButtonData: u16 (실은 signed short)
    let btn_flags: u16 = m.Anonymous.Anonymous.usButtonFlags;
    let button_data: i16 = m.Anonymous.Anonymous.usButtonData as i16;

    match state {
        MasterState::Local => {
            // 엣지 감지 (안쪽 → 화면 끝 크로싱 시에만 트리거). 미러/쓸어넘기기 off 여도
            // 기준점은 계속 갱신해야 해서 이동이 있으면 항상 호출.
            if is_relative && (dx != 0 || dy != 0) {
                check_edge_and_transfer();
            }
            if master::SHARED.mirror.load(Ordering::Acquire) {
                // Mirror 모드: 현재 커서 위치를 % 로 Slave 에 전송 (커서 락 X).
                // 이동만 있든, 버튼/휠만 있든, 이벤트 발생 시 반드시 위치 sync 먼저 → 그 다음 액션.
                // (Raw Input 이 버튼/휠을 dx=dy=0 인 별도 이벤트로 전달하므로 델타 체크만으론 부족.)
                let has_motion = is_relative && (dx != 0 || dy != 0);
                let has_action = btn_flags != 0;
                if has_motion || has_action {
                    send_mouse_pos_from_current();
                }
                forward_buttons_and_wheel(btn_flags, button_data);
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

/// Local 상태에서 커서가 화면 끝을 크로싱 (안쪽에서 → 끝으로 진입) 하면 그 쪽 슬레이브로 전환.
///   오른쪽 끝 → 오른쪽 슬레이브, 왼쪽 끝 → 왼쪽 슬레이브.
/// 크로싱 검출: 이전 X 좌표는 끝 안쪽이었고 지금 X 가 끝이면 트리거.
/// 이렇게 하면 커서가 처음부터 끝에 앉아있는 상황에서 아무 이동에도 튀는 문제 해결.
unsafe fn check_edge_and_transfer() {
    let pt = match cursor::get_pos() { Some(p) => p, None => return };

    // 이전 X 좌표를 스왑해서 가져옴 (동시에 새 값으로 갱신).
    // 미러 / 쓸어넘기기 off / grace 중에도 기준점은 갱신 → 다시 켤 때 낡은 값으로 오작동 방지.
    let prev = PREV_EDGE_X.swap(pt.x, Ordering::AcqRel);

    // 첫 호출은 baseline 설정만 하고 트리거 안 함.
    if prev == i32::MIN { return; }

    // Mirror 상태면 Remote 로 안 넘어감.
    if master::SHARED.mirror.load(Ordering::Acquire) { return; }

    // Startup grace period: 재시작 시 커서가 끝 근처에 있으면 첫 이동에 실수 트리거되는 것 방지.
    if let Some(start) = STARTUP_TIME.get() {
        if start.elapsed().as_millis() < EDGE_GRACE_MS {
            return;
        }
    }

    let scr = cursor::primary_screen();
    let right_edge = scr.right - 1;
    let left_edge = scr.left;

    // 커서가 이미 끝에 앉아있는 상태에서 끝 방향으로 클램프된 이벤트가 계속 와도 무시.
    let side = if prev < right_edge && pt.x >= right_edge {
        Side::Right
    } else if prev > left_edge && pt.x <= left_edge {
        Side::Left
    } else {
        return;
    };

    // 그 쪽 쓸어넘기기가 꺼져 있으면 단축키로만 전환.
    if !master::SHARED.is_edge_enabled(side) { return; }

    let entry_y_pct = if scr.height() > 0 {
        (((pt.y - scr.top) as i64 * 100) / scr.height() as i64).clamp(0, 100) as u8
    } else { 50 };
    master::transfer_by_edge(side, entry_y_pct);
}

unsafe fn handle_keyboard(k: &RAWKEYBOARD) {
    let state = master::SHARED.get();
    let enabled = master::SHARED.enabled.load(Ordering::Acquire);
    // Remote 상태에서만 키 전달. Local 이면 이 PC 가 정상 처리.
    if !enabled || state != MasterState::Remote { return; }
    // config 로 키보드 전송 끌 수 있음. false 면 Slave 는 자체 키보드로 조작.
    if !hooks::FORWARD_KEYBOARD.load(Ordering::Acquire) { return; }

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
    match EPOCH.get() {
        Some(e) => Instant::now().duration_since(*e).as_micros() as u32,
        None => 0,
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

/// Mirror 면 미러 대상 전부, Remote 면 제어 중인 슬레이브로.
fn send_packet(p: Packet) {
    master::route_packet(&p.encode());
}

fn send_move(dx: i16, dy: i16) {
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseMove, flags: 0, button: 0,
        dx, dy, wheel_dx: 0, wheel_dy: 0,
    });
}

fn button_packet(button: u16, down: bool) -> Packet {
    Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseButton,
        flags: if down { FLAG_BTN_DOWN } else { 0 },
        button, dx: 0, dy: 0, wheel_dx: 0, wheel_dy: 0,
    }
}

fn send_button(button: u16, down: bool) {
    let bit = 1u32 << button;
    if down {
        HELD_BTNS.fetch_or(bit, Ordering::AcqRel);
    } else {
        HELD_BTNS.fetch_and(!bit, Ordering::AcqRel);
    }
    send_packet(button_packet(button, down));
}

fn send_wheel(hx: i16, vy: i16) {
    send_packet(Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::MouseWheel, flags: 0, button: 0,
        dx: 0, dy: 0, wheel_dx: hx, wheel_dy: vy,
    });
}

fn key_packet(scan: u16, down: bool, ext: bool) -> Packet {
    let mut flags = 0u8;
    if down { flags |= FLAG_BTN_DOWN; }
    if ext  { flags |= FLAG_KEY_EXT; }
    Packet {
        seq: next_seq(), ts_us: now_us(),
        kind: Kind::KeyEvent, flags,
        button: scan,
        dx: 0, dy: 0, wheel_dx: 0, wheel_dy: 0,
    }
}

pub fn send_key(scan: u16, down: bool, ext: bool) {
    let idx = (scan & 0xFF) as usize + if ext { 256 } else { 0 };
    let mask = 1u64 << (idx % 64);
    if down {
        HELD_KEYS[idx / 64].fetch_or(mask, Ordering::AcqRel);
    } else {
        HELD_KEYS[idx / 64].fetch_and(!mask, Ordering::AcqRel);
    }
    send_packet(key_packet(scan, down, ext));
}

/// 그 슬레이브에 눌린 채 남은 키/버튼을 전부 떼는 패킷 전송.
/// 기록은 지우지 않음 (실제 키를 떼면 그때 지워짐) → 미러 대상이 여럿이어도 안전.
pub fn release_held(side: Side) {
    for (w, word) in HELD_KEYS.iter().enumerate() {
        let mut bits = word.load(Ordering::Acquire);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let idx = w * 64 + b;
            let p = key_packet((idx & 0xFF) as u16, false, idx >= 256);
            master::send_udp(side, &p.encode());
        }
    }
    let btns = HELD_BTNS.load(Ordering::Acquire);
    for b in [MB_LEFT, MB_RIGHT, MB_MIDDLE, MB_X1, MB_X2] {
        if btns & (1 << b) != 0 {
            master::send_udp(side, &button_packet(b, false).encode());
        }
    }
}

/// Local 로 완전히 돌아왔을 때 (더 이상 forward 안 함) 기록 초기화.
pub fn clear_held() {
    for word in &HELD_KEYS {
        word.store(0, Ordering::Release);
    }
    HELD_BTNS.store(0, Ordering::Release);
}
