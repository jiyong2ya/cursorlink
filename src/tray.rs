// 트레이 아이콘.
//
// Shell_NotifyIcon 로 시스템 트레이에 아이콘 추가.
// 사용자가 아이콘 우클릭 → 팝업 메뉴 (Toggle / Exit).
//
// master/slave 의 기존 hidden window 를 재사용.
// 트레이 아이콘의 콜백 메시지는 WM_APP+1 로 통일.

#![cfg(windows)]

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM, POINT};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TRAY: u32 = WM_APP + 1;
pub const TRAY_ICON_ID: u32 = 1;

pub const MENU_TOGGLE:   u32 = 100;
pub const MENU_EXIT:     u32 = 101;
pub const MENU_SETTINGS: u32 = 102;
pub const MENU_LOG:      u32 = 103;

/// 트레이 아이콘 등록 (hwnd 에 WM_TRAY 메시지 전달).
pub fn add(hwnd: HWND, tooltip: &str) -> Result<()> {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;

        // 시스템 기본 앱 아이콘 사용 (본격 배포 시 자체 .ico 로 교체)
        nid.hIcon = LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default();

        // 툴팁 (최대 127자, wide)
        let tip: Vec<u16> = tooltip.encode_utf16().take(127).chain(Some(0)).collect();
        for (i, ch) in tip.iter().enumerate() {
            if i >= nid.szTip.len() { break; }
            nid.szTip[i] = *ch;
        }

        Shell_NotifyIconW(NIM_ADD, &nid).ok().map_err(|e| anyhow::anyhow!("Shell_NotifyIconW ADD: {:?}", e))?;
    }
    tracing::info!("트레이 아이콘 등록");
    Ok(())
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 우클릭 시 팝업 메뉴 표시.
/// enabled: 현재 기능 on/off 상태 (체크 표시용).
pub unsafe fn show_context_menu(hwnd: HWND, enabled: bool) {
    let hmenu = CreatePopupMenu().unwrap_or_default();
    if hmenu.is_invalid() { return; }

    // Toggle 항목 (체크박스)
    let toggle_str: Vec<u16> = "기능 사용 (Toggle)\0".encode_utf16().collect();
    let mut flags = MF_STRING;
    if enabled { flags |= MF_CHECKED; }
    let _ = AppendMenuW(hmenu, flags, MENU_TOGGLE as usize, PCWSTR(toggle_str.as_ptr()));

    // 구분선
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    // 설정 편집
    let settings_str: Vec<u16> = "설정 편집 (config.toml)\0".encode_utf16().collect();
    let _ = AppendMenuW(hmenu, MF_STRING, MENU_SETTINGS as usize, PCWSTR(settings_str.as_ptr()));

    // 로그 열기
    let log_str: Vec<u16> = "로그 보기 (cursorlink.log)\0".encode_utf16().collect();
    let _ = AppendMenuW(hmenu, MF_STRING, MENU_LOG as usize, PCWSTR(log_str.as_ptr()));

    // 구분선
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    // Exit
    let exit_str: Vec<u16> = "종료\0".encode_utf16().collect();
    let _ = AppendMenuW(hmenu, MF_STRING, MENU_EXIT as usize, PCWSTR(exit_str.as_ptr()));

    // 메뉴가 뜰 위치 = 커서 위치
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    // 포커스 없으면 메뉴가 안 사라지는 문제 방지
    let _ = SetForegroundWindow(hwnd);

    TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x, pt.y,
        0,
        hwnd,
        None,
    );

    let _ = DestroyMenu(hmenu);
}

/// WM_COMMAND (메뉴 선택) 파싱: LOWORD(wparam) 가 메뉴 ID.
pub fn menu_id_from_wparam(wparam: WPARAM) -> u32 {
    (wparam.0 as u32) & 0xFFFF
}

/// WM_TRAY lparam 에서 실제 트레이 이벤트 타입 추출 (WM_RBUTTONUP 등).
pub fn tray_event_from_lparam(lparam: LPARAM) -> u32 {
    (lparam.0 as u32) & 0xFFFF
}

/// 외부 프로그램으로 파일 열기 (notepad 등 연결된 앱).
pub fn open_in_editor(path: &std::path::Path) {
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}

