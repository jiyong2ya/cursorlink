// Master 저수준 입력 훅.
//
// 목적:
//  1. Remote 상태일 때 Master의 물리 입력이 Master 자신의 앱/OS로 흘러들어가지 않도록
//     소비 (consume). Raw Input 만으로는 관찰만 가능하고 이벤트가 그대로 Master의
//     활성 창까지 전달되어 이중 입력이 발생함.
//  2. Remote 중 단축키 감지. 훅이 키를 소비하면 RegisterHotKey 가 안 불리므로
//     (복귀 / 왼쪽·오른쪽 전환 / 쓸어넘기기 양쪽·왼쪽·오른쪽 / toggle) 를 여기서 직접 매칭.
//  3. Mirror 중 키보드를 Slave 로 forward (소비는 안 함).
//
// 소비 조건: enabled && state == Remote (Mirror 아님)
// 그 외: CallNextHookEx 로 정상 전달.
//
// 훅 프로시저는 hook을 설치한 스레드의 메시지 루프 안에서 호출됨.
// Master 는 capture.rs 에서 메인 스레드가 메시지 루프를 돌리고, install/uninstall 도
// 메인 스레드에서만 함 (master.rs 의 상태 전환이 전부 메인 스레드).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx,
    HHOOK, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
};

// KBDLLHOOKSTRUCT.flags 비트마스크 (winuser.h)
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_UP: u32 = 0x80;

use crate::master;
use crate::state::MasterState;

static mut G_MOUSE_HOOK: Option<HHOOK> = None;
static mut G_KEYBOARD_HOOK: Option<HHOOK> = None;

/// 키보드를 Slave 로 forward 할지. capture.rs 에서 config 값으로 설정.
pub static FORWARD_KEYBOARD: AtomicBool = AtomicBool::new(true);

/// 훅이 아는 단축키 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    Toggle    = 0,
    Mirror    = 1,
    Left      = 2,
    Right     = 3,
    /// 쓸어넘기기 양쪽 on/off
    Edge      = 4,
    Return    = 5,
    EdgeLeft  = 6,
    EdgeRight = 7,
}

const ACTIONS: [HookAction; 8] = [
    HookAction::Toggle, HookAction::Mirror, HookAction::Left, HookAction::Right,
    HookAction::Edge, HookAction::Return, HookAction::EdgeLeft, HookAction::EdgeRight,
];

/// 단축키 테이블 (HookAction as usize 로 인덱싱).
/// 값 = (modifiers << 16) | vk  (hotkey::pack_for_hook), 0 = 미설정.
static HOTKEYS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

/// Remote 중 단축키로 처리한 키. 오토리피트/떼기를 조용히 소비하기 위해 기억.
static SWALLOW_VK: AtomicU32 = AtomicU32::new(0);

/// 훅이 본 modifier 눌림 상태 (L/R 구분 비트). Remote 중엔 훅이 키를 소비해서
/// GetAsyncKeyState 가 갱신 안 되므로 직접 추적.
static MOD_KEYS: AtomicU32 = AtomicU32::new(0);

/// (vk, MOD_KEYS 비트): LCtrl RCtrl LAlt RAlt LShift RShift LWin RWin
const MOD_VKS: [(u32, u32); 8] = [
    (0xA2, 0), (0xA3, 1),
    (0xA4, 2), (0xA5, 3),
    (0xA0, 4), (0xA1, 5),
    (0x5B, 6), (0x5C, 7),
];

pub fn set_hotkey(action: HookAction, packed: u32) {
    HOTKEYS[action as usize].store(packed, Ordering::Release);
}

fn find_hotkey(vk: u32, mods: u32) -> Option<HookAction> {
    let key = (mods << 16) | vk;
    ACTIONS.into_iter().find(|a| {
        let v = HOTKEYS[*a as usize].load(Ordering::Acquire);
        v != 0 && v == key
    })
}

/// Mirror 중 Slave 로 안 보낸 단축키. 떼짐도 같이 안 보내려고 기억.
static MIRROR_SKIP_VK: AtomicU32 = AtomicU32::new(0);

/// Mirror 중 이 키 이벤트를 Slave 로 보내지 말아야 하는지.
/// RegisterHotKey 로 Master 가 처리하는 단축키 (Return 제외 — 등록 안 된 일반 키) 만 해당.
fn mirror_should_skip(vk: u32, is_up: bool) -> bool {
    if is_up {
        if vk == MIRROR_SKIP_VK.load(Ordering::Acquire) {
            MIRROR_SKIP_VK.store(0, Ordering::Release);
            return true;
        }
        return false;
    }
    match find_hotkey(vk, current_mods()) {
        Some(a) if a != HookAction::Return => {
            MIRROR_SKIP_VK.store(vk, Ordering::Release);
            true
        }
        _ => false,
    }
}

fn track_modifier(vk: u32, is_up: bool) {
    if let Some(&(_, bit)) = MOD_VKS.iter().find(|(v, _)| *v == vk) {
        if is_up {
            MOD_KEYS.fetch_and(!(1 << bit), Ordering::AcqRel);
        } else {
            MOD_KEYS.fetch_or(1 << bit, Ordering::AcqRel);
        }
    }
}

/// RegisterHotKey 와 같은 MOD_* 비트 (alt=1, ctrl=2, shift=4, win=8).
fn current_mods() -> u32 {
    let k = MOD_KEYS.load(Ordering::Acquire);
    let mut m = 0;
    if k & 0b0000_0011 != 0 { m |= 2; } // ctrl
    if k & 0b0000_1100 != 0 { m |= 1; } // alt
    if k & 0b0011_0000 != 0 { m |= 4; } // shift
    if k & 0b1100_0000 != 0 { m |= 8; } // win
    m
}

/// install 시점의 실제 modifier 상태로 초기화 (Local 땐 훅이 없어서 놓친 눌림/떼짐 보정).
unsafe fn init_modifiers() {
    let mut k = 0;
    for (vk, bit) in MOD_VKS {
        if (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 {
            k |= 1 << bit;
        }
    }
    MOD_KEYS.store(k, Ordering::Release);
}

pub unsafe fn install() -> anyhow::Result<()> {
    // Idempotent: 이미 설치된 상태면 skip.
    if G_MOUSE_HOOK.is_some() && G_KEYBOARD_HOOK.is_some() {
        return Ok(());
    }
    init_modifiers();
    SWALLOW_VK.store(0, Ordering::Release);
    MIRROR_SKIP_VK.store(0, Ordering::Release);
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
            track_modifier(vk, is_up);

            let mirror = is_mirror_active();
            let remote = is_remote_active() && !mirror;

            // Remote 중 단축키: Slave 로 안 보내고 여기서 처리 + 소비.
            // 눌림에서 한 번만 실행하고, 오토리피트/떼기는 조용히 소비.
            // (Mirror 단축키는 Remote 중엔 의미 없으므로 일반 키처럼 Slave 로 forward.)
            if remote {
                if vk == SWALLOW_VK.load(Ordering::Acquire) {
                    if is_up { SWALLOW_VK.store(0, Ordering::Release); }
                    return LRESULT(1);
                }
                if !is_up {
                    if let Some(action) = find_hotkey(vk, current_mods()) {
                        if action != HookAction::Mirror {
                            tracing::info!("master: 단축키 {:?} (vk={:#x}) 감지", action, vk);
                            SWALLOW_VK.store(vk, Ordering::Release);
                            master::on_hook_hotkey(action);
                            return LRESULT(1);
                        }
                    }
                }
            }

            // 일반 키보드 forward: Remote 또는 Mirror 에서 forward_keyboard 켜져있으면.
            if FORWARD_KEYBOARD.load(Ordering::Acquire) {
                // Mirror 중엔 단축키 (num/ num- num* 등) 를 Slave 로 안 보냄.
                // 통과시켜서 RegisterHotKey 로 Master 가 처리.
                let skip = mirror && mirror_should_skip(vk, is_up);
                if (remote || mirror) && scan != 0 && !skip {
                    crate::input::capture::send_key(scan, !is_up, is_ext);
                }
                // Remote 만 소비, Mirror 는 통과.
                if remote {
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
