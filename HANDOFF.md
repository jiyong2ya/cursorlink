# cursorlink — 인수인계 문서

새 Claude 세션이 이 파일만 읽고 상황 파악할 수 있도록 자기완결적으로 작성.

---

## 1. 프로젝트 요약

**cursorlink** — Windows PC 사이에서 마우스/키보드를 공유하는 KVM 유틸.
마스터 1대 + 슬레이브 최대 2대 (왼쪽/오른쪽). 유저모드 전용, 드라이버 없음, 인증서 없음.

```
[왼쪽 슬레이브] ← [마스터] → [오른쪽 슬레이브]
```

**목적:** 사무용. 유료인 Multiplicity 대체. Input Director 는 사용자의 게임 (NGX 안티치트) 에서 튕겨서 자체 개발.
→ 안티치트 대응으로 LL 훅은 슬레이브 조작/미러 중에만 설치하고 마스터 사용 중엔 제거한다.

**동작 원리:**
- Master PC: 물리 마우스/키보드가 꽂힌 쪽. Raw Input (`WM_INPUT`) 으로 이벤트 캡처.
- 마스터 화면 오른쪽 끝 → 오른쪽 슬레이브, 왼쪽 끝 → 왼쪽 슬레이브로 제어권 이동 (쓸어넘기기).
- 이후 master 는 커서 락 + 숨김 + LL 훅으로 입력 소비. 이벤트를 UDP 로 슬레이브에 전송.
  슬레이브는 `SetCursorPos`/`SendInput` 으로 주입.
- 슬레이브 커서가 마스터 쪽 벽 (오른쪽 슬레이브는 왼쪽 벽, 왼쪽 슬레이브는 오른쪽 벽) 에 닿으면 복귀.
  어느 벽인지는 마스터가 TAKE_CONTROL 로 알려줌 → 슬레이브 config 엔 위치 설정 없음.
- TCP 로 제어 신호 (TAKE_CONTROL / RETURN_CONTROL / HEARTBEAT / HELLO).

---

## 2. 저장소 / 배포

- **URL:** https://github.com/jiyong2ya/cursorlink.git (branch `master`)
- **배포:** 로컬에서 `cargo build --release` → GitHub Releases 에 `cursorlink.exe` 첨부.
  슬레이브 PC 는 Releases 에서 exe 받아서 씀. CI 없음.
- 마스터 PC 는 `target\release\cursorlink.exe` 를 직접 실행 (autostart 레지스트리도 이 경로).
  슬레이브는 `run_as_admin = true` 권장 (아래 6절).
  실행 중이면 exe 가 잠겨서 `cargo build --release` 가 링크 단계에서 실패 → 종료 후 빌드하거나
  `CARGO_TARGET_DIR` 을 다른 곳으로.

---

## 3. 파일 구조

```
cursorlink/
├── Cargo.toml              — 의존성 (windows 0.58, serde, toml, crossbeam-channel, tracing)
├── build.rs                — 아이콘 리소스 embed (winresource)
├── config.example.toml     — 첫 실행 시 config.toml 로 복사됨 (설정 설명은 여기 주석)
├── assets/                 — 아이콘
└── src/
    ├── main.rs             — 진입점, DPI awareness, autostart 동기화, mode 별 dispatch
    ├── config.rs           — TOML 로드, 첫 실행 시 notepad 자동 오픈
    ├── logging.rs          — 파일 로거 (release 는 stdout 안 뜨니 필수)
    ├── state.rs            — Side, MasterShared / SlaveShared (Atomic 상태)
    ├── hotkey.rs           — RegisterHotKey, 단축키 파싱, 훅용 압축 (pack_for_hook), slave 트레이 창
    ├── autostart.rs        — HKCU\Run 레지스트리 조작
    ├── tray.rs             — 트레이 아이콘, 우클릭 메뉴, 알림 (토스트)
    ├── master.rs           — 마스터 오케스트레이션: 슬레이브별 TCP 워커, 상태 전환, 미러
    ├── slave.rs            — 슬레이브 오케스트레이션 (TCP 서버 + 하트비트 + watchdog + 트레이)
    ├── net/
    │   ├── packet.rs       — 20바이트 UDP 입력 패킷 encode/decode + 테스트
    │   ├── udp.rs          — bind_recv / bind_send
    │   └── tcp.rs          — 8바이트 제어 프레임 + connect/listen + 테스트
    └── input/
        ├── capture.rs      — [master] message window, Raw Input → 엣지 감지 / UDP 송신, 눌린 키 추적
        ├── hooks.rs        — [master] LL 훅: Remote 중 입력 소비 + 단축키 감지, 미러 중 키 forward
        ├── inject.rs       — [slave] UDP 수신 → SetCursorPos / SendInput, 복귀 벽 감지
        └── cursor.rs       — GetCursorPos / SetCursorPos / ClipCursor / ShowCursor 헬퍼
```

---

## 4. 설정 / 단축키

설정 설명은 `config.example.toml` 주석 참고. 이 파일은 `include_str!` 로 exe 에 내장돼서
첫 실행 때 (exe 옆에 config.toml 이 없으면) 주석 그대로 config.toml 로 만들어짐. 핵심:
- 마스터 `peer_ip` = 오른쪽 슬레이브, `left_peer_ip` = 왼쪽 슬레이브 (빈 문자열 = 없음)
- 슬레이브 `peer_ip` = 마스터
- 단축키 파싱: `+` 가 구분자라 numpad + 는 `numplus` 로 씀

| 단축키 | Local | Remote (슬레이브 조작 중) | Local + Mirror |
|---|---|---|---|
| hotkey_transfer_left | 왼쪽으로 (중앙 진입) | 일반 키 (슬레이브로 forward) | 왼쪽 미러 대상 넣기/빼기 |
| hotkey_transfer | 오른쪽으로 (중앙 진입) | 일반 키 (슬레이브로 forward) | 오른쪽 미러 대상 넣기/빼기 |
| hotkey_return | 일반 키 | 마스터 복귀 | 일반 키 |
| hotkey_mirror | 미러 ON | 일반 키 (슬레이브로 forward) | 미러 OFF |
| hotkey_mirror_left / _right | 일반 키 | 일반 키 | 그 쪽 미러 대상 넣기/빼기 (훅 전용) |
| hotkey_edge_toggle | 쓸어넘기기 양쪽 on/off | 〃 | 〃 |
| hotkey_edge_left / _right | 그 쪽 쓸어넘기기만 on/off | 〃 | 〃 |
| hotkey_toggle | 기능 on/off | 기능 off + 복귀 | 기능 off + 미러 off |

- Local / Mirror 에선 `RegisterHotKey` 로 받음.
- Remote 에선 LL 훅이 키를 소비해서 RegisterHotKey 가 안 불림 → 같은 단축키를 훅 테이블에도 넣어
  훅 안에서 (modifier 직접 추적해서) 매칭. 처리한 키의 오토리피트/떼기는 조용히 소비.
  Remote 에서 훅이 처리하는 건 복귀 / 쓸어넘기기 / toggle 만 (`hooks::handled_while_remote`),
  Mirror 에선 미러 대상 전용 키만 (`handled_while_mirror`). 복귀·미러 대상 키는 RegisterHotKey 안 함
  → 그 상태가 아닐 땐 일반 키. 같은 키를 상태별로 다른 기능에 써도 됨 (`find_hotkey_where`).
  넘패드 Del (NumLock 꺼짐, 비확장) 은 매칭 때 num. 로 취급, 일반 Delete (확장) 는 del.
  이동·미러 키는 슬레이브에 일반 키로 보냄 → 슬레이브끼리 바로 이동은 없음 (v0.2.2 에서 제거, 넘패드 입력 우선).
- 쓸어넘기기는 왼쪽/오른쪽 따로 on/off (`MasterShared::edge_enabled[side]`, 시작값 `edge_switch_left/right`).
  양쪽 토글은 하나라도 켜져 있으면 다 끄고, 다 꺼져 있으면 다 켬.
- 쓸어넘기기 off 는 마스터 → 슬레이브 방향만 막음. 슬레이브 → 마스터 복귀 (벽) 는 항상 동작.

---

## 5. 아키텍처

**상태머신 (Master):** `Local` / `Remote(active side)` + 슬레이브별 `connected` 플래그
```
Local     ──화면 오른쪽 끝 / hotkey_transfer──────▶  Remote(오른쪽)  (TAKE_CONTROL, 커서 락+숨김, 훅 install)
Local     ──화면 왼쪽 끝 / hotkey_transfer_left───▶  Remote(왼쪽)
Remote    ──RETURN_CONTROL / hotkey_return / 연결 끊김──▶ Local (커서 언락+원위치, 훅 uninstall)
```
- 상태 전환은 전부 메인 스레드에서만 (커서/훅 API 가 스레드에 묶임).
  TCP 스레드는 `PostMessage(WM_PEER_UP / DOWN / RETURN)` 로 메인 스레드에 넘김.
- 슬레이브를 떠날 때 (복귀/미러 해제) 그 슬레이브에 눌린 채인 키/버튼 key-up 전송 (stuck key 방지).

**Mirror (Local 에서만):** 마스터 + 미러 대상 슬레이브 동시 조작.
- 미러 대상은 슬레이브별 on/off, 미러 끈 뒤에도 기억 (시작값 `mirror_left/right`). 다 빼놓고 켜면 양쪽으로 리셋.
- 커서는 화면 비율 (MousePos 패킷) 로 동기화, 키는 훅에서 forward (소비 X).

**상태머신 (Slave):**
```
Disconnected  ──TCP accept+HELLO OK──▶  Idle   (UDP seq 리셋: 마스터 재시작 대응)
Idle          ──TAKE_CONTROL──▶          Active (진입 위치로 이동, 복귀 벽 기억)
Active        ──복귀 벽 도달──▶          Idle   (RETURN_CONTROL 송신)
Active        ──RETURN_CONTROL (마스터)──▶ Idle
* ──TCP 끊김──▶ Disconnected
```
- watchdog: Active 중 secure desktop (UAC/잠금화면) 감지 → 자동 RETURN_CONTROL.
- disabled 상태에서 TAKE_CONTROL 받으면 바로 RETURN_CONTROL 응답 (마스터 갇힘 방지).

**스레드:**
- Master: main (메시지 펌프: WM_INPUT / WM_HOTKEY / 트레이 / WM_PEER_* / LL 훅),
  `cursorlink-tcp-left|right` (슬레이브별 연결 + reader) + `cursorlink-tcp-w-*` (writer), `cursorlink-hb`
- Slave: main (UDP recv), `cursorlink-tcp-srv`, `cursorlink-tcp-w`, `cursorlink-hb`, `cursorlink-watchdog`, `cursorlink-tray`

**UDP 패킷 (20바이트, LE):**
```
0..4   seq (u32)      — 슬레이브가 오래된/중복 패킷 폐기 (HELLO 마다 리셋)
4..8   ts_us (u32)
8      kind (u8, 0=Move / 1=Btn / 2=Wheel / 3=Key / 4=Heartbeat / 5=MousePos)
9      flags (u8, bit0=down, bit1=확장키)
10..12 button (u16, mouse button or scan code)
12..14 dx (i16)       — MousePos 면 x 비율 (0..10000)
14..16 dy (i16)       — MousePos 면 y 비율
16..18 wheel_dx (i16)
18..20 wheel_dy (i16)
```

**TCP 프레임 (8바이트):**
```
0    msg_type (0x01 TAKE_CONTROL / 0x02 RETURN_CONTROL / 0x03 HEARTBEAT / 0x04 HELLO)
1..8 payload
     TAKE_CONTROL: [entry_side, entry_y_pct, return_side, 0...]
       entry_side: 0=왼쪽 벽, 1=오른쪽 벽, 4=화면 중앙 / return_side: 0=왼쪽 벽 (구버전 기본), 1=오른쪽 벽
     HELLO: shared_secret 앞 7바이트
```

---

## 6. 트러블슈팅

**로그:** exe 옆 `cursorlink.log`. 트레이 우클릭 → 로그 보기. 자세히: `RUST_LOG=cursorlink=debug`.

- **슬레이브 연결 안 됨:** 트레이 메뉴에 슬레이브별 연결 상태 표시. peer 주소 / 방화벽 (46011/UDP, 46012/TCP) / `shared_secret` 확인.
  노트북 등에서 네트워크가 "공용" 이면 방화벽 허용이 개인 네트워크에만 걸려 막힐 수 있음 → 네트워크를 개인으로.
- **슬레이브 팝업에서 클릭/키가 안 먹음:** 두 종류.
  1) 관리자 권한 프로그램 창 (UIPI: 낮은 권한 프로세스의 SendInput 은 높은 권한 창에 안 들어감)
     → 슬레이브 `run_as_admin = true`. autostart 와 같이 켜면 작업 스케줄러 ("cursorlink" 작업,
     로그온 시 가장 높은 권한) 로 확인창 없이 관리자 실행 (`autostart::sync`). HKCU\Run 은 관리자로 못 띄움.
     작업 XML 은 배터리/3일 제한/우선순위 기본값을 직접 덮어씀 (`autostart::task_xml`).
  2) 보안 데스크톱 (UAC 확인창, 잠금화면, Ctrl+Alt+Del) → 관리자 권한으로도 불가 (SYSTEM + Winlogon
     데스크톱 필요 = Input Director 등이 쓰는 서비스 방식). 지금은 watchdog 이 마스터로 자동 복귀만 함.
     실사용 우회: 슬레이브 UAC 를 "알리지 않음" + 화면 잠금 끄기.
- **노트북 덮개 닫기 / 절전:** TCP 는 이걸 바로 알려주지 않음. 양쪽 다 하트비트 (3초) 가 10초 (`tcp::PEER_TIMEOUT`) 동안
  안 오면 끊김 처리 → 그 슬레이브를 조작 중이었으면 마스터로 자동 복귀, 3초마다 재연결 시도.
- **단축키 안 먹음:** 로그에 `RegisterHotKey ... 실패` → 다른 프로그램 (또는 cursorlink 가 두 번 실행) 이 같은 키 사용 중.
- **넘어갔는데 바로 튕겨 돌아옴:** 슬레이브 진입 위치가 벽에서 10px 안쪽이라 반대로 조금만 움직여도 복귀.
- **멀티 모니터:** 엣지/진입 계산이 주 모니터 (`primary_screen`) 기준. 슬레이브가 모니터 여러 개면 주 모니터 끝에서 복귀함.

---

## 7. 다음 후보

- 멀티 모니터 대응 (가상 스크린 기준 엣지)
- DPI 다른 PC 간 delta 스케일링
- ChaCha20 UDP 페이로드 암호화
- egui 설정 창 / 인스톨러
