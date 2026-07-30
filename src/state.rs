// 공유 상태 (마스터/슬레이브 공통).
//
// 스레드 안전을 위해 atomic 으로 표현. u8 로 인코딩.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

// Master 관점
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterState {
    Disconnected = 0, // TCP 미연결
    Local        = 1, // 마스터에서 사용 중
    Remote       = 2, // 슬레이브에서 사용 중
}

impl MasterState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Local,
            2 => Self::Remote,
            _ => Self::Disconnected,
        }
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
    /// 사용자가 단축키로 켜고 끔. false 면 어떤 상태 전환도 안 함.
    pub enabled: AtomicBool,
}

impl MasterShared {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(MasterState::Disconnected as u8),
            enabled: AtomicBool::new(true),
        }
    }
    pub fn get(&self) -> MasterState {
        MasterState::from_u8(self.state.load(Ordering::Acquire))
    }
    pub fn set(&self, s: MasterState) {
        self.state.store(s as u8, Ordering::Release);
    }
}

pub struct SlaveShared {
    state: AtomicU8,
    pub enabled: AtomicBool,
}

impl SlaveShared {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(SlaveState::Disconnected as u8),
            enabled: AtomicBool::new(true),
        }
    }
    pub fn get(&self) -> SlaveState {
        SlaveState::from_u8(self.state.load(Ordering::Acquire))
    }
    pub fn set(&self, s: SlaveState) {
        self.state.store(s as u8, Ordering::Release);
    }
}
