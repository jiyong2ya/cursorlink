// 전역 단축키 + Slave 용 트레이 hidden window.
//
// 두 가지 사용:
//   1) 이미 있는 hidden window 에 hotkey 만 등록 (master 는 capture.rs 의 창을 그대로 사용)
//      → register_toggle / register_optional
//   2) 자체 hidden window + 스레드 필요 (slave 는 UDP 루프에 갇혀 있음)
//      → spawn_window_thread(spec, tooltip, on_toggle, get_enabled)
//        새 스레드가 hidden window 하나 만들어서 hotkey + tray 를 모두 처리.
//
// Master 가 Remote 인 동안엔 LL 훅이 키를 소비해서 RegisterHotKey 가 안 불림.
// 그래서 같은 단축키를 pack_for_hook 으로 압축해 hooks.rs 에도 넘겨서 훅 안에서 매칭.

#![cfg(windows)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const HOTKEY_ID_TOGGLE: i32 = 1;
pub const HOTKEY_ID_MIRROR: i32 = 2;
/// 오른쪽 슬레이브로 전환 (미러 중엔 오른쪽 미러 대상 넣기/빼기)
pub const HOTKEY_ID_TRANSFER: i32 = 3;
/// 왼쪽 슬레이브로 전환 (미러 중엔 왼쪽 미러 대상 넣기/빼기)
pub const HOTKEY_ID_TRANSFER_LEFT: i32 = 4;
/// 쓸어넘기기 양쪽 on/off
pub const HOTKEY_ID_EDGE: i32 = 5;
/// 왼쪽 쓸어넘기기만 on/off
pub const HOTKEY_ID_EDGE_LEFT: i32 = 6;
/// 오른쪽 쓸어넘기기만 on/off
pub const HOTKEY_ID_EDGE_RIGHT: i32 = 7;

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
    tracing::info!("전역 단축키 등록 (toggle): {}", spec.toggle);
    Ok(())
}

/// 선택적 단축키 등록. 빈 문자열이면 아무것도 안 함.
pub fn register_optional(hwnd: HWND, id: i32, name: &str, spec: &str) -> Result<()> {
    if spec.trim().is_empty() {
        return Ok(());
    }
    let (mods, vk) = parse_hotkey(spec)?;
    unsafe {
        RegisterHotKey(hwnd, id, mods, vk as u32)
            .map_err(|e| anyhow::anyhow!("RegisterHotKey {} 실패 ({}): {}", name, spec, e))?;
    }
    tracing::info!("전역 단축키 등록 ({}): {}", name, spec);
    Ok(())
}

/// 훅에서 매칭할 수 있게 (modifiers << 16) | vk 로 압축.
/// modifiers 는 RegisterHotKey 와 같은 MOD_* 비트 (alt=1, ctrl=2, shift=4, win=8).
/// 빈 문자열이거나 파싱 실패면 0 (= 미설정).
pub fn pack_for_hook(spec: &str) -> u32 {
    if spec.trim().is_empty() {
        return 0;
    }
    match parse_hotkey(spec) {
        Ok((mods, vk)) => ((mods.0 & 0xF) << 16) | vk as u32,
        Err(e) => {
            tracing::warn!("단축키 파싱 실패 ({}): {}", spec, e);
            0
        }
    }
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
    use windows::Win32::Foundation::HINSTANCE;
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
                crate::tray::show_context_menu(hwnd, enabled, &[]);
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
        // 넘버패드 키
        "num0" | "numpad0" => VK_NUMPAD0.0,
        "num1" | "numpad1" => VK_NUMPAD1.0,
        "num2" | "numpad2" => VK_NUMPAD2.0,
        "num3" | "numpad3" => VK_NUMPAD3.0,
        "num4" | "numpad4" => VK_NUMPAD4.0,
        "num5" | "numpad5" => VK_NUMPAD5.0,
        "num6" | "numpad6" => VK_NUMPAD6.0,
        "num7" | "numpad7" => VK_NUMPAD7.0,
        "num8" | "numpad8" => VK_NUMPAD8.0,
        "num9" | "numpad9" => VK_NUMPAD9.0,
        "num*" | "nummul" | "multiply" | "numpad_multiply" => VK_MULTIPLY.0,
        "num-" | "numsub" | "subtract" | "numpad_subtract" => VK_SUBTRACT.0,
        "num/" | "numdiv" | "divide"   | "numpad_divide"   => VK_DIVIDE.0,
        "num." | "numdot" | "decimal"  | "numpad_decimal"  => VK_DECIMAL.0,
        // "num+" 는 '+' 가 구분자라 파싱 불가 → numplus 로 씀
        "numplus" | "add"      | "numpad_add"      => VK_ADD.0,
        _ => return Err(anyhow::anyhow!("알 수 없는 키: {}", s)),
    };
    Ok(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_numpad_keys() {
        assert_eq!(pack_for_hook("num/"), VK_DIVIDE.0 as u32);
        assert_eq!(pack_for_hook("numplus"), VK_ADD.0 as u32);
        assert_eq!(pack_for_hook(""), 0);
        assert_eq!(pack_for_hook("num+"), 0); // '+' 구분자라 파싱 실패 → 미설정
    }

    #[test]
    fn pack_with_modifiers() {
        // ctrl=2, shift=4 → 6 << 16
        assert_eq!(pack_for_hook("ctrl+shift+k"), (6 << 16) | b'K' as u32);
        // 쓸어넘기기 한쪽 토글 예시: num/ (왼쪽 전환) 과 다른 키로 구분돼야 함
        assert_eq!(pack_for_hook("ctrl+num/"), (2 << 16) | VK_DIVIDE.0 as u32);
        assert_ne!(pack_for_hook("ctrl+num/"), pack_for_hook("num/"));
    }
}
