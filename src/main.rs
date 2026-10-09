// cursorlink — Windows PC 간 마우스/키보드 공유 (마스터 1 + 슬레이브 최대 2: 왼쪽/오른쪽)

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
        "cursorlink 시작 mode={:?} peer={} left_peer={} udp={} tcp={}",
        cfg.mode, cfg.peer_ip, cfg.left_peer_ip, cfg.udp_port, cfg.tcp_port
    );

    #[cfg(windows)]
    {
        // 자동 실행 등록은 항상 유지되므로, 자동 실행으로 켜졌는데 autostart = false 면 여기서 끝.
        // (config 만 고치고 재부팅해도 바로 반영되게)
        if autostart::launched_by_autostart() && !cfg.autostart {
            tracing::info!("autostart = false → 자동 실행 건너뜀");
            return Ok(());
        }

        // run_as_admin: 관리자 권한이 아니면 관리자로 다시 실행하고 이 프로세스는 끝냄
        // (포트 열기 전이라 겹칠 일 없음). 확인창에서 "아니요" 면 일반 권한으로 계속.
        if cfg.run_as_admin && !autostart::is_elevated() {
            match autostart::relaunch_elevated() {
                Ok(()) => {
                    tracing::info!("관리자 권한으로 다시 실행함 → 이 프로세스는 종료");
                    return Ok(());
                }
                Err(e) => tracing::warn!("관리자 권한 실행 안 됨 (취소?) → 일반 권한으로 계속: {:#}", e),
            }
        }
        let elevated = autostart::is_elevated();
        tracing::info!("관리자 권한: {}", if elevated { "예" } else { "아니요" });

        // 자동 실행 등록 유지 (run_as_admin 에 맞춰 레지스트리 or 작업 스케줄러)
        autostart::sync(cfg.run_as_admin, elevated);
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
