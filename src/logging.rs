// 로깅 초기화.
//
// release 빌드는 windows_subsystem="windows" 라 stdout 이 사라짐 → 파일에 남긴다.
// debug 빌드는 stdout + file 둘 다 남긴다.
//
// 로그 파일: 실행 파일과 같은 디렉토리 아래 `cursorlink.log`.
// (INSTALL_DIR/cursorlink.log — 유저가 눈으로 확인 가능)

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::fmt::MakeWriter;

pub fn init() -> Result<()> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let file_writer = FileWriter::new(log_path()?)
        .context("로그 파일 열기 실패")?;

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(file_writer);

    #[cfg(debug_assertions)]
    {
        let stdout_layer = fmt::layer().with_target(false);
        tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(file_layer)
            .init();
    }

    #[cfg(not(debug_assertions))]
    {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .init();
    }

    Ok(())
}

fn log_path() -> Result<PathBuf> {
    let mut p = std::env::current_exe()
        .context("실행 파일 경로 조회 실패")?;
    p.pop();
    p.push("cursorlink.log");
    Ok(p)
}

/// 파일에 append 하는 MakeWriter. Mutex 로 여러 스레드 write 직렬화.
struct FileWriter {
    inner: std::sync::Arc<Mutex<std::fs::File>>,
}

impl FileWriter {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { inner: std::sync::Arc::new(Mutex::new(file)) })
    }
}

impl Clone for FileWriter {
    fn clone(&self) -> Self {
        Self { inner: std::sync::Arc::clone(&self.inner) }
    }
}

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriterGuard;
    fn make_writer(&'a self) -> Self::Writer {
        FileWriterGuard { inner: std::sync::Arc::clone(&self.inner) }
    }
}

struct FileWriterGuard {
    inner: std::sync::Arc<Mutex<std::fs::File>>,
}

impl Write for FileWriterGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self.inner.lock().unwrap();
        f.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut f = self.inner.lock().unwrap();
        f.flush()
    }
}
