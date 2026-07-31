# cursorlink — Windows 세션 인수인계 문서

이 문서는 Mac 에서 코드 짜놓고 Windows 로 넘어가서 빌드/테스트할 때 참고용.
새 Claude 세션이 이 파일 읽고 상황 파악할 수 있도록 자기완결적으로 작성.

---

## 1. 프로젝트 요약

**cursorlink** — 두 Windows PC 사이에서 마우스/키보드를 공유하는 KVM 유틸.
Multiplicity / Input Director / Barrier 와 같은 방식이지만 유저모드 전용,
드라이버 없음, 인증서 없음.

**목적:** 사무용. 게임 매크로 목적 아님. Barrier 계열 프로그램이 사용자의 게임 (NGX 안티치트) 에서 튕겨서 자체 개발.

**동작 원리:**
- Master PC: 물리 마우스/키보드가 꽂힌 쪽. Raw Input (`WM_INPUT`) 으로 이벤트 캡처.
- Slave PC: master 화면 오른쪽 끝에 커서 닿으면 slave 로 제어권 이동.
- 이후 master 는 커서 락 + 숨김. 이벤트를 UDP 로 slave 에 전송. Slave 는 `SetCursorPos`/`SendInput` 으로 주입.
- Slave 커서가 왼쪽 끝 닿으면 다시 master 로 복귀.
- TCP 로 제어 신호 (TAKE_CONTROL / RETURN_CONTROL / HEARTBEAT / HELLO).

---

## 2. 저장소

- **URL:** https://github.com/jiyong2ya/cursorlink.git
- **Branch:** master
- **최신 커밋 (Mac push 시점):**
  - `6797864` fix: windows 0.58 API 정합성 사전 수정
  - `1be6d69` Phase 3: 전역 단축키 + 트레이 + 자동시작
  - `21b107d` Phase 1 + 2: 마우스/키보드 공유 KVM 뼈대

Windows 에서 최신 상태 확인: `git log --oneline -5`

---

## 3. 파일 구조

```
cursorlink/
├── Cargo.toml              — 의존성 (windows 0.58, serde, tokio 없음, crossbeam-channel)
├── config.example.toml     — 첫 실행 시 config.toml 로 복사됨
├── HANDOFF.md              — 이 문서
├── README.md
└── src/
    ├── main.rs             — 진입점, mode 별 dispatch
    ├── config.rs           — TOML 로드, 첫 실행 시 notepad 자동 오픈
    ├── logging.rs          — 파일 로거 (release 는 stdout 안 뜨니 필수)
    ├── state.rs            — MasterShared / SlaveShared (Atomic 상태머신)
    ├── hotkey.rs           — RegisterHotKey + slave 전용 hidden window
    ├── autostart.rs        — HKCU\Run 레지스트리 조작
    ├── tray.rs             — Shell_NotifyIcon 트레이 아이콘
    ├── master.rs           — 마스터 오케스트레이션 (TCP 워커 + 하트비트)
    ├── slave.rs            — 슬레이브 오케스트레이션 (TCP 서버 + 하트비트 + 트레이)
    ├── net/
    │   ├── mod.rs
    │   ├── packet.rs       — 20바이트 UDP 입력 패킷 encode/decode + 유닛테스트
    │   ├── udp.rs          — bind_recv / bind_send
    │   └── tcp.rs          — 8바이트 제어 프레임 + connect/listen
    └── input/
        ├── mod.rs
        ├── capture.rs      — [master] Raw Input → UDP 송신
        ├── inject.rs       — [slave] UDP 수신 → SetCursorPos / SendInput
        └── cursor.rs       — GetCursorPos / SetCursorPos / ClipCursor / ShowCursor 헬퍼
```

---

## 4. 빌드 방법 (Windows)

**Rust 설치:** `winget install Rustlang.Rustup` 또는 https://rustup.rs 에서 rustup-init.exe

**빌드:**
```
cd cursorlink
cargo build --release
```

- release 산출물: `target\release\cursorlink.exe`
- debug 로 돌리면 콘솔에도 로그 뜸: `cargo run`
- 자세한 로그: `set RUST_LOG=cursorlink=debug && cargo run`

---

## 5. 컴파일 에러 예상 지점 (Mac 에서 짰기 때문)

내가 windows 크레이트 0.58 API 세부에 확신 못 하는 부분들.
실제 빌드에서 여기가 터질 확률 높음:

### 5.1 RAWMOUSE 필드 타입
`src/input/capture.rs`
- `m.usFlags & MOUSE_MOVE_ABSOLUTE` — 만약 `usFlags` 가 `MOUSE_STATE(u16)` wrapper 면 `.0` 필요
- `m.Anonymous.Anonymous.usButtonFlags` — 마찬가지로 `RI_MOUSE_STATE` wrapper 가능성
- `RI_MOUSE_BUTTON_*_DOWN as u16` — 상수가 wrapper 면 `.0` 필요

**fix 방법:** 컴파일러가 "expected u16, found RI_MOUSE_STATE" 같은 에러 내면 그 자리에 `.0` 추가.

### 5.2 RID_DEVICE_INFO_TYPE 비교
`src/input/capture.rs::handle_raw_input`
```rust
if ri.header.dwType == RIM_TYPEMOUSE {
```
- `dwType` 가 `u32` 이고 `RIM_TYPEMOUSE` 가 wrapper 면 `== RIM_TYPEMOUSE.0`
- 둘 다 wrapper 면 == 로 OK
- 둘 다 u32 면 == 로 OK

### 5.3 HRAWINPUT 캐스팅
```rust
let hri = HRAWINPUT(lparam.0 as *mut _);
```
- 0.58 에서 HRAWINPUT 필드가 `*mut c_void` 이면 OK
- `*mut _` 추론 실패하면 `as *mut std::ffi::c_void` 명시

### 5.4 Registry API 시그니처
`src/autostart.rs`
- `RegSetValueExW` 의 마지막 인자를 `Some(bytes)` 로 넘김. 만약 시그니처가 `Option<*const u8>` 이면 `Some(bytes.as_ptr())` + `bytes.len() as u32` 별도 인자
- 이미 pointer cast 사전 fix 는 했지만 시그니처 변경엔 대응 안 됨

### 5.5 트레이 관련
`src/tray.rs`
- `TrackPopupMenu` 인자 갯수 (7개) — 버전마다 다를 수 있음
- `LoadIconW(HINSTANCE::default(), IDI_APPLICATION)` — 이미 default 로 변경했으니 OK 예상

### 5.6 flags 비트 연산
- `let mut flags = MF_STRING; flags |= MF_CHECKED;` — `MF_STRING` 이 wrapper 면 `BitOrAssign` impl 필요.
- windows-rs 0.58 은 대부분 impl 되어 있어서 OK 예상. 안 되면 `let flags = MF_STRING | MF_CHECKED;` 로.

### 5.7 예상 안 되는 것 (아마 OK)
- `SendInput` 시그니처
- `INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { ... } } }` 구조
- `GetCursorPos` / `SetCursorPos`
- 소켓 (std::net)
- crossbeam-channel

---

## 6. 실행 흐름 (사용자 관점)

**첫 실행 (양쪽 PC 각각):**
1. `target\release\cursorlink.exe` 더블클릭
2. 자동으로 notepad 열리고 `config.toml` 편집 화면 나옴
3. 수정 후 저장:
   - Master 쪽:
     ```
     mode = "master"
     peer_ip = "슬레이브PC의IP"
     shared_secret = "양쪽 동일한 문자열"
     ```
   - Slave 쪽:
     ```
     mode = "slave"
     peer_ip = "마스터PC의IP"
     shared_secret = "양쪽 동일한 문자열"
     ```
4. notepad 닫기 + 확인 창 "확인"
5. 트레이 아이콘 (Windows 기본 앱 아이콘) 뜸
6. Windows 방화벽 팝업 → 개인 네트워크 허용

**동작 확인:**
- 트레이 우클릭 → 로그 보기 → `master: TCP 연결 성공, Local 상태` 라인 확인 (master 쪽)
- Master 마우스를 오른쪽 화면 끝으로 이동 → 커서 사라짐 → Slave 화면 왼쪽에 나타나야 함
- Slave 에서 왼쪽 끝 이동 → 다시 Master 로 복귀
- `Ctrl+Alt+Shift+K` → 기능 on/off 토글
- 트레이 우클릭 → 종료

---

## 7. 트러블슈팅 체크리스트

**트레이 아이콘 안 뜸**
- `cursorlink.log` 확인. 에러 있으면 채팅에 붙여줌
- 방화벽 팝업 놓쳤을 수 있음 → `wf.msc` 에서 규칙 확인

**Master TCP 계속 재시도**
- Slave 가 켜져 있는지
- peer_ip 오타 확인
- 방화벽에서 46011/UDP, 46012/TCP 허용됐는지
- `ping <peer_ip>` 로 기본 연결 확인

**HELLO 인증 실패**
- 양쪽 `shared_secret` 이 정확히 동일한지 (공백, 대소문자, 개행)

**커서가 안 넘어감**
- Master 로그에 `Local → Remote` 라인 나오는지 확인
- 안 나오면: `check_edge_and_transfer` 가 호출 안 됨. Local 상태 아닐 수 있음
- 나오는데 slave 커서 안 나타나면: UDP 도달 문제

**커서가 넘어갔는데 delta 안 반영**
- Slave 로그에 `TAKE_CONTROL 수신 → Active` 나오는지
- 안 나오면 TCP 문제
- 나오는데 커서 안 움직이면: UDP 유실 or seq 검증 실패

**단축키 안 먹음**
- 다른 프로그램이 같은 조합 이미 잡고 있을 수 있음. config.hotkey_toggle 변경

**로그 파일**: `cursorlink.exe` 옆에 `cursorlink.log`. `set RUST_LOG=cursorlink=debug` 하고 실행하면 더 자세히.

---

## 8. 다음 세션에서 할 일 (우선순위 순)

1. **빌드 성공시키기** — 컴파일 에러 잡기. 위 5절 참고
2. **단일 PC 스모크 테스트** — master 모드로 실행, 트레이 뜨는지, 로그 정상인지
3. **두 PC (또는 두 VM) 로 실제 마우스 넘기기 테스트**
4. **엣지 감지 튜닝** — 왼쪽 끝 임계값이 `pt.x <= scr.left` 인데, 정상 사용 중 x=0 히트해서 오작동할 수 있음. 필요 시 debounce/threshold 조정
5. **지연 측정** — 로그의 `ts_us` 필드로 왕복 지연 계산
6. **DPI 스케일링 대응** — 두 PC 해상도/DPI 다르면 delta 스케일링 문제 있을 수 있음
7. **키보드 검증** — 특수키 (한/영, 한자, 미디어) 정상 전달되는지

**Phase 4 후보 (당장 안 함):**
- egui 기반 native 설정 창
- 커스텀 .ico 아이콘 embedded resource
- Inno Setup 인스톨러
- ChaCha20 UDP 페이로드 암호화
- run.bat 자동 업데이트 스크립트

---

## 9. 아키텍처 핵심 요약

**상태머신 (Master):**
```
Disconnected  ──TCP연결+HELLO OK──▶  Local
Local         ──커서 오른쪽 끝──▶     Remote (커서 락+숨김, TAKE_CONTROL 송신)
Remote        ──RETURN_CONTROL──▶    Local  (커서 언락+표시)
* ──TCP 끊김──▶ Disconnected (Local 강제 복귀)
```

**상태머신 (Slave):**
```
Disconnected  ──TCP accept+HELLO OK──▶  Idle
Idle          ──TAKE_CONTROL 수신──▶     Active (커서 표시)
Active        ──커서 왼쪽 끝──▶          Idle   (RETURN_CONTROL 송신, 커서 숨김)
* ──TCP 끊김──▶ Disconnected
```

**스레드 구성:**

Master:
- Main: Raw Input 메시지 펌프 (WM_INPUT + WM_HOTKEY + WM_TRAY + WM_COMMAND)
- cursorlink-tcp: TCP 워커 (connect/reconnect + reader)
- cursorlink-tcp-w: TCP writer (rx 채널 → sock.write)
- cursorlink-hb: HEARTBEAT ticker (3초)

Slave:
- Main: UDP recv 블로킹 루프
- cursorlink-tcp-srv: TCP 서버 (accept + HELLO 검증 + reader)
- cursorlink-tcp-w: TCP writer
- cursorlink-hb: HEARTBEAT ticker
- cursorlink-tray: Hotkey + 트레이 hidden window 메시지 루프

**UDP 패킷 (20바이트):**
```
0..4   seq (u32)
4..8   ts_us (u32)
8      kind (u8, 0=Move / 1=Btn / 2=Wheel / 3=Key / 4=Heartbeat)
9      flags (u8)
10..12 button (u16, mouse button or scan code)
12..14 dx (i16)
14..16 dy (i16)
16..18 wheel_dx (i16)
18..20 wheel_dy (i16)
```

**TCP 프레임 (8바이트):**
```
0    msg_type (0x01 TAKE_CONTROL / 0x02 RETURN_CONTROL / 0x03 HEARTBEAT / 0x04 HELLO)
1..8 payload (msg_type 별 해석)
```

---

## 10. 참고

- **Mac 개발 환경:** `/Users/jeongjiyong/IdeaProjects/cursorlink`
- **작성자 계정:** jiyong2ya (GitHub)
- **관련 프로젝트:** game-macro (커널 드라이버 기반, 이건 완전 별개)
- **Rust 버전:** 1.70+ (edition 2021)
- **타겟:** `x86_64-pc-windows-msvc` (기본)
