// 커서 제어 헬퍼 (양쪽 모두 사용).

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, GetCursorPos, GetSystemMetrics, SetCursorPos, ShowCursor,
    SM_CXSCREEN, SM_CYSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

pub struct Rect { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }

impl Rect {
    pub fn width(&self)  -> i32 { self.right - self.left }
    pub fn height(&self) -> i32 { self.bottom - self.top }
    pub fn center(&self) -> (i32, i32) {
        (self.left + self.width() / 2, self.top + self.height() / 2)
    }
}

/// 가상 스크린 전체 (멀티 모니터 포함).
pub fn virtual_screen() -> Rect {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        Rect { left: x, top: y, right: x + w, bottom: y + h }
    }
}

/// 프라이머리 모니터.
pub fn primary_screen() -> Rect {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        Rect { left: 0, top: 0, right: w, bottom: h }
    }
}

pub fn get_pos() -> Option<POINT> {
    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_ok() { Some(pt) } else { None }
    }
}

pub fn set_pos(x: i32, y: i32) {
    unsafe { let _ = SetCursorPos(x, y); }
}

/// 지정된 영역으로 커서 이동 제한. None 이면 해제.
pub fn clip(r: Option<Rect>) {
    unsafe {
        match r {
            Some(r) => {
                let rect = RECT { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
                // windows 0.58: ClipCursor 인자는 Option<*const RECT>
                let _ = ClipCursor(Some(&rect as *const RECT));
            }
            None => {
                let _ = ClipCursor(None);
            }
        }
    }
}

/// 커서 중앙에 1x1 락 (Remote 상태 유지용).
pub fn lock_center() {
    let scr = primary_screen();
    let (cx, cy) = scr.center();
    set_pos(cx, cy);
    clip(Some(Rect { left: cx, top: cy, right: cx + 1, bottom: cy + 1 }));
}

pub fn unlock() {
    clip(None);
}

/// ShowCursor 는 참조 카운트가 있어서 켜고/끄기 반복하면 밸런스 깨짐.
/// 여기선 상태로 관리하고 조건부 호출.
static mut CURSOR_HIDDEN: bool = false;

pub fn hide() {
    unsafe {
        if !CURSOR_HIDDEN {
            // ShowCursor(FALSE) 는 카운터가 -1 될 때 실제 숨김.
            // 초기값 0 에서 시작하니 한 번만 호출.
            while ShowCursor(false) >= 0 {}
            CURSOR_HIDDEN = true;
        }
    }
}

pub fn show() {
    unsafe {
        if CURSOR_HIDDEN {
            while ShowCursor(true) < 0 {}
            CURSOR_HIDDEN = false;
        }
    }
}
