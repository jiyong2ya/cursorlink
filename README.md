# cursorlink

두 Windows PC 사이에서 마우스/키보드를 공유하는 KVM 유틸.
Multiplicity / Input Director / Barrier 같은 프로그램이지만 유저모드 전용, 드라이버/서명 없음.

## 빌드 (Windows)

```
cargo build --release
```

## 실행

1. `config.example.toml` 을 `config.toml` 로 복사
2. 양쪽 PC 에서 mode 와 peer_ip 를 각각 설정
3. `cursorlink.exe` 실행 (양쪽 모두)

## Phase 1 (현재)

- Master 의 마우스 델타를 UDP 로 Slave 에 전송
- Slave 는 델타를 받아 커서를 이동
- 엣지 감지 없음 (마우스 이동이 항상 slave 에도 반영됨)

Phase 2 에서 엣지 감지, TCP 제어, 키보드 추가.
Phase 3 에서 트레이/UI/단축키/자동시작 추가.
