# cursorlink

Windows PC 여러 대 (마스터 1 + 슬레이브 최대 2) 사이에서 마우스/키보드를 공유하는 KVM 유틸.
Multiplicity / Input Director 같은 프로그램이지만 유저모드 전용, 드라이버/서명 없음.

```
[왼쪽 슬레이브] ← [마스터] → [오른쪽 슬레이브]
```

- 마스터 화면 오른쪽 끝 → 오른쪽 슬레이브, 왼쪽 끝 → 왼쪽 슬레이브 (쓸어넘기기)
- 슬레이브 화면에서 마스터 쪽 벽에 닿으면 마스터로 복귀
- 단축키로 슬레이브 이동 / 마스터 복귀
- 쓸어넘기기 on/off 단축키: 왼쪽만 / 오른쪽만 / 양쪽 (꺼도 단축키 이동과 복귀는 동작)
- Mirror 모드: 마스터 + 고른 슬레이브 동시 조작

## 빌드 (Windows)

```
cargo build --release
```

산출물: `target\release\cursorlink.exe`. 릴리스는 GitHub Releases 에 exe 로 올림.

## 실행

1. exe 옆에 `config.example.toml` 을 `config.toml` 로 복사 (첫 실행 시 자동 복사 + 메모장 열림)
2. 각 PC 에서 설정
   - 마스터: `mode = "master"`, `peer_ip` = 오른쪽 슬레이브, `left_peer_ip` = 왼쪽 슬레이브
   - 슬레이브: `mode = "slave"`, `peer_ip` = 마스터 (왼쪽/오른쪽 설정 필요 없음)
   - `shared_secret` 은 모든 PC 동일
3. `cursorlink.exe` 실행 (모든 PC), 방화벽 팝업 → 개인 네트워크 허용

### 슬레이브: 관리자 권한 실행 (권장)
슬레이브 config 에 `run_as_admin = true` (+ `autostart = true`).
관리자 권한 프로그램 창 (설치 프로그램, 작업관리자 등) 에서도 공유 마우스/키보드가 먹힘.
처음 실행할 때 UAC 확인창 한 번, 이후 로그인 때는 작업 스케줄러로 확인창 없이 관리자 실행.
UAC 확인창 ("예/아니요") 과 잠금화면은 이걸로도 조작 안 됨 → 슬레이브 UAC 를 "알리지 않음" 으로.

## 단축키 (config.toml)

| 설정 | 예 | 마스터 사용 중 | 슬레이브 사용 중 | 미러 중 |
|---|---|---|---|---|
| `hotkey_transfer_left` | `num/` | 왼쪽 슬레이브로 | (슬레이브에 일반 키) | 왼쪽 미러 넣기/빼기 |
| `hotkey_transfer` | `num-` | 오른쪽 슬레이브로 | (슬레이브에 일반 키) | 오른쪽 미러 넣기/빼기 |
| `hotkey_return` | `num.` | (일반 키) | 마스터로 복귀 | (일반 키) |
| `hotkey_mirror` | `num*` | 미러 켜기 | (슬레이브에 일반 키) | 미러 끄기 |
| `hotkey_mirror_left` / `_right` | `del` | (일반 키) | (일반 키) | 그 쪽 미러 넣기/빼기 전용 |
| `hotkey_edge_toggle` | `numplus` | 쓸어넘기기 양쪽 on/off | 〃 | 〃 |
| `hotkey_edge_left` | `ctrl+num/` | 왼쪽 쓸어넘기기만 on/off | 〃 | 〃 |
| `hotkey_edge_right` | `ctrl+num-` | 오른쪽 쓸어넘기기만 on/off | 〃 | 〃 |
| `hotkey_toggle` | `ctrl+alt+shift+k` | 기능 전체 on/off | 기능 끄고 복귀 | 기능 끄기 |

트레이 우클릭 메뉴에서 슬레이브 연결 상태, 쓸어넘기기 (왼쪽/오른쪽), 미러 대상도 보고 바꿀 수 있음.
쓸어넘기기 시작 상태는 `edge_switch_left` / `edge_switch_right`, 미러 대상 시작값은 `mirror_left` / `mirror_right`.
설정 항목 설명은 `config.example.toml` 주석 (첫 실행 때 이 파일이 주석 그대로 config.toml 로 만들어짐).

자세한 구조는 [HANDOFF.md](HANDOFF.md).
