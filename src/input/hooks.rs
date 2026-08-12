// Master 저수준 입력 훅.
//
// 목적: Remote 상태일 때 Master의 물리 입력이 Master 자신의 앱/OS로
// 흘러들어가지 않도록 소비 (consume). Raw Input 만으로는 관찰만 가능하고
// 이벤트가 그대로 Master의 활성 창까지 전달되어 이중 입력이 발생함.
//
// 소비 조건: enabled && state == Remote
// 그 외: CallNextHookEx 로 정상 전달.
//
// 훅 프로시저는 hook을 설치한 스레드의 메시지 루프 안에서 호출됨.
// Master 는 capture.rs 에서 메인 스레드가 메시지 루프를 돌리므로 여기서 설치.

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx,
    HHOOK, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
};

// KBDLLHOOKSTRUCT.flags 비트마스크 (winuser.h)
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_UP: u32 = 0x80;

use crate::master;
use crate::state::MasterState;
use std::sync::atomic::AtomicBool;

static mut G_MOUSE_HOOK: Option<HHOOK> = None;
static mut G_KEYBOARD_HOOK: Option<HHOOK> = None;

/// 키보드를 Slave 로 forward 할지. capture.rs 에서 config 값으로 설정.
/// false 면 Remote 상태여도 키보드 훅은 소비 안 함 (Master 에서 정상 처리).
pub static FORWARD_KEYBOARD: AtomicBool = AtomicBool::new(true);

pub unsafe fn install() -> anyhow::Result<()> {
    let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), HINSTANCE::default(), 0)
        .map_err(|e| anyhow::anyhow!("WH_MOUSE_LL 등록 실패: {:?}", e))?;
    G_MOUSE_HOOK = Some(mh);

    let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), HINSTANCE::default(), 0)
        .map_err(|e| anyhow::anyhow!("WH_KEYBOARD_LL 등록 실패: {:?}", e))?;
    G_KEYBOARD_HOOK = Some(kh);

    tracing::info!("master: 저수준 마우스/키보드 훅 등록 완료");
    Ok(())
}

pub unsafe fn uninstall() {
    if let Some(h) = G_MOUSE_HOOK.take() {
        let _ = UnhookWindowsHookEx(h);
    }
    if let Some(h) = G_KEYBOARD_HOOK.take() {
        let _ = UnhookWindowsHookEx(h);
    }
}

extern "system" fn mouse_hook_proc(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Remote: 소비 (Master 앱에 입력 안 감).
    // Mirror: 소비 안 함 (Master 도 정상 처리 + Slave forward).
    if n_code >= 0 && is_remote_active() && !is_mirror_active() {
        return LRESULT(1);
    }
    unsafe {
        if let Some(h) = G_MOUSE_HOOK {
            return CallNextHookEx(h, n_code, wparam, lparam);
        }
    }
    LRESULT(0)
}

extern "system" fn keyboard_hook_proc(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Windows 특성상 WH_KEYBOARD_LL 소비 시 Raw Input 도 못 받음.
    // 따라서 훅 안에서 직접 UDP forward 후 소비.
    // Remote: forward + 소비 (Master 앱에 입력 안 감)
    // Mirror: forward + 소비 안 함 (Master 도 정상 처리 + Slave 도 받음)
    if n_code >= 0 && FORWARD_KEYBOARD.load(Ordering::Acquire) {
        let should_forward = is_remote_active() || is_mirror_active();
        if should_forward {
            unsafe {
                let kbd = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                let scan = kbd.scanCode as u16;
                let flags = kbd.flags.0;
                let is_up = (flags & LLKHF_UP) != 0;
                let is_ext = (flags & LLKHF_EXTENDED) != 0;
                if scan != 0 {
                    crate::input::capture::send_key(scan, !is_up, is_ext);
                }
            }
            // Mirror 는 소비 안 함, Remote 만 소비.
            if is_remote_active() && !is_mirror_active() {
                return LRESULT(1);
            }
        }
    }
    unsafe {
        if let Some(h) = G_KEYBOARD_HOOK {
            return CallNextHookEx(h, n_code, wparam, lparam);
        }
    }
    LRESULT(0)
}

fn is_remote_active() -> bool {
    if !master::SHARED.enabled.load(Ordering::Acquire) {
        return false;
    }
    master::SHARED.get() == MasterState::Remote
}

fn is_mirror_active() -> bool {
    if !master::SHARED.enabled.load(Ordering::Acquire) {
        return false;
    }
    master::SHARED.mirror.load(Ordering::Acquire)
}
