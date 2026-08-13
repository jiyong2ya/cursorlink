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
use std::sync::atomic::{AtomicBool, AtomicU32};

static mut G_MOUSE_HOOK: Option<HHOOK> = None;
static mut G_KEYBOARD_HOOK: Option<HHOOK> = None;

/// 키보드를 Slave 로 forward 할지. capture.rs 에서 config 값으로 설정.
pub static FORWARD_KEYBOARD: AtomicBool = AtomicBool::new(true);

/// Remote 상태에서 이 VK 를 감지하면 Slave 로 forward 안 하고 Master 로 복귀.
/// 0 = 미설정 (기능 비활성).
pub static RETURN_VK: AtomicU32 = AtomicU32::new(0);

pub unsafe fn install() -> anyhow::Result<()> {
    // Idempotent: 이미 설치된 상태면 skip.
    if G_MOUSE_HOOK.is_some() && G_KEYBOARD_HOOK.is_some() {
        return Ok(());
    }
    if G_MOUSE_HOOK.is_none() {
        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), HINSTANCE::default(), 0)
            .map_err(|e| anyhow::anyhow!("WH_MOUSE_LL 등록 실패: {:?}", e))?;
        G_MOUSE_HOOK = Some(mh);
    }
    if G_KEYBOARD_HOOK.is_none() {
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), HINSTANCE::default(), 0)
            .map_err(|e| anyhow::anyhow!("WH_KEYBOARD_LL 등록 실패: {:?}", e))?;
        G_KEYBOARD_HOOK = Some(kh);
    }
    tracing::info!("master: 저수준 훅 install");
    Ok(())
}

pub unsafe fn uninstall() {
    let mut any = false;
    if let Some(h) = G_MOUSE_HOOK.take() {
        let _ = UnhookWindowsHookEx(h);
        any = true;
    }
    if let Some(h) = G_KEYBOARD_HOOK.take() {
        let _ = UnhookWindowsHookEx(h);
        any = true;
    }
    if any {
        tracing::info!("master: 저수준 훅 uninstall");
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
    if n_code >= 0 {
        unsafe {
            let kbd = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let scan = kbd.scanCode as u16;
            let flags = kbd.flags.0;
            let is_up = (flags & LLKHF_UP) != 0;
            let is_ext = (flags & LLKHF_EXTENDED) != 0;
            let vk = kbd.vkCode;

            // Remote 상태에서 RETURN_VK 감지 시 Slave 로 forward 안 하고 Master 복귀.
            // Key-down 에서만 트리거 (key-up 은 조용히 소비).
            let return_vk = RETURN_VK.load(Ordering::Acquire);
            if return_vk != 0 && vk == return_vk && is_remote_active() && !is_mirror_active() {
                if !is_up {
                    tracing::info!("master: return hotkey (vk={:#x}) 감지 → Local 복귀", vk);
                    crate::master::force_return_to_local();
                }
                return LRESULT(1); // 소비 (Slave 로 안 감)
            }

            // 일반 키보드 forward: Remote 또는 Mirror 에서 forward_keyboard 켜져있으면.
            if FORWARD_KEYBOARD.load(Ordering::Acquire) {
                let should_forward = is_remote_active() || is_mirror_active();
                if should_forward && scan != 0 {
                    crate::input::capture::send_key(scan, !is_up, is_ext);
                }
                // Remote 만 소비, Mirror 는 통과.
                if is_remote_active() && !is_mirror_active() {
                    return LRESULT(1);
                }
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
