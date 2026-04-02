# claude-gateway 개선 TODO

## 현재 상태

- [x] axum + tokio HTTP 서버
- [x] POST /chat — Claude CLI 호출 + 응답 파싱
- [x] GET /health — 상태 + CLI 감지
- [x] spawn_blocking으로 비동기 CLI 호출
- [x] stream-json 파싱 (assistant/result 메시지)
- [x] CORS 허용
- [x] CLI 인자 (--port, --model)

## 개선 필요

### P1: 세션/대화 히스토리

- [ ] `--resume SESSION_ID` 플래그로 CLI 세션 유지 가능 여부 확인
- [ ] 세션 ID 기반 히스토리 관리 (HashMap<session_id, Vec<Message>>)
- [ ] `/chat` 요청에 `session_id` 필드 추가
- [ ] 히스토리를 CLI의 `--continue` 옵션으로 전달하는 방식 검토

### P2: 동시 요청 제한

- [ ] tokio::sync::Semaphore로 동시 CLI 호출 제한 (기본 3)
- [ ] 큐 대기 시 타임아웃 설정
- [ ] `/health`에 현재 동시 요청 수 표시

### P3: 에러 처리 개선

- [ ] 구조화된 에러 타입 (enum GatewayError)
- [ ] CLI 타임아웃 (기본 60초)
- [ ] CLI 프로세스 비정상 종료 처리
- [ ] rate_limit_event 감지 + 자동 대기

### P4: system_prompt 검증

- [ ] `claude -p --system-prompt` 플래그 동작 확인
- [ ] 대안: 프롬프트 앞에 시스템 메시지 삽입

### P5: 관측성

- [ ] 요청/응답 로깅 (토큰 수, 비용, 응답 시간)
- [ ] stream-json에서 usage 정보 추출 (input_tokens, output_tokens, cost)
- [ ] `/stats` 엔드포인트 — 누적 호출 수, 토큰, 비용

### P6: d8-rust 통합

- [ ] nav_chat.js에서 Python Gateway → claude-gateway로 전환
- [ ] nav_browser_poc.js 테스트
- [ ] sas-agent 크레이트에서 직접 호출 (HTTP 대신 라이브러리 통합 가능성)

### P7: 배포

- [ ] GitHub Actions CI/CD
- [ ] macOS / Linux / Windows 크로스 빌드
- [ ] 릴리스 바이너리 배포
