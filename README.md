# claude-gateway

Claude Code CLI를 래핑하는 Rust 네이티브 REST API 게이트웨이.
단일 바이너리로 배포되며, Claude Code 월정액 구독만으로 API 키 없이 동작합니다.

## 개요

```
Client  ──HTTP/SSE──▶  claude-gateway  ──stdin/stdout──▶  claude CLI (subprocess)
```

- **단일 바이너리**: Rust로 빌드, Node.js는 CLI 실행에만 필요
- **세션 관리**: Multi-turn 대화, 세션 fork, 히스토리 조회
- **실시간 스트리밍**: SSE(Server-Sent Events)로 응답 실시간 수신
- **Hook 시스템**: 도구 실행 전 승인/차단/위임 제어
- **MCP 지원**: 외부 MCP 서버 연결

## 사전 요구사항

| 항목 | 설명 |
|------|------|
| **Claude Code CLI** | `npm install -g @anthropic-ai/claude-code` |
| **Claude Code 구독** | 월정액 (API 키 불필요) |
| **Node.js** | v18+ (CLI 실행용) |

```bash
# CLI 설치 확인
claude --version
```

## 빌드

```bash
cargo build --release
```

바이너리: `target/release/claude-agent-rs`

## 실행

```bash
# 기본 (127.0.0.1:8765)
./claude-agent-rs

# 포트/호스트 지정
./claude-agent-rs --port 9000 --host 0.0.0.0

# CLI 설치 확인만
./claude-agent-rs --check-cli
```

### 설정

`config.toml` (선택):

```toml
[server]
host = "127.0.0.1"
port = 8765
max_sessions = 100

[cli]
bin_path = ""                    # 빈 문자열 = PATH 자동 탐색
session_idle_timeout_secs = 1800 # 30분
```

환경변수: `CLAUDE_GATEWAY__SERVER__PORT=9000`

로그: `RUST_LOG=debug ./claude-agent-rs`

## API

### 관리

```bash
GET  /health    # 서버 상태
GET  /stats     # 누적 통계 (tokens, cost)
GET  /config    # 현재 설정
```

### 단일 쿼리

```bash
# 동기 응답
curl -X POST http://localhost:8765/query \
  -H "Content-Type: application/json" \
  -d '{"prompt": "2+2는?", "options": {"max_turns": 1, "permission_mode": "plan"}}'

# SSE 스트리밍
curl -N -X POST http://localhost:8765/query/stream \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "options": {"max_turns": 1, "permission_mode": "plan"}}'
```

### 세션 (Multi-turn)

```bash
# 생성
curl -X POST http://localhost:8765/sessions \
  -H "Content-Type: application/json" \
  -d '{"options": {"permission_mode": "plan"}}'

# 메시지 전송
curl -X POST http://localhost:8765/sessions/{id}/send \
  -H "Content-Type: application/json" \
  -d '{"message": "내 이름은 Alex야"}'

# SSE 구독 (히스토리 포함)
curl -N http://localhost:8765/sessions/{id}/stream

# 히스토리 조회
curl "http://localhost:8765/sessions/{id}/messages?limit=50"

# 세션 목록 / 삭제 / 분기 / 중단
GET    /sessions
DELETE /sessions/{id}
POST   /sessions/{id}/fork
POST   /sessions/{id}/interrupt
```

### Hook 시스템

세션 생성 시 서버사이드 규칙 설정:

```json
{
  "options": {
    "hook_rules": [
      {"event": "PreToolUse", "tool_pattern": "Read", "action": "approve"},
      {"event": "PreToolUse", "tool_pattern": "Bash", "action": {"block": {"reason": "Bash 차단"}}},
      {"event": "PreToolUse", "tool_pattern": "*", "action": "defer"}
    ]
  }
}
```

SSE로 수신한 `hook_request`에 30초 내 응답:

```bash
curl -X POST http://localhost:8765/sessions/{id}/hook_response \
  -H "Content-Type: application/json" \
  -d '{"hook_id": "hook-001", "decision": "approve"}'
```

### MCP 서버 연결

```json
{
  "options": {
    "mcp_servers": {
      "filesystem": {
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
      }
    }
  }
}
```

## 아키텍처

```
src/
├── bin/server.rs      # 엔트리포인트 (axum 서버)
├── api/               # HTTP 핸들러 (sessions, hooks, query, admin)
├── session/           # 세션 상태 관리 + store
├── transport/         # CLI subprocess 통신 (stdin/stdout NDJSON)
├── messages/          # 메시지 타입 (cli_input, cli_output, content)
���── hooks/             # Hook 자동 규칙 + 타임아웃
├── mcp/               # MCP 서버 설��� 파일 생성
├── permissions/       # 권한 모드
├── client.rs          # 세션 생성 + 이벤트 루프
├── query.rs           # 단일 쿼리 / 스트리밍 쿼리
├── config.rs          # 설정 로드 (TOML + 환경변수)
├── options.rs         # ClaudeAgentOptions
└── error.rs           # 에러 타입 + HTTP 매핑
```

## 에러 응답

```json
{
  "error": {
    "code": "session_not_found",
    "message": "Session abc123 not found"
  }
}
```

| code | HTTP | 설명 |
|------|------|------|
| `cli_not_found` | 503 | claude CLI 미설치 |
| `cli_connection` | 502 | subprocess spawn 실패 |
| `process_error` | 502 | CLI 비정상 종료 |
| `session_not_found` | 404 | 세션 없음 |
| `invalid_state` | 409 | ���못된 상태 전이 |
| `rate_limited` | 429 | 동시 세션 한도 초과 |
| `hook_timeout` | 408 | hook 응답 30초 초과 |

## 라이선스

MIT
