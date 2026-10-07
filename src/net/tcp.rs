// TCP 제어 채널.
//
// 프로토콜: 8바이트 고정 프레임.
//   [0]     msg_type (u8)
//   [1..8]  payload (msg_type 마다 해석 다름)
//
// msg_type:
//   0x01 TAKE_CONTROL    Master → Slave   payload: [entry_side, entry_y_percent, return_side, 0, 0, 0, 0]
//                                          entry_side: 커서가 나타날 곳. 0=왼쪽 벽, 1=오른쪽 벽, 4=화면 중앙
//                                          entry_y_percent: 0..100 (진입 위치의 세로 비율)
//                                          return_side: 이 벽에 닿으면 마스터로 복귀. 0=왼쪽 (구버전 기본값), 1=오른쪽
//   0x02 RETURN_CONTROL  양방향            payload: [reason, 0...] (Slave → Master 일 때 복귀 이유, 구버전은 0)
//                                          reason: 0=보통 (벽/단축키), 1=보안 화면 (UAC/잠금화면), 2=슬레이브 꺼짐
//   0x03 HEARTBEAT       양방향            payload: 무의미 (3초 주기)
//   0x04 HELLO           Master → Slave   최초 연결 직후 인증. payload: shared_secret 앞 8바이트
//
// TCP_NODELAY 필수 (Nagle 지연 회피).
// 하트비트 (3초 주기) 포함 아무 프레임도 PEER_TIMEOUT 동안 안 오면 연결 끊김으로 처리 → 재연결.
// (노트북 덮개 닫기/절전/Wi-Fi 끊김은 TCP 가 바로 알려주지 않아서 직접 감지해야 함)

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const FRAME_SIZE: usize = 8;

/// 이 시간 동안 상대에게서 프레임 (하트비트 포함) 이 하나도 안 오면 연결 끊김.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);

// Message types
pub const MSG_TAKE_CONTROL:   u8 = 0x01;
pub const MSG_RETURN_CONTROL: u8 = 0x02;
pub const MSG_HEARTBEAT:      u8 = 0x03;
pub const MSG_HELLO:          u8 = 0x04;

// Entry side (TAKE_CONTROL payload byte 0)
pub const SIDE_LEFT:   u8 = 0;
pub const SIDE_RIGHT:  u8 = 1;
pub const SIDE_TOP:    u8 = 2;
pub const SIDE_BOTTOM: u8 = 3;
/// hotkey_transfer 로 즉시 전환할 때 사용. Slave 커서를 화면 중앙에 배치.
pub const SIDE_CENTER: u8 = 4;

// RETURN_CONTROL 이유 (payload byte 0)
/// 복귀 벽 도달 / 단축키 등 보통 복귀
pub const RETURN_NORMAL: u8 = 0;
/// 슬레이브에 UAC 확인창 / 잠금화면 (보안 화면) → 공유 입력으로 조작 불가라 자동 복귀
pub const RETURN_SECURE_DESKTOP: u8 = 1;
/// 슬레이브 기능이 꺼져 있어서 TAKE_CONTROL 거절
pub const RETURN_DISABLED: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Frame(pub [u8; FRAME_SIZE]);

impl Frame {
    pub fn new(msg_type: u8) -> Self {
        let mut b = [0u8; FRAME_SIZE];
        b[0] = msg_type;
        Self(b)
    }
    pub fn take_control(entry_side: u8, entry_y_pct: u8, return_side: u8) -> Self {
        let mut f = Self::new(MSG_TAKE_CONTROL);
        f.0[1] = entry_side;
        f.0[2] = entry_y_pct;
        f.0[3] = return_side;
        f
    }
    pub fn return_control(reason: u8) -> Self {
        let mut f = Self::new(MSG_RETURN_CONTROL);
        f.0[1] = reason;
        f
    }
    pub fn hello(secret: &str) -> Self {
        let mut f = Self::new(MSG_HELLO);
        let bytes = secret.as_bytes();
        let n = bytes.len().min(7);
        f.0[1..1+n].copy_from_slice(&bytes[..n]);
        f
    }
    pub fn msg_type(&self) -> u8 { self.0[0] }
    pub fn payload_byte(&self, idx: usize) -> u8 { self.0[1 + idx.min(6)] }
    pub fn is_authenticated(&self, secret: &str) -> bool {
        let bytes = secret.as_bytes();
        let n = bytes.len().min(7);
        &self.0[1..1+n] == &bytes[..n]
    }
}

pub fn write_frame(sock: &mut TcpStream, f: Frame) -> std::io::Result<()> {
    sock.write_all(&f.0)?;
    sock.flush()
}

pub fn read_frame(sock: &mut TcpStream) -> std::io::Result<Frame> {
    let mut b = [0u8; FRAME_SIZE];
    sock.read_exact(&mut b)?;
    Ok(Frame(b))
}

/// 연결이 살아있는 동안 프레임을 읽어 on_frame 에 넘김. 반환 = 연결 끊김.
/// read timeout 은 주기적 wake-up 용도 (정상 idle). 진짜 연결 오류이거나
/// PEER_TIMEOUT 동안 아무것도 안 오면 반환.
pub fn read_loop(sock: &mut TcpStream, tag: &str, mut on_frame: impl FnMut(Frame)) {
    let mut last_rx = Instant::now();
    loop {
        match read_frame(sock) {
            Ok(f) => {
                last_rx = Instant::now();
                on_frame(f);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {
                if last_rx.elapsed() >= PEER_TIMEOUT {
                    tracing::warn!(
                        "{} {}초 동안 응답 없음 (절전/네트워크 끊김?) → 연결 해제",
                        tag, PEER_TIMEOUT.as_secs()
                    );
                    return;
                }
            }
            Err(e) => {
                tracing::warn!("{} TCP read 오류: {}", tag, e);
                return;
            }
        }
    }
}

pub fn configure_stream(sock: &TcpStream) -> Result<()> {
    sock.set_nodelay(true).context("TCP_NODELAY 설정 실패")?;
    // 짧게 깨어나서 PEER_TIMEOUT 을 확인 (read_loop).
    sock.set_read_timeout(Some(Duration::from_secs(2))).context("TCP read timeout 설정 실패")?;
    sock.set_write_timeout(Some(Duration::from_secs(2))).context("TCP write timeout 설정 실패")?;
    Ok(())
}

pub fn connect(peer: SocketAddr) -> Result<TcpStream> {
    let sock = TcpStream::connect_timeout(&peer, Duration::from_secs(3))
        .with_context(|| format!("TCP {} 연결 실패", peer))?;
    configure_stream(&sock)?;
    Ok(sock)
}

pub fn listen(local_port: u16) -> Result<TcpListener> {
    let addr: SocketAddr = format!("0.0.0.0:{}", local_port).parse()?;
    let l = TcpListener::bind(addr)
        .with_context(|| format!("TCP {} bind 실패", local_port))?;
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_control_payload_layout() {
        let f = Frame::take_control(SIDE_RIGHT, 37, SIDE_RIGHT);
        assert_eq!(f.msg_type(), MSG_TAKE_CONTROL);
        assert_eq!(f.payload_byte(0), SIDE_RIGHT);
        assert_eq!(f.payload_byte(1), 37);
        assert_eq!(f.payload_byte(2), SIDE_RIGHT);
    }

    #[test]
    fn old_style_take_control_returns_left() {
        // 구버전 마스터는 payload[2] 를 0 으로 보냄 → 왼쪽 벽 복귀 (기존 동작) 로 해석돼야 함.
        let mut f = Frame::new(MSG_TAKE_CONTROL);
        f.0[1] = SIDE_LEFT;
        f.0[2] = 50;
        assert_eq!(f.payload_byte(2), SIDE_LEFT);
    }

    #[test]
    fn return_control_reason() {
        let f = Frame::return_control(RETURN_SECURE_DESKTOP);
        assert_eq!(f.msg_type(), MSG_RETURN_CONTROL);
        assert_eq!(f.payload_byte(0), RETURN_SECURE_DESKTOP);
        // 구버전 형식 (payload 0) 은 보통 복귀
        assert_eq!(Frame::new(MSG_RETURN_CONTROL).payload_byte(0), RETURN_NORMAL);
    }

    #[test]
    fn hello_auth() {
        let f = Frame::hello("change-me-to-random-string");
        assert!(f.is_authenticated("change-me-to-random-string"));
        assert!(!f.is_authenticated("other-secret"));
    }
}
