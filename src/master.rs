// Master 오케스트레이션.
//
// 배치: [왼쪽 슬레이브] ← [마스터] → [오른쪽 슬레이브]. 슬레이브는 한쪽만 있어도 됨.
//
// 스레드:
//   - main             : Raw Input 메시지 펌프 (WM_INPUT, 엣지 감지, UDP 송신) + 모든 상태 전환
//   - tcp-left/right   : 슬레이브별 TCP 연결/재연결 + reader (+ 연결마다 writer 스레드)
//   - hb               : 3초마다 연결된 슬레이브에 HEARTBEAT
//
// 상태 전환 (커서 락/숨김, 훅 install/uninstall) 은 전부 메인 스레드에서만 한다.
// TCP 스레드는 PostMessage 로 메인 스레드에 이벤트 (UP / DOWN / RETURN) 만 넘김.
//
//   Local --화면 오른쪽 끝 or hotkey_transfer------> Remote(오른쪽)
//   Local --화면 왼쪽 끝 or hotkey_transfer_left----> Remote(왼쪽)
//   Remote(A) --반대쪽 단축키--> Remote(B)   (A 에 RETURN_CONTROL, B 에 TAKE_CONTROL)
//   Remote --슬레이브 RETURN_CONTROL / hotkey_return / 연결 끊김--> Local (커서 원위치)
//
// Mirror (Local 에서만): 마스터 + 미러 대상 슬레이브 동시 조작.
//   미러 중엔 왼쪽/오른쪽 단축키가 그 슬레이브를 미러 대상에 넣기/빼기로 바뀜.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::config::Config;
use crate::input::hooks::HookAction;
use crate::net::tcp::{
    self, Frame, MSG_HEARTBEAT, MSG_HELLO, MSG_RETURN_CONTROL, MSG_TAKE_CONTROL,
};
use crate::net::udp;
use crate::state::{MasterShared, MasterState, Side};
use crate::tray::MenuItem;

pub static SHARED: MasterShared = MasterShared::new();

/// TCP 스레드 → 메인 스레드 이벤트. wparam = Side.
pub const WM_PEER_UP: u32 = WM_APP + 2;
pub const WM_PEER_DOWN: u32 = WM_APP + 3;
pub const WM_PEER_RETURN: u32 = WM_APP + 4;
/// 미뤄둔 트레이 알림 표시.
pub const WM_SHOW_NOTE: u32 = WM_APP + 5;

static PENDING_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Remote 진입 직전에 저장하는 Master 커서 위치. Local 복귀 시 여기로 되돌림.
/// i32::MIN 이면 "저장 안 됨" 센티넬 (첫 진입 이전 상태).
static SAVED_CURSOR_X: AtomicI32 = AtomicI32::new(i32::MIN);
static SAVED_CURSOR_Y: AtomicI32 = AtomicI32::new(i32::MIN);

/// 메인 스레드 message window (PostMessage 대상 + 트레이 알림). 0 = 아직 없음.
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub type TcpTx = Sender<Frame>;

/// 설정된 슬레이브 하나와의 연결 통로.
struct PeerLink {
    /// config 에 적힌 주소 (IP 또는 hostname)
    target: String,
    tcp_tx: TcpTx,
    /// TCP 연결 시점에 실제 IP 로 connect 됨.
    udp: UdpSocket,
}

/// Side::idx 로 인덱싱. 설정 안 된 쪽은 None.
static LINKS: OnceLock<[Option<PeerLink>; 2]> = OnceLock::new();

fn link(side: Side) -> Option<&'static PeerLink> {
    LINKS.get().and_then(|l| l[side.idx()].as_ref())
}

pub fn run(cfg: Config) -> Result<()> {
    let targets = [cfg.left_peer_ip.trim().to_string(), cfg.peer_ip.trim().to_string()];
    if targets.iter().all(|t| t.is_empty()) {
        anyhow::bail!("슬레이브 주소가 없습니다 (peer_ip / left_peer_ip 중 하나는 필요)");
    }
    SHARED.edge_enabled[Side::Left.idx()].store(cfg.edge_switch_left, Ordering::Release);
    SHARED.edge_enabled[Side::Right.idx()].store(cfg.edge_switch_right, Ordering::Release);

    let mut links: [Option<PeerLink>; 2] = [None, None];
    let mut workers = Vec::new();
    for side in Side::ALL {
        let target = &targets[side.idx()];
        if target.is_empty() { continue; }
        let (tx, rx) = bounded::<Frame>(64);
        links[side.idx()] = Some(PeerLink {
            target: target.clone(),
            tcp_tx: tx,
            udp: udp::bind_send()?,
        });
        // peer 는 IP 또는 hostname (Windows 컴퓨터 이름 포함) 둘 다 지원.
        // 재접속마다 재해석해서 Slave IP 바뀌어도 자동 대응.
        workers.push((side, format!("{}:{}", target, cfg.tcp_port), rx));
    }
    let _ = LINKS.set(links);

    for (side, peer_target, rx) in workers {
        let secret = cfg.shared_secret.clone();
        let udp_port = cfg.udp_port;
        thread::Builder::new()
            .name(format!("cursorlink-tcp-{}", side.tag()))
            .spawn(move || tcp_worker_loop(side, peer_target, secret, udp_port, rx))
            .context("TCP worker 스레드 시작 실패")?;
    }

    thread::Builder::new()
        .name("cursorlink-hb".to_string())
        .spawn(heartbeat_loop)
        .context("HEARTBEAT 스레드 시작 실패")?;

    // 메인 스레드: 캡처 시작 (블로킹)
    crate::input::capture::run(cfg)
}

fn tcp_worker_loop(side: Side, peer_target: String, secret: String, udp_port: u16, rx: Receiver<Frame>) {
    use std::net::ToSocketAddrs;
    let name = side.label();
    // 슬레이브가 꺼져 있으면 3초마다 재시도 → 로그 안 쌓이게 연속 실패는 debug 로.
    let mut quiet = false;
    loop {
        if !quiet {
            tracing::info!("master: [{}] TCP {} 로 연결 시도", name, peer_target);
        }

        // hostname 이면 매번 DNS/NetBIOS 해석. UDP 소켓이 IPv4 라 IPv4 주소만 씀.
        let peer: Option<SocketAddr> = match peer_target.to_socket_addrs() {
            Ok(mut iter) => iter.find(|a| a.is_ipv4()),
            Err(e) => {
                if !quiet { tracing::warn!("master: [{}] 주소 해석 실패 ({}): {}", name, peer_target, e); }
                None
            }
        };

        if let Some(peer) = peer {
            match tcp::connect(peer) {
                Ok(mut sock) => {
                    if let Err(e) = tcp::write_frame(&mut sock, Frame::hello(&secret)) {
                        tracing::warn!("master: [{}] HELLO 전송 실패: {}", name, e);
                    } else {
                        if let Some(l) = link(side) {
                            if let Err(e) = l.udp.connect(SocketAddr::new(peer.ip(), udp_port)) {
                                tracing::warn!("master: [{}] UDP connect 실패: {}", name, e);
                            }
                        }
                        // 끊겨 있던 동안 쌓인 프레임 (하트비트 등) 은 버림.
                        while rx.try_recv().is_ok() {}
                        SHARED.connected[side.idx()].store(true, Ordering::Release);
                        tracing::info!("master: [{}] TCP 연결 성공 ({})", name, peer);
                        post_to_main(WM_PEER_UP, side);

                        run_connected(side, &mut sock, &rx);

                        SHARED.connected[side.idx()].store(false, Ordering::Release);
                        tracing::warn!("master: [{}] TCP 연결 해제됨, 재시도 예정", name);
                        // Remote 였으면 커서 락/숨김 해제는 메인 스레드가 처리.
                        post_to_main(WM_PEER_DOWN, side);
                        quiet = false;
                        thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                }
                Err(e) => {
                    if !quiet {
                        tracing::info!("master: [{}] 연결 실패, 3초마다 재시도: {}", name, e);
                    } else {
                        tracing::debug!("master: [{}] TCP 연결 실패: {}", name, e);
                    }
                }
            }
        } else if !quiet {
            tracing::warn!("master: [{}] peer 주소 해석 결과 없음: {}", name, peer_target);
        }
        quiet = true;
        // 재연결 대기
        thread::sleep(Duration::from_secs(3));
    }
}

fn run_connected(side: Side, sock: &mut TcpStream, rx: &Receiver<Frame>) {
    // 별도 write worker 스레드 + 이 스레드는 reader.
    let write_sock = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => { tracing::warn!("try_clone 실패: {}", e); return; }
    };
    let rx_clone = rx.clone();
    let writer = thread::Builder::new()
        .name(format!("cursorlink-tcp-w-{}", side.tag()))
        .spawn(move || tcp_writer_loop(write_sock, rx_clone));
    let writer = match writer {
        Ok(h) => h,
        Err(e) => { tracing::warn!("writer 스레드 실패: {}", e); return; }
    };

    // reader: 연결 오류 or 슬레이브 하트비트 끊김 (노트북 절전 등) 까지 블로킹.
    let tag = format!("master: [{}]", side.label());
    tcp::read_loop(sock, &tag, |f| handle_incoming(side, f));

    // sock 이 close 되면 writer 의 write_all 도 에러나서 종료됨.
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
}

fn tcp_writer_loop(mut sock: TcpStream, rx: Receiver<Frame>) {
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

fn handle_incoming(side: Side, f: Frame) {
    match f.msg_type() {
        MSG_RETURN_CONTROL => {
            tracing::info!("master: [{}] RETURN_CONTROL 수신", side.label());
            post_to_main(WM_PEER_RETURN, side);
        }
        MSG_HEARTBEAT => {
            tracing::trace!("master: [{}] HEARTBEAT 수신", side.label());
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

fn heartbeat_loop() {
    loop {
        thread::sleep(Duration::from_secs(3));
        for side in Side::ALL {
            if SHARED.is_connected(side) {
                send_tcp(side, Frame::new(MSG_HEARTBEAT));
            }
        }
    }
}

// -----------------------------------------------------------------------------
// 송신 / 메인 스레드 연결
// -----------------------------------------------------------------------------

fn send_tcp(side: Side, f: Frame) {
    if let Some(l) = link(side) {
        let _ = l.tcp_tx.try_send(f);
    }
}

/// 특정 슬레이브에 UDP 패킷 송신 (capture.rs 의 release_held 등).
pub fn send_udp(side: Side, bytes: &[u8]) {
    if let Some(l) = link(side) {
        let _ = l.udp.send(bytes);
    }
}

/// 입력 패킷 라우팅: Mirror 면 미러 대상 전부, Remote 면 제어 중인 슬레이브.
pub fn route_packet(bytes: &[u8]) {
    if SHARED.mirror.load(Ordering::Acquire) {
        for side in live_mirror_targets() {
            send_udp(side, bytes);
        }
    } else if SHARED.get() == MasterState::Remote {
        send_udp(SHARED.active(), bytes);
    }
}

pub fn set_main_hwnd(hwnd: HWND) {
    MAIN_HWND.store(hwnd.0 as isize, Ordering::Release);
}

fn main_hwnd() -> Option<HWND> {
    match MAIN_HWND.load(Ordering::Acquire) {
        0 => None,
        raw => Some(HWND(raw as *mut _)),
    }
}

fn post_to_main(msg: u32, side: Side) {
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            let _ = PostMessageW(hwnd, msg, WPARAM(side as usize), LPARAM(0));
        }
    }
}

/// 트레이 알림 (토스트). LL 훅 콜백 안에서도 불리므로 바로 띄우지 않고
/// 메인 스레드 메시지로 미룸 (Shell_NotifyIcon 이 explorer 와 동기 통신 → 훅 타임아웃 위험).
fn notify(msg: &str) {
    if let Some(hwnd) = main_hwnd() {
        *PENDING_NOTE.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.to_string());
        unsafe {
            let _ = PostMessageW(hwnd, WM_SHOW_NOTE, WPARAM(0), LPARAM(0));
        }
    }
}

/// WM_SHOW_NOTE 처리 (capture.rs wnd_proc). 마지막 알림만 표시.
pub fn show_pending_note() {
    let note = PENDING_NOTE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let (Some(msg), Some(hwnd)) = (note, main_hwnd()) {
        crate::tray::notify(hwnd, "cursorlink", &msg);
    }
}

/// 슬레이브 화면에서 마스터가 있는 쪽 벽. 진입 (엣지 크로싱) 과 복귀 모두 이 벽 기준.
fn master_wall(side: Side) -> u8 {
    match side {
        Side::Right => tcp::SIDE_LEFT,
        Side::Left  => tcp::SIDE_RIGHT,
    }
}

/// 단축키 대상 슬레이브가 쓸 수 있는 상태인지. 아니면 알림.
fn check_peer(side: Side) -> bool {
    if link(side).is_none() {
        notify(&format!("{} 슬레이브가 설정에 없음", side.label()));
        false
    } else if !SHARED.is_connected(side) {
        notify(&format!("{} 슬레이브 연결 안 됨", side.label()));
        false
    } else {
        true
    }
}

// -----------------------------------------------------------------------------
// 상태 전환 API (전부 메인 스레드에서 호출: wnd_proc / LL 훅)
// -----------------------------------------------------------------------------

/// 엣지 크로싱 (capture.rs). 오른쪽 끝 → 오른쪽 슬레이브, 왼쪽 끝 → 왼쪽 슬레이브.
/// 쓸어넘기기 on/off 는 호출 전에 capture.rs 가 확인.
pub fn transfer_by_edge(side: Side, entry_y_pct: u8) {
    transfer_to_remote(side, master_wall(side), entry_y_pct);
}

/// 왼쪽/오른쪽 단축키 (RegisterHotKey 또는 Remote 중 LL 훅).
///   Local          → 그 슬레이브로 전환 (커서는 화면 중앙)
///   Local + Mirror → 그 슬레이브를 미러 대상에 넣기/빼기
///   Remote         → 그 슬레이브로 바로 전환 (마스터 안 거침)
pub fn on_side_hotkey(side: Side) {
    if !SHARED.enabled.load(Ordering::Acquire) { return; }
    if SHARED.mirror.load(Ordering::Acquire) {
        toggle_mirror_target(side);
        return;
    }
    match SHARED.get() {
        MasterState::Local => {
            if check_peer(side) {
                transfer_to_remote(side, tcp::SIDE_CENTER, 50);
            }
        }
        MasterState::Remote => switch_remote(side),
    }
}

/// Remote 중 LL 훅이 감지한 단축키.
pub fn on_hook_hotkey(action: HookAction) {
    match action {
        HookAction::Return => force_return_to_local(),
        HookAction::Left   => on_side_hotkey(Side::Left),
        HookAction::Right  => on_side_hotkey(Side::Right),
        HookAction::Edge   => toggle_edge(),
        HookAction::EdgeLeft  => toggle_edge_side(Side::Left),
        HookAction::EdgeRight => toggle_edge_side(Side::Right),
        HookAction::Toggle => toggle_enabled(),
        HookAction::Mirror => {}
    }
}

fn transfer_to_remote(side: Side, entry_side: u8, entry_y_pct: u8) {
    if !SHARED.enabled.load(Ordering::Acquire) { return; }
    if SHARED.get() != MasterState::Local || SHARED.mirror.load(Ordering::Acquire) { return; }
    if !SHARED.is_connected(side) { return; }

    send_tcp(side, Frame::take_control(entry_side, entry_y_pct, master_wall(side)));

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
    SHARED.set_active(side);
    SHARED.set(MasterState::Remote);
    tracing::info!("master: Local → Remote({}) (entry={}, y_pct={})", side.label(), entry_side, entry_y_pct);
}

/// Remote 상태에서 다른 슬레이브로 바로 이동. 커서 락/훅은 그대로 유지.
fn switch_remote(to: Side) {
    let from = SHARED.active();
    if from == to { return; }
    if !check_peer(to) { return; }

    // 이전 슬레이브에 눌린 채 남은 키/버튼 떼고 Idle 로.
    crate::input::capture::release_held(from);
    send_tcp(from, Frame::new(MSG_RETURN_CONTROL));
    send_tcp(to, Frame::take_control(tcp::SIDE_CENTER, 50, master_wall(to)));
    SHARED.set_active(to);
    tracing::info!("master: Remote({}) → Remote({})", from.label(), to.label());
}

/// Remote → Local. notify_slave: 슬레이브에 RETURN_CONTROL 을 보낼지
/// (슬레이브가 먼저 RETURN_CONTROL 보낸 경우 / 연결 끊긴 경우는 false).
fn leave_remote(notify_slave: bool) {
    if SHARED.get() != MasterState::Remote { return; }
    let side = SHARED.active();

    crate::input::capture::release_held(side);
    if notify_slave {
        send_tcp(side, Frame::new(MSG_RETURN_CONTROL));
    }

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

    crate::input::capture::clear_held();
    SHARED.set(MasterState::Local);
    tracing::info!("master: Remote({}) → Local", side.label());
}

/// hotkey_return (LL 훅에서 감지). Master 로 즉시 복귀 + Slave 에 RETURN_CONTROL 전송.
pub fn force_return_to_local() {
    leave_remote(true);
}

/// 슬레이브가 RETURN_CONTROL 보냄 (화면 끝 도달 / UAC 감지 / 슬레이브 disabled).
pub fn on_peer_return(side: Side) {
    if SHARED.get() == MasterState::Remote && SHARED.active() == side {
        leave_remote(false);
    } else if SHARED.mirror.load(Ordering::Acquire) && SHARED.is_mirror_target(side) {
        // 미러 중 슬레이브가 스스로 빠짐 (UAC/잠금화면 or 슬레이브 disabled).
        // 다시 넣으려면 그 쪽 단축키를 두 번 (빼기 → 넣기).
        tracing::warn!("master: 미러 중 {} 슬레이브가 RETURN_CONTROL → 그 슬레이브는 미러 안 됨", side.label());
    } else {
        tracing::debug!("master: {} 슬레이브 RETURN_CONTROL 무시 (제어 중 아님)", side.label());
    }
}

/// 슬레이브 TCP 연결됨.
pub fn on_peer_up(side: Side) {
    // 미러 중에 미러 대상이 (재)연결되면 바로 미러에 합류.
    if SHARED.mirror.load(Ordering::Acquire) && SHARED.is_mirror_target(side) {
        send_tcp(side, Frame::take_control(tcp::SIDE_CENTER, 50, master_wall(side)));
        tracing::info!("master: {} 슬레이브 연결 → 미러 합류", side.label());
    }
}

/// 슬레이브 TCP 끊김. 그 슬레이브를 제어 중이었으면 커서 락/숨김을 반드시 풀어야 함.
pub fn on_peer_down(side: Side) {
    if SHARED.get() == MasterState::Remote && SHARED.active() == side {
        tracing::info!("master: {} 슬레이브 연결 끊김 → Local 강제 복귀", side.label());
        leave_remote(false);
        notify(&format!("{} 슬레이브 연결 끊김", side.label()));
    }
    // 미러할 슬레이브가 하나도 안 남으면 미러 off.
    if SHARED.mirror.load(Ordering::Acquire) && live_mirror_targets().is_empty() {
        mirror_off();
    }
}

pub fn toggle_enabled() {
    let cur = SHARED.enabled.load(Ordering::Acquire);
    set_enabled(!cur);
}

/// 단축키로 기능 완전 off. Remote 였다면 즉시 Local 복귀, Mirror 였다면 Mirror off.
pub fn set_enabled(en: bool) {
    let prev = SHARED.enabled.swap(en, Ordering::AcqRel);
    if prev && !en {
        if SHARED.get() == MasterState::Remote {
            leave_remote(true);
        }
        if SHARED.mirror.load(Ordering::Acquire) {
            mirror_off();
        }
    }
    tracing::info!("master: enabled={}", en);
    notify(if en { "cursorlink 켜짐" } else { "cursorlink 꺼짐" });
}

/// 쓸어넘기기 (마스터 화면 끝 → 슬레이브) 양쪽 한번에 on/off.
/// 설정된 쪽 중 하나라도 켜져 있으면 양쪽 다 끄고, 다 꺼져 있으면 양쪽 다 켬.
/// 슬레이브 → 마스터 복귀는 항상 동작.
pub fn toggle_edge() {
    let any_on = Side::ALL.into_iter().any(|s| link(s).is_some() && SHARED.is_edge_enabled(s));
    for s in Side::ALL {
        SHARED.edge_enabled[s.idx()].store(!any_on, Ordering::Release);
    }
    edge_changed();
}

/// 한쪽 쓸어넘기기만 on/off.
pub fn toggle_edge_side(side: Side) {
    if link(side).is_none() {
        notify(&format!("{} 슬레이브가 설정에 없음", side.label()));
        return;
    }
    let on = !SHARED.is_edge_enabled(side);
    SHARED.edge_enabled[side.idx()].store(on, Ordering::Release);
    edge_changed();
}

fn edge_changed() {
    // "왼쪽 ON / 오른쪽 OFF" (설정된 쪽만)
    let desc = Side::ALL.into_iter()
        .filter(|s| link(*s).is_some())
        .map(|s| format!("{} {}", s.label(), if SHARED.is_edge_enabled(s) { "ON" } else { "OFF" }))
        .collect::<Vec<_>>()
        .join(" / ");
    tracing::info!("master: 쓸어넘기기 ({})", desc);
    notify(&format!("쓸어넘기기 — {}", desc));
}

// -----------------------------------------------------------------------------
// Mirror
// -----------------------------------------------------------------------------

/// 미러 대상 중 지금 연결된 슬레이브.
fn live_mirror_targets() -> Vec<Side> {
    Side::ALL.into_iter()
        .filter(|s| link(*s).is_some() && SHARED.is_mirror_target(*s) && SHARED.is_connected(*s))
        .collect()
}

/// "왼쪽 ON / 오른쪽 OFF" (설정된 쪽만)
fn mirror_desc() -> String {
    Side::ALL.into_iter()
        .filter(|s| link(*s).is_some())
        .map(|s| format!("{} {}", s.label(), if SHARED.is_mirror_target(s) { "ON" } else { "OFF" }))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Mirror 모드 토글. Local 상태에서만 켜짐.
/// - on: 미러 대상 슬레이브를 Active 로 (TAKE_CONTROL), 커서 락은 하지 않음.
/// - off: 미러 대상에 RETURN_CONTROL → Idle.
pub fn toggle_mirror() {
    if !SHARED.enabled.load(Ordering::Acquire) {
        tracing::debug!("master: enabled=false 라 mirror 토글 무시");
        return;
    }
    if SHARED.mirror.load(Ordering::Acquire) {
        mirror_off();
        return;
    }
    if SHARED.get() != MasterState::Local {
        tracing::warn!("master: Local 상태 아니라 mirror 켤 수 없음");
        return;
    }
    // 대상을 전부 빼놓은 상태면 양쪽으로 리셋 (빈 미러로 켜지는 것 방지).
    if Side::ALL.into_iter().all(|s| !SHARED.is_mirror_target(s)) {
        for s in Side::ALL {
            SHARED.mirror_targets[s.idx()].store(true, Ordering::Release);
        }
    }
    let live = live_mirror_targets();
    if live.is_empty() {
        notify("미러: 연결된 대상 슬레이브 없음");
        return;
    }
    // Mirror 도 훅이 필요 (키보드 forward 를 위해). 소비는 안 함.
    unsafe {
        if let Err(e) = crate::input::hooks::install() {
            tracing::warn!("master: mirror 훅 install 실패: {}", e);
        }
    }
    SHARED.mirror.store(true, Ordering::Release);
    for side in live {
        send_tcp(side, Frame::take_control(tcp::SIDE_CENTER, 50, master_wall(side)));
    }
    tracing::info!("master: Mirror ON ({})", mirror_desc());
    notify(&format!("미러 ON — {}", mirror_desc()));
}

fn mirror_off() {
    for side in live_mirror_targets() {
        crate::input::capture::release_held(side);
        send_tcp(side, Frame::new(MSG_RETURN_CONTROL));
    }
    SHARED.mirror.store(false, Ordering::Release);
    crate::input::capture::clear_held();
    // Remote 상태가 아니면 훅도 uninstall (Local 로 완전 복귀).
    if SHARED.get() != MasterState::Remote {
        unsafe { crate::input::hooks::uninstall(); }
    }
    tracing::info!("master: Mirror OFF");
    notify("미러 OFF");
}

/// 미러 대상 넣기/빼기. 미러 중이면 바로 반영, 아니면 다음 미러 ON 때 반영.
pub fn toggle_mirror_target(side: Side) {
    if link(side).is_none() {
        notify(&format!("{} 슬레이브가 설정에 없음", side.label()));
        return;
    }
    let on = !SHARED.is_mirror_target(side);
    SHARED.mirror_targets[side.idx()].store(on, Ordering::Release);
    if SHARED.mirror.load(Ordering::Acquire) && SHARED.is_connected(side) {
        if on {
            send_tcp(side, Frame::take_control(tcp::SIDE_CENTER, 50, master_wall(side)));
        } else {
            crate::input::capture::release_held(side);
            send_tcp(side, Frame::new(MSG_RETURN_CONTROL));
        }
    }
    tracing::info!("master: 미러 대상 변경 ({})", mirror_desc());
    notify(&format!("미러 대상 — {}", mirror_desc()));
}

// -----------------------------------------------------------------------------
// 트레이 메뉴
// -----------------------------------------------------------------------------

/// master 트레이 메뉴 추가 항목: 슬레이브 연결 상태, 쓸어넘기기, 미러 대상.
pub fn tray_items() -> Vec<MenuItem> {
    let mut items = Vec::new();
    for side in Side::ALL {
        if let Some(l) = link(side) {
            let st = if SHARED.is_connected(side) { "연결됨" } else { "연결 안 됨" };
            items.push(MenuItem {
                id: 0,
                label: format!("{} 슬레이브 ({}): {}", side.label(), l.target, st),
                checked: false,
                grayed: true,
            });
        }
    }
    for (side, id) in [(Side::Left, crate::tray::MENU_EDGE_LEFT), (Side::Right, crate::tray::MENU_EDGE_RIGHT)] {
        if link(side).is_some() {
            items.push(MenuItem {
                id,
                label: format!("쓸어넘기기: {} (화면 {} 끝 → {} 슬레이브)", side.label(), side.label(), side.label()),
                checked: SHARED.is_edge_enabled(side),
                grayed: false,
            });
        }
    }
    for (side, id) in [(Side::Left, crate::tray::MENU_MIRROR_LEFT), (Side::Right, crate::tray::MENU_MIRROR_RIGHT)] {
        if link(side).is_some() {
            items.push(MenuItem {
                id,
                label: format!("미러 대상: {}", side.label()),
                checked: SHARED.is_mirror_target(side),
                grayed: false,
            });
        }
    }
    items
}
