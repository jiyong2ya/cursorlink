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

/// ephemeral 포트 송신 소켓. 목적지는 TCP 연결 시점에 connect 로 지정
/// (peer 가 hostname 이어도 TCP 가 실제로 붙은 IP 를 그대로 씀).
pub fn bind_send() -> Result<UdpSocket> {
    UdpSocket::bind("0.0.0.0:0").context("UDP 송신 소켓 바인딩 실패")
}
