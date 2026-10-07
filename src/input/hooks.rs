// Master 저수준 입력 훅.
//
// 목적:
//  1. Remote 상태일 때 Master의 물리 입력이 Master 자신의 앱/OS로 흘러들어가지 않도록
//     소비 (consume). Raw Input 만으로는 관찰만 가능하고 이벤트가 그대로 Master의
//     활성 창까지 전달되어 이중 입력이 발생함.
//  2. Remote 중 단축키 감지. 훅이 키를 소비하면 RegisterHotKey 가 안 불리므로
//     (복귀 / 쓸어넘기기 양쪽·왼쪽·오른쪽 / toggle) 를 여기서 직접 매칭.
//     왼쪽/오른쪽 이동·미러 키는 슬레이브에 그냥 입력되게 둠 (handled_while_remote).
//  3. Mirror 중 키보드를 Slave 로 forward (소비는 안 함). 미러 대상 전용 키만 여기서 처리 + 소비.
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
    /// 미러 중에만: 왼쪽/오른쪽 슬레이브를 미러에서 빼기/넣기 (RegisterHotKey 안 함, 평소엔 일반 키)
    MirrorLeft  = 8,
    MirrorRight = 9,
}

const ACTIONS: [HookAction; 10] = [
    HookAction::Toggle, HookAction::Mirror, HookAction::Left, HookAction::Right,
    HookAction::Edge, HookAction::Return, HookAction::EdgeLeft, HookAction::EdgeRight,
    HookAction::MirrorLeft, HookAction::MirrorRight,
];

/// 단축키 테이블 (HookAction as usize 로 인덱싱).
/// 값 = (modifiers << 16) | vk  (hotkey::pack_for_hook), 0 = 미설정.
static HOTKEYS: [AtomicU32; 10] = [const { AtomicU32::new(0) }; 10];

/// 훅이 단축키로 처리한 키 (Remote / Mirror 중). 오토리피트/떼기를 조용히 소비하기 위해 기억.
static SWALLOW_VK: AtomicU32 = AtomicU32::new(0);

// 넘패드 . 키는 NumLock 이 꺼져 있으면 Del (VK_DELETE, 확장 플래그 없음) 로 들어옴.
// 단축키 매칭에선 num. (VK_DECIMAL) 로 취급 → NumLock 상관없이 같은 키.
// 일반 Delete 키는 확장 플래그가 있어서 그대로 del.
const VK_DELETE_RAW: u32 = 0x2E;
const VK_DECIMAL_RAW: u32 = 0x6E;

fn hotkey_vk(vk: u32, is_ext: bool) -> u32 {
    if vk == VK_DELETE_RAW && !is_ext { VK_DECIMAL_RAW } else { vk }
}

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

/// (vk, mods) 와 같은 단축키 중 pred 를 만족하는 첫 번째.
/// 같은 키를 상태별로 다른 기능에 써도 되게 (예: num. = 슬레이브 중 복귀, 미러 중 왼쪽 빼기) 상태로 거름.
fn find_hotkey_where(vk: u32, mods: u32, pred: fn(HookAction) -> bool) -> Option<HookAction> {
    let key = (mods << 16) | vk;
    ACTIONS.into_iter().find(|a| {
        let v = HOTKEYS[*a as usize].load(Ordering::Acquire);
        v != 0 && v == key && pred(*a)
    })
}

/// 슬레이브 쓰는 중 (Remote) 에 훅이 가로채서 처리하는 단축키.
/// 복귀 / 쓸어넘기기 on/off / 전체 on/off 만. 나머지는 슬레이브에 그냥 입력됨.
fn handled_while_remote(action: HookAction) -> bool {
    matches!(
        action,
        HookAction::Return | HookAction::Edge | HookAction::EdgeLeft
            | HookAction::EdgeRight | HookAction::Toggle
    )
}

/// 미러 중에 훅이 가로채서 처리하는 단축키 (미러 대상 전용 키).
/// 나머지 단축키 (num/ num- num* 등) 는 RegisterHotKey 로 Master 가 처리.
fn handled_while_mirror(action: HookAction) -> bool {
    matches!(action, HookAction::MirrorLeft | HookAction::MirrorRight)
}

/// RegisterHotKey 로 등록되는 단축키 (훅에서만 감지하는 복귀/미러 대상 키 제외).
fn is_registered(action: HookAction) -> bool {
    !matches!(action, HookAction::Return | HookAction::MirrorLeft | HookAction::MirrorRight)
}

/// Mirror 중 Slave 로 안 보낸 단축키. 떼짐도 같이 안 보내려고 기억.
static MIRROR_SKIP_VK: AtomicU32 = AtomicU32::new(0);

/// Mirror 중 이 키 이벤트를 Slave 로 보내지 말아야 하는지.
/// RegisterHotKey 로 Master 가 처리하는 단축키만 해당 (복귀 키 등 등록 안 된 키는 일반 키).
fn mirror_should_skip(vk: u32, is_up: bool) -> bool {
    if is_up {
        if vk == MIRROR_SKIP_VK.load(Ordering::Acquire) {
            MIRROR_SKIP_VK.store(0, Ordering::Release);
            return true;
        }
        return false;
    }
    if find_hotkey_where(vk, current_mods(), is_registered).is_some() {
        MIRROR_SKIP_VK.store(vk, Ordering::Release);
        true
    } else {
        false
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
            // 단축키 매칭용 vk (넘패드 Del → num.)
            let hk_vk = hotkey_vk(vk, is_ext);

            // 훅이 직접 처리하는 단축키: Slave 로 안 보내고 여기서 처리 + 소비.
            // 눌림에서 한 번만 실행하고, 오토리피트/떼기는 조용히 소비.
            //   Remote: 복귀 / 쓸어넘기기 / toggle. 이동·미러 키는 안 가로챔 → 슬레이브에 그냥 입력
            //           (슬레이브에서 넘패드 / - * 입력 가능. 다른 슬레이브로 가려면 복귀 후 이동).
            //   Mirror: 미러 대상 전용 키 (hotkey_mirror_left/right). 미러 아닐 땐 일반 키로 동작.
            if remote || mirror {
                if hk_vk == SWALLOW_VK.load(Ordering::Acquire) {
                    if is_up { SWALLOW_VK.store(0, Ordering::Release); }
                    return LRESULT(1);
                }
                if !is_up {
                    let pred: fn(HookAction) -> bool =
                        if remote { handled_while_remote } else { handled_while_mirror };
                    if let Some(action) = find_hotkey_where(hk_vk, current_mods(), pred) {
                        tracing::info!("master: 단축키 {:?} (vk={:#x}) 감지", action, vk);
                        SWALLOW_VK.store(hk_vk, Ordering::Release);
                        master::on_hook_hotkey(action);
                        return LRESULT(1);
                    }
                }
            }

            // 일반 키보드 forward: Remote 또는 Mirror 에서 forward_keyboard 켜져있으면.
            if FORWARD_KEYBOARD.load(Ordering::Acquire) {
                // Mirror 중엔 단축키 (num/ num- num* 등) 를 Slave 로 안 보냄.
                // 통과시켜서 RegisterHotKey 로 Master 가 처리.
                let skip = mirror && mirror_should_skip(hk_vk, is_up);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_vs_numpad_del() {
        // 일반 Delete (확장키) 는 del 그대로, 넘패드 Del (NumLock 꺼짐) 은 num. 로
        assert_eq!(hotkey_vk(VK_DELETE_RAW, true), VK_DELETE_RAW);
        assert_eq!(hotkey_vk(VK_DELETE_RAW, false), VK_DECIMAL_RAW);
        assert_eq!(hotkey_vk(VK_DECIMAL_RAW, false), VK_DECIMAL_RAW);
    }

    #[test]
    fn same_key_by_state() {
        // num. 을 복귀 (슬레이브 중) 와 미러 왼쪽 (미러 중) 에 같이 써도 상태별로 갈림
        set_hotkey(HookAction::Return, VK_DECIMAL_RAW);
        set_hotkey(HookAction::MirrorLeft, VK_DECIMAL_RAW);
        assert_eq!(find_hotkey_where(VK_DECIMAL_RAW, 0, handled_while_remote), Some(HookAction::Return));
        assert_eq!(find_hotkey_where(VK_DECIMAL_RAW, 0, handled_while_mirror), Some(HookAction::MirrorLeft));
        // modifier 가 다르면 매칭 안 됨
        assert_eq!(find_hotkey_where(VK_DECIMAL_RAW, 2, handled_while_remote), None);
        // 훅 전용 키는 미러 중 forward 제외 대상 (RegisterHotKey) 이 아님
        assert!(!is_registered(HookAction::MirrorLeft) && !is_registered(HookAction::Return));
        set_hotkey(HookAction::Return, 0);
        set_hotkey(HookAction::MirrorLeft, 0);
    }
}
