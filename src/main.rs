// cursorlink — 두 Windows PC 간 마우스/키보드 공유

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod net;
mod state;
mod logging;

#[cfg(windows)]
mod input;
#[cfg(windows)]
mod master;
#[cfg(windows)]
mod slave;
#[cfg(windows)]
mod hotkey;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod tray;

use anyhow::{Context, Result};
use config::{Config, Mode};

fn main() -> Result<()> {
    logging::init().context("로깅 초기화 실패")?;

    // DPI 스케일링 환경에서도 물리 픽셀 기준 좌표를 얻기 위해 per-monitor DPI awareness 설정.
    // Mirror 모드의 % 계산과 커서 위치 sync 정확도에 필수.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cfg = Config::load_or_default(Config::config_path())?;
    tracing::info!(
        "cursorlink 시작 mode={:?} peer={}:{} tcp={}",
        cfg.mode, cfg.peer_ip, cfg.udp_port, cfg.tcp_port
    );

    // 자동시작 동기화 (config.autostart 에 맞춰 레지스트리 업데이트)
    #[cfg(windows)]
    {
        let want = cfg.autostart;
        let is  = autostart::is_enabled();
        if want && !is {
            if let Err(e) = autostart::enable() { tracing::warn!("autostart 활성화 실패: {}", e); }
        } else if !want && is {
            if let Err(e) = autostart::disable() { tracing::warn!("autostart 비활성화 실패: {}", e); }
        }
    }

    let r = match cfg.mode {
        Mode::Master => run_master(cfg),
        Mode::Slave  => run_slave(cfg),
    };
    if let Err(e) = &r {
        tracing::error!("종료: {:#}", e);
    }
    r
}

fn run_master(cfg: Config) -> Result<()> {
    #[cfg(windows)]
    { master::run(cfg) }
    #[cfg(not(windows))]
    { let _ = cfg; anyhow::bail!("cursorlink 은 Windows 전용입니다") }
}

fn run_slave(cfg: Config) -> Result<()> {
    #[cfg(windows)]
    { slave::run(cfg) }
    #[cfg(not(windows))]
    { let _ = cfg; anyhow::bail!("cursorlink 은 Windows 전용입니다") }
}
