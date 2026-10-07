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
//   Active --복귀 벽 도달--> Idle (RETURN_CONTROL 송신 + 커서 숨김)
//     복귀 벽은 마스터가 TAKE_CONTROL 로 알려줌: 오른쪽 슬레이브 = 왼쪽 벽, 왼쪽 슬레이브 = 오른쪽 벽.
//     그래서 슬레이브 config 엔 자기 위치 설정이 없음.
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
    let hk_spec = HotkeySpec { toggle: cfg.hotkey_toggle.clone() };
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
                        // 새 마스터 세션 (마스터 재시작 포함) → UDP seq 가 1 부터 다시 시작하므로 리셋.
                        crate::input::inject::reset_seq();
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

    // reader: 연결 오류 or 마스터 하트비트 끊김 (마스터 절전/재부팅 등) 까지 블로킹.
    tcp::read_loop(sock, "slave:", |f| handle_incoming(f, tx));

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
            let entry_side = f.payload_byte(0);
            let entry_y_pct = f.payload_byte(1);
            // 구버전 마스터는 0 (= 왼쪽 벽 복귀) 을 보냄 → 기존 동작 그대로.
            let return_side = f.payload_byte(2);
            tracing::info!(
                "slave: TAKE_CONTROL 수신 (side={}, y_pct={}, return={}) → Active",
                entry_side, entry_y_pct, return_side
            );
            enter_active(entry_side, entry_y_pct, return_side, tx);
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

/// Secure desktop (UAC 확인창, 잠금화면, Ctrl+Alt+Del 등) 감지용 watchdog.
///
/// UAC/잠금화면이 뜨면 입력이 별도 데스크톱 (Winlogon) 으로 전환되어 우리 프로세스의
/// SendInput/SetCursorPos 가 안 먹힘. Slave 가 Active 인 상태에서 이걸 감지하면 Master 에게
/// 자동으로 RETURN_CONTROL (이유 = 보안 화면) 을 보내 갇힘 상태에서 자동 탈출시킴.
fn foreground_watcher_loop(tx: TcpTx) {
    // 오탐 방지: 2회 연속 (0.4초) 이어야 실제 보안 화면으로 간주.
    let mut hits = 0u32;
    loop {
        thread::sleep(Duration::from_millis(200));
        if SHARED.get() != SlaveState::Active {
            hits = 0;
            continue;
        }
        if input_desktop_is_secure() {
            hits += 1;
            if hits >= 2 {
                tracing::warn!("slave: 보안 화면 감지 (UAC 확인창/잠금화면 등) → 마스터로 자동 복귀");
                return_control_because(&tx, tcp::RETURN_SECURE_DESKTOP);
                hits = 0;
            }
        } else {
            hits = 0;
        }
    }
}

/// 지금 입력을 받는 데스크톱이 일반 데스크톱 (Default) 이 아니면 true.
/// UAC 확인창 / 잠금화면 / Ctrl+Alt+Del 은 Winlogon (보안) 데스크톱으로 전환되는데,
/// 일반/관리자 권한 프로세스는 그 데스크톱을 열 수 없어서 OpenInputDesktop 이 실패함.
/// (예전 방식 GetForegroundWindow == NULL 은 UAC 때 NULL 이 안 나와서 놓쳤음)
fn input_desktop_is_secure() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop,
        DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, UOI_NAME,
    };
    unsafe {
        let desk = match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(d) => d,
            Err(_) => return true, // 못 열면 보안 데스크톱
        };
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr() as *mut core::ffi::c_void),
            (name.len() * 2) as u32,
            Some(&mut needed),
        ).is_ok();
        let _ = CloseDesktop(desk);
        if !ok {
            return false;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        !String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("Default")
    }
}

// -----------------------------------------------------------------------------
// 상태 전환 API
// -----------------------------------------------------------------------------

fn enter_active(entry_side: u8, entry_y_pct: u8, return_side: u8, tx: &TcpTx) {
    if !SHARED.enabled.load(Ordering::Acquire) {
        tracing::info!("slave: TAKE_CONTROL 수신했으나 disabled → RETURN_CONTROL 자동 응답");
        let _ = tx.try_send(Frame::return_control(tcp::RETURN_DISABLED));
        return;
    }

    let scr = cursor::primary_screen();
    let ey = scr.top + (scr.height() as i64 * entry_y_pct as i64 / 100) as i32;
    let (entry_x, entry_y) = match entry_side {
        // 단축키 / 미러로 진입: 화면 중앙.
        tcp::SIDE_CENTER => (scr.left + scr.width() / 2, scr.top + scr.height() / 2),
        // 엣지 크로싱 진입 (왼쪽 슬레이브): 오른쪽 벽 10px 안쪽 + 지정된 세로 비율.
        tcp::SIDE_RIGHT => (scr.right - 11, ey),
        // 엣지 크로싱 진입 (오른쪽 슬레이브): 왼쪽 벽 10px 안쪽 + 지정된 세로 비율.
        _ => (scr.left + 10, ey),
    };

    let rs = if return_side == tcp::SIDE_RIGHT { tcp::SIDE_RIGHT } else { tcp::SIDE_LEFT };
    SHARED.return_side.store(rs, Ordering::Release);

    cursor::set_pos(entry_x, entry_y);
    // 이전 세션의 낡은 X 좌표로 진입 직후 바로 복귀되는 것 방지.
    crate::input::inject::reset_edge_baseline(entry_x);
    cursor::show();
    SHARED.set(SlaveState::Active);
}

/// 복귀 벽 (왼쪽 or 오른쪽) 도달 시 호출. Active → Idle.
pub fn return_control(tx: &TcpTx) {
    return_control_because(tx, tcp::RETURN_NORMAL);
}

/// Active → Idle + 마스터에 복귀 이유와 함께 RETURN_CONTROL.
fn return_control_because(tx: &TcpTx, reason: u8) {
    if SHARED.get() != SlaveState::Active { return; }
    let _ = tx.try_send(Frame::return_control(reason));
    cursor::hide();
    SHARED.set(SlaveState::Idle);
    tracing::info!("slave: Active → Idle (reason={})", reason);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 사용자 세션의 일반 화면에서 돌릴 때만 의미 있음 (서비스/CI 에선 다를 수 있음) → 수동 실행:
    /// cargo test input_desktop -- --ignored
    #[test]
    #[ignore]
    fn input_desktop_is_default_in_normal_session() {
        assert!(!input_desktop_is_secure());
    }
}
