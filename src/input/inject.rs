// Slave 쪽 입력 주입.
//
// 진입: slave::run 에서 SlaveCtx 를 만들어 넘겨줌.
//
// 동작:
//  1. UDP 수신 (블로킹, 1초 타임아웃)
//  2. 패킷 디코딩 → seq 검증 → 종류별 처리
//     - MouseMove   : Active 상태일 때만 SetCursorPos + 엣지 감지 (왼쪽 끝 → RETURN_CONTROL)
//     - MouseButton : Active 상태일 때만 SendInput
//     - MouseWheel  : Active 상태일 때만 SendInput
//     - KeyEvent    : Active 상태일 때만 SendInput (scan code)

use anyhow::Result;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::input::cursor;
use crate::net::packet::{
    Kind, Packet, FLAG_BTN_DOWN, FLAG_KEY_EXT,
    MB_LEFT, MB_MIDDLE, MB_RIGHT, MB_X1, MB_X2,
};
use crate::net::udp;
use crate::slave::{self, SlaveCtx, TcpTx};
use crate::state::SlaveState;

static LAST_SEQ: AtomicU32 = AtomicU32::new(0);

pub fn run_with_ctx(ctx: SlaveCtx) -> Result<()> {
    unsafe { boost_priority(); }

    let sock = udp::bind_recv(ctx.cfg.udp_port)?;
    tracing::info!("slave: UDP {} 대기 중", ctx.cfg.udp_port);

    let mut buf = [0u8; 64];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, _peer)) => {
                if let Some(p) = Packet::decode(&buf[..n]) {
                    if is_new_seq(p.seq) {
                        handle_packet(p, &ctx.tcp_tx);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                tracing::error!("UDP recv 오류: {}", e);
                continue;
            }
        }
    }
}

unsafe fn boost_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
        HIGH_PRIORITY_CLASS, THREAD_PRIORITY_TIME_CRITICAL,
    };
    let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
}

fn is_new_seq(seq: u32) -> bool {
    // 최초 (LAST_SEQ==0) 는 통과.
    let last = LAST_SEQ.load(Ordering::Relaxed);
    if last == 0 {
        LAST_SEQ.store(seq, Ordering::Relaxed);
        return true;
    }
    let diff = seq.wrapping_sub(last) as i32;
    if diff > 0 {
        LAST_SEQ.store(seq, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn handle_packet(p: Packet, tx: &TcpTx) {
    // Active 상태에서만 처리
    let state = slave::SHARED.get();
    if !slave::SHARED.enabled.load(Ordering::Acquire) { return; }
    if state != SlaveState::Active {
        tracing::trace!("state={:?} 라 패킷 드롭 kind={:?}", state, p.kind);
        return;
    }

    match p.kind {
        Kind::MouseMove => {
            inject_move(p.dx as i32, p.dy as i32);
            check_edge_and_return(tx);
        }
        Kind::MouseButton => inject_button(p.button, (p.flags & FLAG_BTN_DOWN) != 0),
        Kind::MouseWheel  => inject_wheel(p.wheel_dx as i32, p.wheel_dy as i32),
        Kind::KeyEvent    => inject_key(p.button, (p.flags & FLAG_BTN_DOWN) != 0, (p.flags & FLAG_KEY_EXT) != 0),
        Kind::Heartbeat   => {}
    }
}

fn inject_move(dx: i32, dy: i32) {
    let pt = match cursor::get_pos() { Some(p) => p, None => return };
    cursor::set_pos(pt.x + dx, pt.y + dy);
}

/// 왼쪽 엣지 도달 시 RETURN_CONTROL 전송.
fn check_edge_and_return(tx: &TcpTx) {
    let pt = match cursor::get_pos() { Some(p) => p, None => return };
    let scr = cursor::primary_screen();
    if pt.x <= scr.left {
        slave::return_control(tx);
    }
}

fn inject_button(button: u16, down: bool) {
    let flag = match (button, down) {
        (MB_LEFT,   true)  => MOUSEEVENTF_LEFTDOWN,
        (MB_LEFT,   false) => MOUSEEVENTF_LEFTUP,
        (MB_RIGHT,  true)  => MOUSEEVENTF_RIGHTDOWN,
        (MB_RIGHT,  false) => MOUSEEVENTF_RIGHTUP,
        (MB_MIDDLE, true)  => MOUSEEVENTF_MIDDLEDOWN,
        (MB_MIDDLE, false) => MOUSEEVENTF_MIDDLEUP,
        (MB_X1,     true)  => MOUSEEVENTF_XDOWN,
        (MB_X1,     false) => MOUSEEVENTF_XUP,
        (MB_X2,     true)  => MOUSEEVENTF_XDOWN,
        (MB_X2,     false) => MOUSEEVENTF_XUP,
        _ => return,
    };
    let mouse_data: u32 = match button {
        MB_X1 => XBUTTON1 as u32,
        MB_X2 => XBUTTON2 as u32,
        _ => 0,
    };

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0, dy: 0,
                mouseData: mouse_data,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn inject_wheel(hx: i32, vy: i32) {
    if vy != 0 {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0, dy: 0,
                    mouseData: vy as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32); }
    }
    if hx != 0 {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0, dy: 0,
                    mouseData: hx as u32,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32); }
    }
}

fn inject_key(scan: u16, down: bool, ext: bool) {
    // scan code 기반 주입: KEYEVENTF_SCANCODE 로 hardware scan code 를 그대로 전달.
    // 언어/키보드 레이아웃 차이에 영향 안 받음.
    let mut flags = KEYEVENTF_SCANCODE;
    if !down { flags |= KEYEVENTF_KEYUP; }
    if ext   { flags |= KEYEVENTF_EXTENDEDKEY; }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}
