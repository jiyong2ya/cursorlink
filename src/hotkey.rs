// 전역 단축키 + Slave 용 트레이 hidden window.
//
// 두 가지 사용:
//   1) 이미 있는 hidden window 에 hotkey 만 등록 (master 는 capture.rs 의 창을 그대로 사용)
//      → register_toggle(hwnd, spec)
//   2) 자체 hidden window + 스레드 필요 (slave 는 UDP 루프에 갇혀 있음)
//      → spawn_window_thread(spec, tooltip, on_toggle, get_enabled)
//        새 스레드가 hidden window 하나 만들어서 hotkey + tray 를 모두 처리.

#![cfg(windows)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const HOTKEY_ID_TOGGLE: i32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeySpec {
    #[serde(default = "default_hotkey")]
    pub toggle: String,
}

impl Default for HotkeySpec {
    fn default() -> Self { Self { toggle: default_hotkey() } }
}

fn default_hotkey() -> String { "ctrl+alt+shift+k".to_string() }

pub fn register_toggle(hwnd: HWND, spec: &HotkeySpec) -> Result<()> {
    let (mods, vk) = parse_hotkey(&spec.toggle)?;
    unsafe {
        RegisterHotKey(hwnd, HOTKEY_ID_TOGGLE, mods, vk as u32)
            .map_err(|e| anyhow::anyhow!("RegisterHotKey 실패 ({}): {}", spec.toggle, e))?;
    }
    tracing::info!("전역 단축키 등록: {}", spec.toggle);
    Ok(())
}

pub fn unregister_toggle(hwnd: HWND) {
    unsafe { let _ = UnregisterHotKey(hwnd, HOTKEY_ID_TOGGLE); }
}

// 콜백 저장: 새 스레드에서 wnd_proc 이 참조.
thread_local! {
    static ON_TOGGLE:  RefCell<Option<Box<dyn Fn()>>>        = const { RefCell::new(None) };
    static GET_ENABLED: RefCell<Option<Box<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

/// slave 등 자체 message loop 이 없는 컴포넌트용.
/// 백그라운드 스레드가 hidden window 를 만들고 hotkey + tray 이벤트를 처리.
pub fn spawn_window_thread(
    spec: HotkeySpec,
    tooltip: String,
    on_toggle: impl Fn() + Send + 'static,
    get_enabled: impl Fn() -> bool + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("cursorlink-tray".to_string())
        .spawn(move || {
            let cb_t: Box<dyn Fn()> = Box::new(on_toggle);
            let cb_e: Box<dyn Fn() -> bool> = Box::new(get_enabled);
            ON_TOGGLE.with(|s|  *s.borrow_mut() = Some(cb_t));
            GET_ENABLED.with(|s| *s.borrow_mut() = Some(cb_e));

            unsafe {
                if let Err(e) = run_window_message_loop(&spec, &tooltip) {
                    tracing::warn!("hotkey/tray window 오류: {}", e);
                }
            }
        })?;
    Ok(handle)
}

unsafe fn run_window_message_loop(spec: &HotkeySpec, tooltip: &str) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::HBRUSH;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    let hmod = GetModuleHandleW(PCWSTR::null())
        .map_err(|e| anyhow::anyhow!("GetModuleHandleW: {}", e))?;
    let hinst = HINSTANCE(hmod.0);
    let class_name: Vec<u16> = "cursorlink_tray\0".encode_utf16().collect();

    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(hk_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
    };
    if RegisterClassW(&wc) == 0 {
        anyhow::bail!("RegisterClassW tray");
    }

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
    ).map_err(|e| anyhow::anyhow!("CreateWindowExW tray: {}", e))?;

    register_toggle(hwnd, spec)?;
    if let Err(e) = crate::tray::add(hwnd, tooltip) {
        tracing::warn!("tray 등록 실패: {}", e);
    }

    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
        if r.0 <= 0 { break; }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    Ok(())
}

extern "system" fn hk_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        // WM_TRAY 는 상수라 match 안에 못 넣음 → if 로.
        if msg == crate::tray::WM_TRAY {
            let event = crate::tray::tray_event_from_lparam(lparam);
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                let enabled = GET_ENABLED.with(|s| {
                    s.borrow().as_ref().map(|f| f()).unwrap_or(true)
                });
                crate::tray::show_context_menu(hwnd, enabled);
            }
            return LRESULT(0);
        }

        match msg {
            WM_HOTKEY => {
                if wparam.0 as i32 == HOTKEY_ID_TOGGLE {
                    ON_TOGGLE.with(|s| {
                        if let Some(cb) = s.borrow().as_ref() { cb(); }
                    });
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match crate::tray::menu_id_from_wparam(wparam) {
                    crate::tray::MENU_TOGGLE => {
                        ON_TOGGLE.with(|s| {
                            if let Some(cb) = s.borrow().as_ref() { cb(); }
                        });
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

fn parse_hotkey(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u16)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk: Option<u16> = None;

    for part in spec.split('+').map(str::trim).map(str::to_ascii_lowercase) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt"              => mods |= MOD_ALT,
            "shift"            => mods |= MOD_SHIFT,
            "win" | "meta" | "super" => mods |= MOD_WIN,
            s => vk = Some(parse_vk(s)?),
        }
    }
    let vk = vk.ok_or_else(|| anyhow::anyhow!("단축키에 키가 없습니다: {}", spec))?;
    Ok((mods, vk))
}

fn parse_vk(s: &str) -> Result<u16> {
    let bytes = s.as_bytes();
    if bytes.len() == 1 {
        let b = bytes[0].to_ascii_uppercase();
        if (b'A'..=b'Z').contains(&b) || (b'0'..=b'9').contains(&b) {
            return Ok(b as u16);
        }
    }
    let vk = match s {
        "space"    => VK_SPACE.0,
        "enter" | "return" => VK_RETURN.0,
        "tab"      => VK_TAB.0,
        "esc" | "escape" => VK_ESCAPE.0,
        "f1"       => VK_F1.0,  "f2" => VK_F2.0,  "f3" => VK_F3.0,
        "f4"       => VK_F4.0,  "f5" => VK_F5.0,  "f6" => VK_F6.0,
        "f7"       => VK_F7.0,  "f8" => VK_F8.0,  "f9" => VK_F9.0,
        "f10"      => VK_F10.0, "f11" => VK_F11.0, "f12" => VK_F12.0,
        "home"     => VK_HOME.0,
        "end"      => VK_END.0,
        "pageup" | "pgup"   => VK_PRIOR.0,
        "pagedown" | "pgdn" => VK_NEXT.0,
        "insert" | "ins"    => VK_INSERT.0,
        "delete" | "del"    => VK_DELETE.0,
        "left"     => VK_LEFT.0,
        "right"    => VK_RIGHT.0,
        "up"       => VK_UP.0,
        "down"     => VK_DOWN.0,
        _ => return Err(anyhow::anyhow!("알 수 없는 키: {}", s)),
    };
    Ok(vk)
}
