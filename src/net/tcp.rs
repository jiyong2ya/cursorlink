// TCP 제어 채널.
//
// 프로토콜: 8바이트 고정 프레임.
//   [0]     msg_type (u8)
//   [1..8]  payload (msg_type 마다 해석 다름)
//
// msg_type:
//   0x01 TAKE_CONTROL    Master → Slave   payload: [entry_side, entry_y_percent, 0, 0, 0, 0, 0]
//                                          entry_side: 0=left, 1=right, 2=top, 3=bottom
//                                          entry_y_percent: 0..100 (진입 위치의 세로 비율)
//   0x02 RETURN_CONTROL  Slave  → Master  payload: 무의미
//   0x03 HEARTBEAT       양방향            payload: 무의미 (3초 주기)
//   0x04 HELLO           Master → Slave   최초 연결 직후 인증. payload: shared_secret 앞 8바이트
//
// TCP_NODELAY 필수 (Nagle 지연 회피).
// 하트비트 미수신 6초 → 연결 재수립.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

pub const FRAME_SIZE: usize = 8;

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

#[derive(Debug, Clone, Copy)]
pub struct Frame(pub [u8; FRAME_SIZE]);

impl Frame {
    pub fn new(msg_type: u8) -> Self {
        let mut b = [0u8; FRAME_SIZE];
        b[0] = msg_type;
        Self(b)
    }
    pub fn take_control(entry_side: u8, entry_y_pct: u8) -> Self {
        let mut f = Self::new(MSG_TAKE_CONTROL);
        f.0[1] = entry_side;
        f.0[2] = entry_y_pct;
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

pub fn configure_stream(sock: &TcpStream) -> Result<()> {
    sock.set_nodelay(true).context("TCP_NODELAY 설정 실패")?;
    sock.set_read_timeout(Some(Duration::from_secs(6))).context("TCP read timeout 설정 실패")?;
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
