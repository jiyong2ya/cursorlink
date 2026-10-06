// Master 오케스트레이션.
//
// 스레드:
//   - main         : Raw Input 메시지 펌프 (WM_INPUT 처리, 엣지 감지, UDP 송신)
//   - tcp_worker   : slave 에 TCP 연결, read/write 루프, HEARTBEAT
//   - hb_ticker    : 3초마다 HEARTBEAT 전송 요청
//
// 상태 전환:
//   Disconnected --TCP연결+HELLO OK--> Local
//   Local --커서 오른쪽 끝 도달--> Remote (커서 락 + 숨김 + TAKE_CONTROL 송신)
//   Remote --RETURN_CONTROL 수신--> Local (커서 언락 + 표시)
//   * --TCP 끊김--> Disconnected (Local 강제 복귀)

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Sender};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::net::tcp::{
    self, Frame, MSG_HEARTBEAT, MSG_HELLO, MSG_RETURN_CONTROL, MSG_TAKE_CONTROL,
};
use crate::state::{MasterShared, MasterState};

pub static SHARED: MasterShared = MasterShared::new();

/// Remote 진입 직전에 저장하는 Master 커서 위치. Local 복귀 시 여기로 되돌림.
/// i32::MIN 이면 "저장 안 됨" 센티넬 (첫 진입 이전 상태).
static SAVED_CURSOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);
static SAVED_CURSOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);

/// wnd_proc / edge 감지 에서 TCP 전송을 요청하기 위한 채널.
pub type TcpTx = Sender<Frame>;

pub struct MasterCtx {
    pub cfg: Config,
    pub tcp_tx: TcpTx,
}

pub fn run(cfg: Config) -> Result<()> {
    let (tx, rx) = bounded::<Frame>(64);

    // TCP worker 스레드. peer_ip 는 IP 또는 hostname (Windows 컴퓨터 이름 포함) 둘 다 지원.
    // 재접속마다 재해석해서 Slave IP 바뀌어도 자동 대응.
    let peer_target = format!("{}:{}", cfg.peer_ip, cfg.tcp_port);
    let secret = cfg.shared_secret.clone();
    let rx_clone = rx.clone();
    let tx_for_hb = tx.clone();

    thread::Builder::new()
        .name("cursorlink-tcp".to_string())
        .spawn(move || tcp_worker_loop(peer_target, secret, rx_clone))
        .context("TCP worker 스레드 시작 실패")?;

    // HEARTBEAT ticker
    thread::Builder::new()
        .name("cursorlink-hb".to_string())
        .spawn(move || heartbeat_loop(tx_for_hb))
        .context("HEARTBEAT 스레드 시작 실패")?;

    // 메인 스레드: 캡처 시작 (블로킹)
    crate::input::capture::run_with_ctx(MasterCtx { cfg, tcp_tx: tx })
}

fn tcp_worker_loop(peer_target: String, secret: String, rx: crossbeam_channel::Receiver<Frame>) {
    use std::net::ToSocketAddrs;
    loop {
        SHARED.set(MasterState::Disconnected);
        tracing::info!("master: TCP {} 로 연결 시도", peer_target);

        // hostname 이면 매번 DNS/NetBIOS 해석 (IP 바뀌어도 자동 대응).
        let peer: SocketAddr = match peer_target.to_socket_addrs() {
            Ok(mut iter) => match iter.next() {
                Some(a) => a,
                None => {
                    tracing::warn!("master: peer 주소 해석 결과 없음: {}", peer_target);
                    thread::sleep(Duration::from_secs(3));
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!("master: peer 주소 해석 실패 ({}): {}", peer_target, e);
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        match tcp::connect(peer) {
            Ok(mut sock) => {
                // HELLO 전송 + 확인 (slave 가 응답 없이 그냥 놔둬도 됨, 여기선 편의상 즉시 Local 전환)
                if let Err(e) = tcp::write_frame(&mut sock, Frame::hello(&secret)) {
                    tracing::warn!("master: HELLO 전송 실패: {}", e);
                    thread::sleep(Duration::from_secs(3));
                    continue;
                }
                SHARED.set(MasterState::Local);
                tracing::info!("master: TCP 연결 성공, Local 상태");
                run_connected(&mut sock, &rx);
                tracing::warn!("master: TCP 연결 해제됨, 재시도 예정");
                // Remote 상태였으면 커서 락/숨김이 걸려있음. 반드시 해제하지 않으면 마우스 얼어붙음.
                if SHARED.get() == MasterState::Remote {
                    crate::input::cursor::unlock();
                    crate::input::cursor::show();
                    tracing::info!("master: TCP 끊김으로 인한 Remote → Local 강제 복귀");
                }
                // Mirror 도 자동 off (Slave 와 연결 끊겼으니)
                if SHARED.mirror.load(Ordering::Acquire) {
                    toggle_mirror_off_internal();
                }
            }
            Err(e) => {
                tracing::debug!("master: TCP 연결 실패: {}", e);
            }
        }
        // 재연결 대기
        thread::sleep(Duration::from_secs(3));
    }
}

fn run_connected(sock: &mut TcpStream, rx: &crossbeam_channel::Receiver<Frame>) {
    // 별도 write worker 를 만드는 대신, TCP 는 read/write 를 번갈아 처리.
    // read 는 6초 타임아웃 → 타임아웃 시 채널에서 write 처리 후 재시도.
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

    // reader 루프
    // read timeout (tcp::configure_stream 에서 6초로 설정됨) 은 주기적 wake-up 용도.
    // WouldBlock/TimedOut 은 정상 idle 상태이므로 continue. 진짜 연결 문제만 break.
    loop {
        match tcp::read_frame(sock) {
            Ok(f) => handle_incoming(f),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                tracing::warn!("master: TCP read 오류: {}", e);
                break;
            }
        }
    }

    // reader 종료 → writer 도 종료 유도 (dummy send 로 채널 유지, 여기선 그냥 join 대기)
    // sock 이 close 되면 writer 의 write_all 도 에러날 것.
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
}

fn tcp_writer_loop(mut sock: TcpStream, rx: crossbeam_channel::Receiver<Frame>) {
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(f) => {
                if let Err(e) = tcp::write_frame(&mut sock, f) {
                    tracing::warn!("master: TCP write 오류: {}", e);
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
        MSG_RETURN_CONTROL => {
            tracing::info!("master: RETURN_CONTROL 수신 → Local 로 복귀");
            return_to_local();
        }
        MSG_HEARTBEAT => {
            tracing::trace!("master: HEARTBEAT 수신");
        }
        MSG_HELLO => {
            // Master 는 HELLO 를 받을 이유 없음. 무시.
            tracing::debug!("master: 알수없는 HELLO 수신");
        }
        MSG_TAKE_CONTROL => {
            tracing::debug!("master: 자기 자신이 보낸 TAKE_CONTROL echo?");
        }
        other => {
            tracing::warn!("master: 알 수 없는 msg_type=0x{:02x}", other);
        }
    }
}

fn heartbeat_loop(tx: TcpTx) {
    loop {
        thread::sleep(Duration::from_secs(3));
        // Disconnected 면 어차피 receiver 가 없음 (모두 있음, 그냥 무해).
        let _ = tx.try_send(Frame::new(MSG_HEARTBEAT));
    }
}

// -----------------------------------------------------------------------------
// 상태 전환 API (capture.rs 에서 호출)
// -----------------------------------------------------------------------------

/// 엣지 크로싱 (커서가 오른쪽 끝 도달) 시 호출. Local → Remote 전환. Slave 는 왼쪽 벽에서 진입.
pub fn transfer_to_remote(tx: &TcpTx, entry_y_pct: u8) {
    transfer_to_remote_impl(tx, tcp::SIDE_LEFT, entry_y_pct)
}

/// hotkey_transfer 로 즉시 전환. Slave 커서를 화면 중앙에 놓음.
pub fn transfer_to_remote_center(tx: &TcpTx) {
    transfer_to_remote_impl(tx, tcp::SIDE_CENTER, 50)
}

fn transfer_to_remote_impl(tx: &TcpTx, entry_side: u8, entry_y_pct: u8) {
    if !SHARED.enabled.load(Ordering::Acquire) { return; }
    if SHARED.get() != MasterState::Local { return; }

    let _ = tx.try_send(Frame::take_control(entry_side, entry_y_pct));

    // Remote 진입 직전 커서 위치 저장 → Local 복귀 시 여기로 되돌림.
    if let Some(pt) = crate::input::cursor::get_pos() {
        SAVED_CURSOR_X.store(pt.x, Ordering::Release);
        SAVED_CURSOR_Y.store(pt.y, Ordering::Release);
    }

    // Remote 진입 시 LL 훅 install (Master 앱에 입력 안 가게 소비).
    unsafe {
        if let Err(e) = crate::input::hooks::install() {
            tracing::warn!("master: 훅 install 실패, 이중 입력 발생 가능: {}", e);
        }
    }

    crate::input::cursor::lock_center();
    crate::input::cursor::hide();
    SHARED.set(MasterState::Remote);
    tracing::info!("master: Local → Remote (side={}, y_pct={})", entry_side, entry_y_pct);
}

/// slave 로부터 RETURN_CONTROL 수신 시 호출. Remote → Local 로 복귀.
pub fn return_to_local() {
    if SHARED.get() != MasterState::Remote { return; }
    crate::input::cursor::unlock();
    crate::input::cursor::show();
    // Local 복귀 시 LL 훅 uninstall (게임 anti-cheat 감지 대상 제거).
    unsafe { crate::input::hooks::uninstall(); }

    // 저장된 커서 위치로 복원 (Remote 진입 직전 위치).
    let sx = SAVED_CURSOR_X.load(Ordering::Acquire);
    let sy = SAVED_CURSOR_Y.load(Ordering::Acquire);
    if sx != i32::MIN && sy != i32::MIN {
        crate::input::cursor::set_pos(sx, sy);
        tracing::debug!("master: 커서 위치 복원 ({}, {})", sx, sy);
    }

    SHARED.set(MasterState::Local);
    tracing::info!("master: Remote → Local");
}

/// hotkey_return (LL 훅에서 감지) 에서 호출. Master 로 즉시 복귀 + Slave 에 RETURN_CONTROL 전송.
/// return_to_local 과 달리 tx 통해 Slave 에도 알림.
pub fn force_return_to_local() {
    if SHARED.get() != MasterState::Remote { return; }
    // 이 함수는 LL 훅 스레드에서 호출됨. G_TCP_TX 는 capture.rs 의 static 이라 unsafe 로 접근.
    unsafe {
        if let Some(tx) = crate::input::capture::tcp_tx_ref() {
            let _ = tx.try_send(Frame::new(MSG_RETURN_CONTROL));
        }
    }
    return_to_local();
}

/// 단축키로 기능 완전 off. Remote 였다면 즉시 Local 복귀.
/// tcp_tx 가 있으면 Mirror OFF 시 RETURN_CONTROL 도 Slave 에 전송.
pub fn set_enabled(en: bool, tcp_tx: Option<&TcpTx>) {
    let prev = SHARED.enabled.swap(en, Ordering::AcqRel);
    if prev && !en && SHARED.get() == MasterState::Remote {
        return_to_local();
    }
    // 비활성화 시 Mirror 도 자동 off + Slave 에 RETURN_CONTROL 전송
    if !en && SHARED.mirror.load(Ordering::Acquire) {
        if let Some(tx) = tcp_tx {
            let _ = tx.try_send(Frame::new(MSG_RETURN_CONTROL));
        }
        toggle_mirror_off_internal();
    }
    tracing::info!("master: enabled={}", en);
}

/// Mirror 모드 토글. Local 상태에서만 켜짐.
/// - on: Slave 를 Active 상태로 만들기 위해 TAKE_CONTROL 전송, 커서 락은 하지 않음.
/// - off: RETURN_CONTROL 전송, Slave 를 Idle 로 되돌림.
pub fn toggle_mirror(tx: &TcpTx) {
    if !SHARED.enabled.load(Ordering::Acquire) {
        tracing::debug!("master: enabled=false 라 mirror 토글 무시");
        return;
    }
    let now = SHARED.mirror.load(Ordering::Acquire);
    if !now {
        // 켜기: Local 상태에서만
        if SHARED.get() != MasterState::Local {
            tracing::warn!("master: Local 상태 아니라 mirror 켤 수 없음");
            return;
        }
        let _ = tx.try_send(Frame::take_control(tcp::SIDE_LEFT, 50));
        // Mirror 도 훅이 필요 (키보드 forward 를 위해). 소비는 안 함.
        unsafe {
            if let Err(e) = crate::input::hooks::install() {
                tracing::warn!("master: mirror 훅 install 실패: {}", e);
            }
        }
        SHARED.mirror.store(true, Ordering::Release);
        tracing::info!("master: Mirror ON");
    } else {
        toggle_mirror_off_internal();
        let _ = tx.try_send(Frame::new(MSG_RETURN_CONTROL));
    }
}

/// Mirror off 처리 (TCP 전송 없이 상태만). Remote 아닌 경우 훅도 uninstall.
fn toggle_mirror_off_internal() {
    SHARED.mirror.store(false, Ordering::Release);
    // Remote 상태가 아니면 훅도 uninstall (Local 로 완전 복귀).
    if SHARED.get() != MasterState::Remote {
        unsafe { crate::input::hooks::uninstall(); }
    }
    tracing::info!("master: Mirror OFF");
}
