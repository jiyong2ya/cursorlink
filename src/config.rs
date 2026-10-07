use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Master,
    Slave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    /// master: 오른쪽 슬레이브 주소 (빈 문자열이면 오른쪽 없음) / slave: 마스터 주소.
    pub peer_ip: String,
    /// master 전용: 왼쪽 슬레이브 주소. 빈 문자열이면 왼쪽 없음.
    #[serde(default)]
    pub left_peer_ip: String,
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_tcp_port")]
    pub tcp_port: u16,
    #[serde(default = "default_secret")]
    pub shared_secret: String,
    #[serde(default = "default_hotkey")]
    pub hotkey_toggle: String,
    /// Mirror 모드 (Master + Slave 동시 조작) 토글 단축키.
    /// 빈 문자열이면 Mirror 기능 비활성.
    #[serde(default)]
    pub hotkey_mirror: String,
    /// 오른쪽 슬레이브로 즉시 전환 단축키. Slave 커서는 화면 중앙에 놓임.
    /// 미러 중에는 오른쪽 슬레이브를 미러 대상에 넣기/빼기.
    /// 빈 문자열이면 비활성 (엣지 크로싱만 사용).
    #[serde(default)]
    pub hotkey_transfer: String,
    /// 왼쪽 슬레이브로 즉시 전환 단축키. 미러 중에는 왼쪽 미러 대상 넣기/빼기.
    #[serde(default)]
    pub hotkey_transfer_left: String,
    /// 쓸어넘기기 (마스터 화면 끝 → 슬레이브) on/off 단축키.
    #[serde(default)]
    pub hotkey_edge_toggle: String,
    /// 시작 시 쓸어넘기기 사용 여부. 슬레이브 → 마스터 복귀는 항상 동작.
    #[serde(default = "default_true")]
    pub edge_switch: bool,
    /// Remote 상태에서 Master 로 즉시 복귀 단축키.
    /// LL 훅 안에서 감지 (Slave 로 forward 안 하고 소비).
    #[serde(default)]
    pub hotkey_return: String,
    #[serde(default)]
    pub autostart: bool,
    /// Remote 상태에서 키보드 입력을 Slave 로 전송할지.
    /// false 면 마우스만 넘기고 키보드는 각 PC 가 각자 처리 (Slave 에 게임 켜져있을 때 유용).
    #[serde(default = "default_forward_keyboard")]
    pub forward_keyboard: bool,
}

fn default_hotkey() -> String { "ctrl+alt+shift+k".to_string() }
fn default_forward_keyboard() -> bool { true }
fn default_true() -> bool { true }

fn default_udp_port() -> u16 { 46011 }
fn default_tcp_port() -> u16 { 46012 }
fn default_secret() -> String { "change-me-to-random-string".to_string() }

impl Config {
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // 첫 실행: config.toml 이 없으면 example 을 복사하고 notepad 로 열어줌.
        if !path.exists() {
            first_run_setup(path)?;
        }

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("설정 파일 읽기 실패: {}", path.display()))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("설정 파일 파싱 실패: {}", path.display()))?;
        Ok(cfg)
    }

    pub fn config_path() -> PathBuf {
        // exe 옆의 config.toml
        let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        p.pop();
        p.push("config.toml");
        p
    }
}

/// 첫 실행 처리:
///   - config.example.toml 이 exe 옆에 있으면 config.toml 로 복사
///   - notepad 로 열어서 사용자가 편집할 수 있게 하고, 사용자에게 알림
///   - 이 함수가 끝나도 config.toml 이 없으면 에러 반환 (사용자가 notepad 를 그냥 닫은 경우)
fn first_run_setup(path: &Path) -> Result<()> {
    let example = {
        let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        p.pop();
        p.push("config.example.toml");
        p
    };

    if example.exists() {
        std::fs::copy(&example, path)
            .with_context(|| format!("config.example.toml 복사 실패"))?;
        tracing::info!("첫 실행: {} 를 {} 로 복사했습니다", example.display(), path.display());
    } else {
        // example 이 없으면 최소 기본값을 씀
        let default = r#"mode = "master"
peer_ip = "192.168.0.20"
udp_port = 46011
tcp_port = 46012
shared_secret = "change-me-to-random-string"
hotkey_toggle = "ctrl+alt+shift+k"
autostart = false
"#;
        std::fs::write(path, default)
            .with_context(|| format!("기본 config 작성 실패"))?;
        tracing::info!("첫 실행: 기본 {} 생성", path.display());
    }

    // notepad 로 열기 + 사용자 안내 (Windows 만 유효)
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONINFORMATION};

        let msg = format!(
            "cursorlink 이 처음 실행됩니다.\n\n\
             {} 를 편집해서 mode(master/slave)와 peer_ip 를 설정한 뒤,\n\
             파일을 저장하고 이 알림을 확인하시면 프로그램이 계속 실행됩니다.",
            path.display()
        );
        let title_w: Vec<u16> = "cursorlink\0".encode_utf16().collect();
        let msg_w: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();

        // 편집기 실행 (비동기)
        let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
        // 사용자가 편집 후 OK 누르면 진행
        unsafe {
            MessageBoxW(None, PCWSTR(msg_w.as_ptr()), PCWSTR(title_w.as_ptr()), MB_OK | MB_ICONINFORMATION);
        }
    }

    Ok(())
}
