// 고정 크기 입력 패킷.
// UDP 로 왕복 지연 최소화하기 위해 20바이트 짜리 packed struct.
//
// 바이트 레이아웃 (little-endian):
//   0..4   seq       : u32   시퀀스 번호 (수신측이 out-of-order 폐기용)
//   4..8   ts_us     : u32   송신측 로컬 타임스탬프 µs (디버그/지연 측정)
//   8..9   kind      : u8    이벤트 종류 (아래 참조)
//   9..10  flags     : u8    부가 플래그
//   10..12 button    : u16   버튼/키 코드 (mouse: MouseButton, key: scan code)
//   12..14 dx        : i16
//   14..16 dy        : i16
//   16..18 wheel_dx  : i16
//   18..20 wheel_dy  : i16

use std::mem::size_of;

pub const PACKET_SIZE: usize = 20;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    MouseMove = 0,
    MouseButton = 1,
    MouseWheel = 2,
    KeyEvent = 3,
    Heartbeat = 4,
    /// Mirror 모드용 절대 위치 sync.
    /// dx = x_ppm (0..10000, 화면 폭의 0.01% 단위)
    /// dy = y_ppm (0..10000, 화면 높이의 0.01% 단위)
    MousePos = 5,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::MouseMove,
            1 => Self::MouseButton,
            2 => Self::MouseWheel,
            3 => Self::KeyEvent,
            4 => Self::Heartbeat,
            5 => Self::MousePos,
            _ => return None,
        })
    }
}

// flags 비트
pub const FLAG_BTN_DOWN: u8 = 1 << 0;   // MouseButton / KeyEvent 공용: 눌림 여부
pub const FLAG_KEY_EXT:  u8 = 1 << 1;   // 확장 키 스캔코드 (E0 prefix)

// 마우스 버튼 코드 (button 필드)
pub const MB_LEFT:   u16 = 1;
pub const MB_RIGHT:  u16 = 2;
pub const MB_MIDDLE: u16 = 3;
pub const MB_X1:     u16 = 4;
pub const MB_X2:     u16 = 5;

#[derive(Debug, Clone, Copy)]
pub struct Packet {
    pub seq: u32,
    pub ts_us: u32,
    pub kind: Kind,
    pub flags: u8,
    pub button: u16,
    pub dx: i16,
    pub dy: i16,
    pub wheel_dx: i16,
    pub wheel_dy: i16,
}

impl Packet {
    pub fn encode(&self) -> [u8; PACKET_SIZE] {
        let mut b = [0u8; PACKET_SIZE];
        b[0..4].copy_from_slice(&self.seq.to_le_bytes());
        b[4..8].copy_from_slice(&self.ts_us.to_le_bytes());
        b[8] = self.kind as u8;
        b[9] = self.flags;
        b[10..12].copy_from_slice(&self.button.to_le_bytes());
        b[12..14].copy_from_slice(&self.dx.to_le_bytes());
        b[14..16].copy_from_slice(&self.dy.to_le_bytes());
        b[16..18].copy_from_slice(&self.wheel_dx.to_le_bytes());
        b[18..20].copy_from_slice(&self.wheel_dy.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < PACKET_SIZE { return None; }
        let seq      = u32::from_le_bytes(b[0..4].try_into().ok()?);
        let ts_us    = u32::from_le_bytes(b[4..8].try_into().ok()?);
        let kind     = Kind::from_u8(b[8])?;
        let flags    = b[9];
        let button   = u16::from_le_bytes(b[10..12].try_into().ok()?);
        let dx       = i16::from_le_bytes(b[12..14].try_into().ok()?);
        let dy       = i16::from_le_bytes(b[14..16].try_into().ok()?);
        let wheel_dx = i16::from_le_bytes(b[16..18].try_into().ok()?);
        let wheel_dy = i16::from_le_bytes(b[18..20].try_into().ok()?);
        Some(Self { seq, ts_us, kind, flags, button, dx, dy, wheel_dx, wheel_dy })
    }
}

const _: () = assert!(PACKET_SIZE == 20);
const _: () = assert!(size_of::<u32>() == 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mouse_move() {
        let p = Packet {
            seq: 12345,
            ts_us: 987654,
            kind: Kind::MouseMove,
            flags: 0,
            button: 0,
            dx: -42,
            dy: 7,
            wheel_dx: 0,
            wheel_dy: 0,
        };
        let bytes = p.encode();
        let q = Packet::decode(&bytes).unwrap();
        assert_eq!(q.seq, p.seq);
        assert_eq!(q.dx, p.dx);
        assert_eq!(q.dy, p.dy);
        assert_eq!(q.kind as u8, p.kind as u8);
    }

    #[test]
    fn decode_short_returns_none() {
        assert!(Packet::decode(&[0u8; 10]).is_none());
    }
}
