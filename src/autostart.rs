// Windows 자동 시작 + 관리자 권한 실행.
//
// 방식 두 가지 (sync() 가 config 에 맞춰 정리):
//   1) 일반: HKCU\Software\Microsoft\Windows\CurrentVersion\Run 에 값 추가. 관리자 권한 필요 없음.
//   2) run_as_admin: 작업 스케줄러에 "로그온 시, 가장 높은 권한으로" 작업 등록.
//      HKCU\Run 은 관리자 권한으로 못 띄움 (UAC 때문에 건너뜀). 작업 스케줄러는 확인창 없이 관리자로 실행.
//      관리자 권한이면 관리자 권한 프로그램 창 (설치창, 작업관리자 등) 에도 입력 주입 가능 (UIPI).
//      UAC 확인창 / 잠금화면 (보안 데스크톱) 은 이걸로도 안 됨.
// 서비스로 만들지 않는 이유: 서비스는 유저 데스크톱에 접근 못 함 (Raw Input, SetCursorPos 불가).

#![cfg(windows)]

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::System::Registry::*;
use windows::Win32::Foundation::ERROR_SUCCESS;

/// config 의 autostart / run_as_admin 에 맞춰 자동 실행 방식 정리.
///   autostart + run_as_admin → 작업 스케줄러 (관리자 권한일 때 등록/갱신), HKCU\Run 은 지움
///   autostart 만             → HKCU\Run (작업 스케줄러 작업이 있으면 지움)
///   autostart 꺼짐           → 둘 다 지움
/// 작업 등록이 안 되면 (관리자 권한 거절 등) HKCU\Run 으로라도 자동 시작.
pub fn sync(autostart: bool, run_as_admin: bool, elevated: bool) {
    let want_task = autostart && run_as_admin;
    let mut task_ok = false;
    if want_task {
        if elevated {
            match enable_admin_task() {
                Ok(()) => task_ok = true,
                Err(e) => tracing::warn!("autostart: 작업 스케줄러 등록 실패, 레지스트리로 대신: {:#}", e),
            }
        } else {
            // 관리자 권한이 아니면 등록 못 함. 예전에 등록해 둔 게 있으면 그걸 씀.
            task_ok = admin_task_exists();
        }
    } else if admin_task_exists() {
        if let Err(e) = disable_admin_task() {
            tracing::warn!("autostart: 작업 스케줄러 cursorlink 작업 삭제 실패 (관리자 권한 필요): {:#}", e);
        }
    }

    let use_run = autostart && !task_ok;
    let is = is_enabled();
    if use_run && !is {
        if let Err(e) = enable() { tracing::warn!("autostart 활성화 실패: {}", e); }
    } else if !use_run && is {
        if let Err(e) = disable() { tracing::warn!("autostart 비활성화 실패: {}", e); }
    }
}

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
    unsafe {
        let key = match open_run_key(false) { Ok(k) => k, Err(_) => return false };
        let name_w: Vec<u16> = VALUE_NAME.encode_utf16().chain(Some(0)).collect();
        let mut typ: REG_VALUE_TYPE = REG_VALUE_TYPE(0);
        let mut sz: u32 = 0;
        // windows 0.58: 마지막 세 인자는 raw pointer Option 이라 명시적 캐스트 필요.
        let r = RegQueryValueExW(
            key,
            PCWSTR(name_w.as_ptr()),
            None,
            Some(&mut typ as *mut REG_VALUE_TYPE),
            None,
            Some(&mut sz as *mut u32),
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

// -----------------------------------------------------------------------------
// 관리자 권한 실행 (run_as_admin)
// -----------------------------------------------------------------------------

const TASK_NAME: &str = "cursorlink";
/// 콘솔 프로그램 (schtasks) 실행 시 검은 창 안 뜨게.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 이 프로세스가 관리자 권한 (elevated) 인지.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        ).is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// 같은 exe 를 관리자 권한으로 다시 실행 (UAC 확인창). Ok 면 호출자는 바로 종료해야 함.
/// 사용자가 확인창에서 "아니요" 누르면 Err.
pub fn relaunch_elevated() -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().context("실행 파일 경로 조회 실패")?;
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let verb = wide("runas");
    let file = wide(&exe.display().to_string());
    let params = wide(&args.join(" "));
    let cwd = wide(&dir.display().to_string());
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR(cwd.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    // 32 초과면 성공 (ShellExecute 규칙)
    if r.0 as isize > 32 {
        Ok(())
    } else {
        anyhow::bail!("ShellExecute runas 실패 (code={})", r.0 as isize)
    }
}

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

pub fn admin_task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 작업 스케줄러에 관리자 권한 자동 실행 작업 등록 (있으면 덮어씀 → exe 경로 갱신).
/// 관리자 권한 프로세스에서만 성공함.
pub fn enable_admin_task() -> Result<()> {
    let exe = std::env::current_exe().context("실행 파일 경로 조회 실패")?;
    let dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let xml = task_xml(&exe.display().to_string(), &dir, &current_user(), true);

    let path = std::env::temp_dir().join("cursorlink_task.xml");
    write_utf16(&path, &xml)?;
    let out = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path.display().to_string(), "/F"]);
    let _ = std::fs::remove_file(&path);
    let out = out.context("schtasks 실행 실패")?;
    if out.status.success() {
        tracing::info!("autostart: 작업 스케줄러 등록 (로그온 시 관리자 권한, 경로={})", exe.display());
        Ok(())
    } else {
        anyhow::bail!("schtasks /Create 실패: {}", String::from_utf8_lossy(&out.stderr).trim())
    }
}

pub fn disable_admin_task() -> Result<()> {
    let out = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]).context("schtasks 실행 실패")?;
    if out.status.success() {
        tracing::info!("autostart: 작업 스케줄러 작업 삭제");
        Ok(())
    } else {
        anyhow::bail!("schtasks /Delete 실패: {}", String::from_utf8_lossy(&out.stderr).trim())
    }
}

fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{}\\{}", d, user),
        _ => user,
    }
}

/// schtasks /XML 은 UTF-16 (BOM) 파일을 기대.
fn write_utf16(path: &std::path::Path, s: &str) -> Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(path, bytes).with_context(|| format!("{} 쓰기 실패", path.display()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 작업 스케줄러 XML. 기본값 함정을 피하려고 직접 지정:
///   - 배터리 모드에서도 시작/유지 (기본은 배터리면 안 켜짐 → 노트북 문제)
///   - 실행 시간 제한 없음 (기본은 3일 뒤 강제 종료)
///   - 우선순위 보통 (기본 7 = 보통보다 낮음)
///   - 이미 실행 중이면 새로 안 띄움
/// highest = false 는 테스트용 (일반 권한으로 등록 검증할 때).
pub(crate) fn task_xml(exe: &str, dir: &str, user: &str, highest: bool) -> String {
    let level = if highest { "HighestAvailable" } else { "LeastPrivilege" };
    let exe = xml_escape(exe);
    let dir = xml_escape(dir);
    let user = xml_escape(user);
    format!(
r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>cursorlink 자동 실행 (로그온 시 관리자 권한)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_settings() {
        let x = task_xml(r"C:\cursor & link\cursorlink.exe", r"C:\cursor & link", r"PC\user", true);
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<Priority>4</Priority>"));
        assert!(x.contains(r"<Command>C:\cursor &amp; link\cursorlink.exe</Command>"));
        assert!(x.contains(r"<UserId>PC\user</UserId>"));
    }

    /// 수동 검증용: CURSORLINK_TASK_XML_OUT 에 경로를 주면 일반 권한 버전 XML 을 써둠
    /// (schtasks /Create /XML 로 실제 등록되는지 확인할 때). 평소엔 아무것도 안 함.
    #[test]
    fn dump_task_xml_for_manual_check() {
        if let Ok(out) = std::env::var("CURSORLINK_TASK_XML_OUT") {
            let xml = task_xml(r"C:\Windows\System32\notepad.exe", r"C:\Windows\System32", &current_user(), false);
            write_utf16(std::path::Path::new(&out), &xml).unwrap();
        }
    }
}
