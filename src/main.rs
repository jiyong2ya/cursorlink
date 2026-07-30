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

use anyhow::{Context, Result};
use config::{Config, Mode};

fn main() -> Result<()> {
    logging::init().context("로깅 초기화 실패")?;

    let cfg = Config::load_or_default("config.toml")?;
    tracing::info!(
        "cursorlink 시작 mode={:?} peer={}:{} tcp={}",
        cfg.mode, cfg.peer_ip, cfg.udp_port, cfg.tcp_port
    );

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
