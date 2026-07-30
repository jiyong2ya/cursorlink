use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Master,
    Slave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub peer_ip: String,
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_tcp_port")]
    pub tcp_port: u16,
    #[serde(default = "default_secret")]
    pub shared_secret: String,
}

fn default_udp_port() -> u16 { 46011 }
fn default_tcp_port() -> u16 { 46012 }
fn default_secret() -> String { "change-me-to-random-string".to_string() }

impl Config {
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            anyhow::bail!(
                "설정 파일이 없습니다: {}\nconfig.example.toml 을 config.toml 로 복사해서 편집하세요.",
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("설정 파일 읽기 실패: {}", path.display()))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("설정 파일 파싱 실패: {}", path.display()))?;
        Ok(cfg)
    }
}
