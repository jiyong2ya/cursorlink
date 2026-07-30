// UDP 송/수신 유틸.
// 저지연을 위해 std::net::UdpSocket 를 블로킹 모드로 그대로 사용.
// 스레드 하나가 recv_from 에 블로킹되어 있으면 커널이 즉시 깨워줌.

use anyhow::{Context, Result};
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

pub fn bind_recv(local_port: u16) -> Result<UdpSocket> {
    let addr: SocketAddr = format!("0.0.0.0:{}", local_port).parse()?;
    let sock = UdpSocket::bind(addr)
        .with_context(|| format!("UDP {} 바인딩 실패", local_port))?;
    // 재시작 시 TIME_WAIT 회피는 UDP 에선 무의미. 수신 타임아웃만 살짝.
    sock.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok(sock)
}

pub fn bind_send(peer: SocketAddr) -> Result<UdpSocket> {
    // ephemeral 포트에 바인딩, connect 로 목적지 고정 → send 만 호출 가능
    let sock = UdpSocket::bind("0.0.0.0:0")
        .context("UDP 송신 소켓 바인딩 실패")?;
    sock.connect(peer)
        .with_context(|| format!("UDP connect {} 실패", peer))?;
    Ok(sock)
}
