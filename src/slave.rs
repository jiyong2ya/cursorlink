// Slave 오케스트레이션.
//
// 스레드:
//   - main       : UDP 수신 루프 (블로킹) + 델타 주입
//   - tcp_accept : TCP listen/accept, HELLO 검증 후 read 루프 + writer
//   - hb_ticker  : 3초마다 HEARTBEAT 전송 요청
//
// 상태 전환:
//   Disconnected --TCP accept + HELLO OK--> Idle
//   Idle --TAKE_CONTROL 수신--> Active (커서 표시 + 진입 위치로 이동)
//   Active --왼쪽 엣지 도달--> Idle (RETURN_CONTROL 송신 + 커서 숨김)
//   * --TCP 끊김--> Disconnected (Idle 강제, 커서 상태 복구)

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Sender};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::input::cursor;
use crate::net::tcp::{
    self, Frame, MSG_HEARTBEAT, MSG_HELLO, MSG_RETURN_CONTROL, MSG_TAKE_CONTROL,
};
use crate::state::{SlaveShared, SlaveState};

pub static SHARED: SlaveShared = SlaveShared::new();

pub type TcpTx = Sender<Frame>;

pub struct SlaveCtx {
    pub cfg: Config,
    pub tcp_tx: TcpTx,
}

pub fn run(cfg: Config) -> Result<()> {
    let (tx, rx) = bounded::<Frame>(64);

    // TCP 서버 스레드
    let secret = cfg.shared_secret.clone();
    let tcp_port = cfg.tcp_port;
    let rx_clone = rx.clone();
    let tx_for_hb = tx.clone();

    thread::Builder::new()
        .name("cursorlink-tcp-srv".to_string())
        .spawn(move || tcp_server_loop(tcp_port, secret, rx_clone))
        .context("TCP 서버 스레드 시작 실패")?;

    // HEARTBEAT ticker
    thread::Builder::new()
        .name("cursorlink-hb".to_string())
        .spawn(move || heartbeat_loop(tx_for_hb))
        .context("HEARTBEAT 스레드 시작 실패")?;

    // 메인: UDP 수신
    crate::input::inject::run_with_ctx(SlaveCtx { cfg, tcp_tx: tx })
}

fn tcp_server_loop(port: u16, secret: String, rx: crossbeam_channel::Receiver<Frame>) {
    let listener = match tcp::listen(port) {
        Ok(l) => l,
        Err(e) => { tracing::error!("slave: TCP listen 실패: {}", e); return; }
    };
    tracing::info!("slave: TCP {} 대기 중", port);

    loop {
        SHARED.set(SlaveState::Disconnected);
        match listener.accept() {
            Ok((mut sock, addr)) => {
                if let Err(e) = tcp::configure_stream(&sock) {
                    tracing::warn!("slave: stream 설정 실패: {}", e);
                    continue;
                }
                tracing::info!("slave: TCP 연결 수락: {}", addr);
                // HELLO 검증
                match tcp::read_frame(&mut sock) {
                    Ok(f) if f.msg_type() == MSG_HELLO && f.is_authenticated(&secret) => {
                        SHARED.set(SlaveState::Idle);
                        tracing::info!("slave: HELLO OK, Idle 상태");
                        run_connected(&mut sock, &rx);
                        tracing::warn!("slave: 연결 종료됨");
                        force_return_idle();
                    }
                    Ok(_) => {
                        tracing::warn!("slave: HELLO 인증 실패, 연결 종료");
                    }
                    Err(e) => {
                        tracing::warn!("slave: HELLO read 실패: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("slave: accept 오류: {}", e);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn run_connected(sock: &mut TcpStream, rx: &crossbeam_channel::Receiver<Frame>) {
    let write_sock = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => { tracing::warn!("try_clone 실패: {}", e); return; }
    };
    let rx_clone = rx.clone();
    let writer = thread::Builder::new()
        .name("cursorlink-tcp-w".to_string())
        .spawn(move || tcp_writer_loop(write_sock, rx_clone));
    let writer = match writer {
        Ok(h) => h,
        Err(e) => { tracing::warn!("writer 스레드 실패: {}", e); return; }
    };

    loop {
        match tcp::read_frame(sock) {
            Ok(f) => handle_incoming(f),
            Err(e) => {
                tracing::warn!("slave: TCP read 오류: {}", e);
                break;
            }
        }
    }

    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
}

fn tcp_writer_loop(mut sock: TcpStream, rx: crossbeam_channel::Receiver<Frame>) {
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(f) => {
                if let Err(e) = tcp::write_frame(&mut sock, f) {
                    tracing::warn!("slave: TCP write 오류: {}", e);
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        }
    }
}

fn handle_incoming(f: Frame) {
    match f.msg_type() {
        MSG_TAKE_CONTROL => {
            let entry_y_pct = f.payload_byte(1);
            tracing::info!("slave: TAKE_CONTROL 수신 (entry_y_pct={}) → Active", entry_y_pct);
            enter_active(entry_y_pct);
        }
        MSG_HEARTBEAT => {
            tracing::trace!("slave: HEARTBEAT 수신");
        }
        MSG_HELLO | MSG_RETURN_CONTROL => {
            tracing::debug!("slave: 상황에 맞지 않는 msg_type={:02x}", f.msg_type());
        }
        other => {
            tracing::warn!("slave: 알 수 없는 msg_type=0x{:02x}", other);
        }
    }
}

fn heartbeat_loop(tx: TcpTx) {
    loop {
        thread::sleep(Duration::from_secs(3));
        let _ = tx.try_send(Frame::new(MSG_HEARTBEAT));
    }
}

// -----------------------------------------------------------------------------
// 상태 전환 API
// -----------------------------------------------------------------------------

fn enter_active(entry_y_pct: u8) {
    if !SHARED.enabled.load(Ordering::Acquire) { return; }

    let scr = cursor::primary_screen();
    // 진입: 왼쪽 끝 (10px 안쪽) + 지정된 세로 비율
    let entry_x = scr.left + 10;
    let entry_y = scr.top + (scr.height() as i64 * entry_y_pct as i64 / 100) as i32;

    cursor::set_pos(entry_x, entry_y);
    cursor::show();
    SHARED.set(SlaveState::Active);
}

/// 왼쪽 엣지 도달 시 호출. Active → Idle.
pub fn return_control(tx: &TcpTx) {
    if SHARED.get() != SlaveState::Active { return; }
    let _ = tx.try_send(Frame::new(MSG_RETURN_CONTROL));
    cursor::hide();
    SHARED.set(SlaveState::Idle);
    tracing::info!("slave: Active → Idle");
}

fn force_return_idle() {
    if SHARED.get() == SlaveState::Active {
        cursor::hide();
    }
    SHARED.set(SlaveState::Disconnected);
}

pub fn set_enabled(en: bool) {
    SHARED.enabled.store(en, Ordering::Release);
    tracing::info!("slave: enabled={}", en);
}
