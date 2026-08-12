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
use crate::hotkey::{self, HotkeySpec};
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
    let tx_for_watch = tx.clone();
    let tx_for_srv = tx.clone();

    thread::Builder::new()
        .name("cursorlink-tcp-srv".to_string())
        .spawn(move || tcp_server_loop(tcp_port, secret, rx_clone, tx_for_srv))
        .context("TCP 서버 스레드 시작 실패")?;

    // HEARTBEAT ticker
    thread::Builder::new()
        .name("cursorlink-hb".to_string())
        .spawn(move || heartbeat_loop(tx_for_hb))
        .context("HEARTBEAT 스레드 시작 실패")?;

    // Watchdog: secure desktop / UAC 자동 감지 → RETURN_CONTROL
    thread::Builder::new()
        .name("cursorlink-watchdog".to_string())
        .spawn(move || foreground_watcher_loop(tx_for_watch))
        .context("watchdog 스레드 시작 실패")?;

    // Hotkey + Tray 스레드 (자체 hidden window)
    let hk_spec = HotkeySpec { toggle: cfg.hotkey_toggle.clone(), mirror: String::new() };
    if let Err(e) = hotkey::spawn_window_thread(
        hk_spec,
        "cursorlink (slave)".to_string(),
        || {
            let cur = SHARED.enabled.load(Ordering::Acquire);
            set_enabled(!cur);
        },
        || SHARED.enabled.load(Ordering::Acquire),
    ) {
        tracing::warn!("slave: hotkey/tray 스레드 시작 실패, 계속 진행: {}", e);
    }

    // 메인: UDP 수신
    crate::input::inject::run_with_ctx(SlaveCtx { cfg, tcp_tx: tx })
}

fn tcp_server_loop(port: u16, secret: String, rx: crossbeam_channel::Receiver<Frame>, tx: TcpTx) {
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
                        run_connected(&mut sock, &rx, &tx);
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

fn run_connected(sock: &mut TcpStream, rx: &crossbeam_channel::Receiver<Frame>, tx: &TcpTx) {
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

    // read timeout (configure_stream 에서 6초로 설정됨) 은 주기적 wake-up.
    // WouldBlock/TimedOut 은 정상 idle 이므로 continue. 진짜 연결 문제만 break.
    loop {
        match tcp::read_frame(sock) {
            Ok(f) => handle_incoming(f, tx),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => continue,
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

fn handle_incoming(f: Frame, tx: &TcpTx) {
    match f.msg_type() {
        MSG_TAKE_CONTROL => {
            let entry_y_pct = f.payload_byte(1);
            tracing::info!("slave: TAKE_CONTROL 수신 (entry_y_pct={}) → Active", entry_y_pct);
            enter_active(entry_y_pct, tx);
        }
        MSG_RETURN_CONTROL => {
            // Master 가 Mirror OFF 하면서 보낸 경우. Idle 로 복귀 + 커서 숨김.
            tracing::info!("slave: RETURN_CONTROL 수신 → Idle");
            enter_idle_from_master();
        }
        MSG_HEARTBEAT => {
            tracing::trace!("slave: HEARTBEAT 수신");
        }
        MSG_HELLO => {
            tracing::debug!("slave: 상황에 맞지 않는 HELLO 수신");
        }
        other => {
            tracing::warn!("slave: 알 수 없는 msg_type=0x{:02x}", other);
        }
    }
}

/// Master 로부터 RETURN_CONTROL 을 받아 Active → Idle 복귀.
fn enter_idle_from_master() {
    if SHARED.get() == SlaveState::Active {
        cursor::hide();
    }
    SHARED.set(SlaveState::Idle);
}

fn heartbeat_loop(tx: TcpTx) {
    loop {
        thread::sleep(Duration::from_secs(3));
        let _ = tx.try_send(Frame::new(MSG_HEARTBEAT));
    }
}

/// Secure desktop (UAC, lock screen, Ctrl+Alt+Del 등) 감지용 watchdog.
///
/// UAC/잠금화면이 뜨면 secure desktop 이 활성화되어 우리 프로세스는 SendInput/SetCursorPos 를
/// 실행해도 무시됨. Slave 가 Active 인 상태에서 이 상황을 감지하면 Master 에게
/// 자동으로 RETURN_CONTROL 을 보내 사용자를 갇힘 상태에서 자동 탈출시킴.
///
/// 감지 방식: `GetForegroundWindow` 가 NULL 을 반환하면 secure desktop 상태.
/// (일반 상태에선 항상 non-null. NULL 은 다른 desktop 이 활성이라는 뜻.)
fn foreground_watcher_loop(tx: TcpTx) {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    // 오탐 방지: 2회 연속 NULL 이어야 실제 secure desktop 으로 간주.
    let mut null_count = 0u32;
    loop {
        thread::sleep(Duration::from_millis(200));
        if SHARED.get() != SlaveState::Active {
            null_count = 0;
            continue;
        }
        let is_secure = unsafe {
            let hwnd = GetForegroundWindow();
            hwnd.0.is_null()
        };
        if is_secure {
            null_count += 1;
            if null_count >= 2 {
                tracing::warn!(
                    "slave: secure desktop 감지 (UAC/lock/etc) → 자동 RETURN_CONTROL"
                );
                return_control(&tx);
                null_count = 0;
            }
        } else {
            null_count = 0;
        }
    }
}

// -----------------------------------------------------------------------------
// 상태 전환 API
// -----------------------------------------------------------------------------

fn enter_active(entry_y_pct: u8, tx: &TcpTx) {
    if !SHARED.enabled.load(Ordering::Acquire) {
        // Slave 가 disabled 인 상태에서 Master 로부터 TAKE_CONTROL 을 받은 상황.
        // 이대로 무시하면 Master 는 커서 락 상태로 영구 갇힘 (사용자가 hotkey 로만 탈출 가능).
        // 즉시 RETURN_CONTROL 을 응답해서 Master 를 Local 로 복귀시킴.
        tracing::info!("slave: TAKE_CONTROL 수신했으나 disabled → RETURN_CONTROL 자동 응답");
        let _ = tx.try_send(Frame::new(MSG_RETURN_CONTROL));
        return;
    }

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
