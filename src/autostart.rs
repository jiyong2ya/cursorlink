// Windows 자동 시작.
//
// 방식: HKCU\Software\Microsoft\Windows\CurrentVersion\Run 에 값 추가.
// 관리자 권한 필요 없음 (HKCU 니까).
// 서비스로 만들지 않는 이유: 서비스는 유저 데스크톱에 접근 못 함 (Raw Input, SetCursorPos 불가).

#![cfg(windows)]

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::System::Registry::*;
use windows::Win32::Foundation::ERROR_SUCCESS;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "cursorlink";

pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("실행 파일 경로 조회 실패")?;
    let exe_str = format!("\"{}\"", exe.display());
    write_run_key(&exe_str)
}

pub fn disable() -> Result<()> {
    unsafe {
        let key = open_run_key(true)?;
        let name_w: Vec<u16> = VALUE_NAME.encode_utf16().chain(Some(0)).collect();
        let r = RegDeleteValueW(key, PCWSTR(name_w.as_ptr()));
        let _ = RegCloseKey(key);
        if r == ERROR_SUCCESS {
            tracing::info!("autostart: 비활성화");
            Ok(())
        } else {
            // 이미 없어도 성공 처리 (사용자 관점에서 원하는 상태 = "없음" 이니까)
            tracing::debug!("autostart: 값 없음 (이미 비활성화된 상태)");
            Ok(())
        }
    }
}

pub fn is_enabled() -> bool {
    // 조용히 시도.
    unsafe {
        let key = match open_run_key(false) { Ok(k) => k, Err(_) => return false };
        let name_w: Vec<u16> = VALUE_NAME.encode_utf16().chain(Some(0)).collect();
        let mut typ: REG_VALUE_TYPE = REG_VALUE_TYPE(0);
        let mut sz: u32 = 0;
        let r = RegQueryValueExW(
            key,
            PCWSTR(name_w.as_ptr()),
            None,
            Some(&mut typ),
            None,
            Some(&mut sz),
        );
        let _ = RegCloseKey(key);
        r == ERROR_SUCCESS
    }
}

fn write_run_key(exe: &str) -> Result<()> {
    unsafe {
        let key = open_run_key(true)?;
        let name_w: Vec<u16> = VALUE_NAME.encode_utf16().chain(Some(0)).collect();
        let value_w: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
        let bytes = std::slice::from_raw_parts(
            value_w.as_ptr() as *const u8,
            value_w.len() * 2,
        );
        let r = RegSetValueExW(
            key,
            PCWSTR(name_w.as_ptr()),
            0,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);
        if r == ERROR_SUCCESS {
            tracing::info!("autostart: 활성화 (경로={})", exe);
            Ok(())
        } else {
            anyhow::bail!("RegSetValueExW 실패: {:?}", r)
        }
    }
}

unsafe fn open_run_key(write: bool) -> Result<HKEY> {
    let path_w: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
    let mut hkey = HKEY::default();
    let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
    let r = RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(path_w.as_ptr()),
        0,
        access,
        &mut hkey,
    );
    if r == ERROR_SUCCESS {
        Ok(hkey)
    } else {
        anyhow::bail!("RegOpenKeyExW 실패: {:?}", r)
    }
}
