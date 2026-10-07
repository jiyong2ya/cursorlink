// 공유 상태 (마스터/슬레이브 공통).
//
// 스레드 안전을 위해 atomic 으로 표현. u8 로 인코딩.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// 마스터 기준 슬레이브 위치. 배치: [왼쪽 슬레이브] ← [마스터] → [오른쪽 슬레이브]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left  = 0,
    Right = 1,
}

impl Side {
    pub const ALL: [Side; 2] = [Side::Left, Side::Right];

    pub fn from_u8(v: u8) -> Self {
        if v == 1 { Self::Right } else { Self::Left }
    }
    pub fn idx(self) -> usize { self as usize }
    /// 로그/알림용
    pub fn label(self) -> &'static str {
        match self { Self::Left => "왼쪽", Self::Right => "오른쪽" }
    }
    /// 스레드 이름용
    pub fn tag(self) -> &'static str {
        match self { Self::Left => "left", Self::Right => "right" }
    }
}

// Master 관점
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState {
    Local  = 1, // 마스터에서 사용 중
    Remote = 2, // 슬레이브 (MasterShared::active 쪽) 에서 사용 중
}

impl MasterState {
    pub fn from_u8(v: u8) -> Self {
        if v == 2 { Self::Remote } else { Self::Local }
    }
}

// Slave 관점
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaveState {
    Disconnected = 0,
    Idle         = 1, // TCP 연결됐지만 마스터가 제어중
    Active       = 2, // 슬레이브가 제어중
}

impl SlaveState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Idle,
            2 => Self::Active,
            _ => Self::Disconnected,
        }
    }
}

// 마스터 전역 상태
pub struct MasterShared {
    state: AtomicU8,
    /// Remote 일 때 제어 중인 슬레이브 (Side as u8).
    active: AtomicU8,
    /// 사용자가 단축키로 켜고 끔. false 면 어떤 상태 전환도 안 함.
    pub enabled: AtomicBool,
    /// 쓸어넘기기 (마스터 화면 끝 → 슬레이브) 사용 여부.
    /// 꺼도 슬레이브 → 마스터 복귀 (슬레이브 화면 끝) 는 항상 동작.
    pub edge_enabled: AtomicBool,
    /// Mirror 모드: 마스터 + 미러 대상 슬레이브 동시 조작. Local 상태에서만 켜짐.
    pub mirror: AtomicBool,
    /// 미러 대상 (Side::idx 로 인덱싱). 미러 off 중에도 유지 → 다음 미러 ON 때 그대로 사용.
    pub mirror_targets: [AtomicBool; 2],
    /// 슬레이브별 TCP 연결 + HELLO 완료 여부 (Side::idx 로 인덱싱).
    pub connected: [AtomicBool; 2],
}

impl MasterShared {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(MasterState::Local as u8),
            active: AtomicU8::new(Side::Right as u8),
            enabled: AtomicBool::new(true),
            edge_enabled: AtomicBool::new(true),
            mirror: AtomicBool::new(false),
            mirror_targets: [AtomicBool::new(true), AtomicBool::new(true)],
            connected: [AtomicBool::new(false), AtomicBool::new(false)],
        }
    }
    pub fn get(&self) -> MasterState {
        MasterState::from_u8(self.state.load(Ordering::Acquire))
    }
    pub fn set(&self, s: MasterState) {
        self.state.store(s as u8, Ordering::Release);
    }
    pub fn active(&self) -> Side {
        Side::from_u8(self.active.load(Ordering::Acquire))
    }
    pub fn set_active(&self, s: Side) {
        self.active.store(s as u8, Ordering::Release);
    }
    pub fn is_connected(&self, s: Side) -> bool {
        self.connected[s.idx()].load(Ordering::Acquire)
    }
    pub fn is_mirror_target(&self, s: Side) -> bool {
        self.mirror_targets[s.idx()].load(Ordering::Acquire)
    }
}

pub struct SlaveShared {
    state: AtomicU8,
    pub enabled: AtomicBool,
    /// 이 벽에 닿으면 마스터로 복귀 (tcp::SIDE_LEFT / SIDE_RIGHT). TAKE_CONTROL 때 마스터가 알려줌.
    pub return_side: AtomicU8,
}

impl SlaveShared {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(SlaveState::Disconnected as u8),
            enabled: AtomicBool::new(true),
            return_side: AtomicU8::new(0),
        }
    }
    pub fn get(&self) -> SlaveState {
        SlaveState::from_u8(self.state.load(Ordering::Acquire))
    }
    pub fn set(&self, s: SlaveState) {
        self.state.store(s as u8, Ordering::Release);
    }
}
