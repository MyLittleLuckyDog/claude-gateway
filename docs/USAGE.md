# claude-agent-rs 사용 가이드

> Claude Code CLI를 래핑하는 REST API 게이트웨이. 단일 바이너리로 배포.

---

## 1. 사전 요구사항

| 항목 | 설명 |
|------|------|
| **Claude Code CLI** | `npm install -g @anthropic-ai/claude-code` 로 설치. `claude` 명령이 PATH에 있어야 함 |
| **Claude Code 구독** | CLI가 사용하는 토큰 풀 (월정액). API 키 불필요 |
| **Node.js** | Claude CLI 실행에 필요 (v18+) |

설치 확인:
```bash
claude --version
# 예: 2.1.81
```

---

## 2. 실행

### 기본 실행
```bash
./claude-agent-rs
# 기본: http://127.0.0.1:8765
```

### 옵션
```bash
./claude-agent-rs --port 9000 --host 0.0.0.0
```

| 플래그 | 기본값 | 설명 |
|--------|--------|------|
| `--port` | 8765 | 리스닝 포트 |
| `--host` | 127.0.0.1 | 바인드 주소 |
| `--check-cli` | - | CLI 설치 확인 후 종료 |

### 설정 파일 (선택)

프로젝트 루트에 `config.toml` 배치:
```toml
[server]
host = "127.0.0.1"
port = 8765
max_sessions = 100                  # CLI-wrap 세션 상한
cors_origins = ["http://localhost", "http://127.0.0.1"]  # 빈 배열 = 모두 허용

[cli]
bin_path = ""                       # 빈 문자열 = PATH에서 자동 탐색
session_idle_timeout_secs = 1800    # 30분 유휴 시 세션 자동 정리

[proxy]
enabled = true                      # /v1/* 라우트 on/off
max_concurrent = 1                  # 동시 API 호출 수
max_proxy_sessions = 50             # Proxy 세션 상한
session_idle_timeout_secs = 1800    # Proxy 세션 idle timeout
```

환경변수로도 설정 가능 (섹션/필드 구분은 `__`):
```bash
CLAUDE_GATEWAY__SERVER__PORT=9000 ./claude-agent-rs
CLAUDE_GATEWAY__PROXY__MAX_CONCURRENT=4 ./claude-agent-rs
```

기타 환경변수:

| 변수 | 용도 |
|------|------|
| `RUST_LOG` | 로그 레벨 (`info` / `debug` / `claude_agent=debug`) |
| `CLAUDE_CONFIG_DIR` | Claude Code 설정 디렉터리 (credentials 읽기 경로) |
| `CLAUDE_CODE_CUSTOM_OAUTH_URL` | 사내망 OAuth 엔드포인트 override (allowlist 필수) |
| `USE_LOCAL_OAUTH`, `USE_STAGING_OAUTH`, `USER_TYPE` | Anthropic 내부 테스트용 OAuth 런타임 선택 |
| `CLAUDE_LOCAL_OAUTH_API_BASE` | 로컬 OAuth 서버 베이스 URL |
| `CLAUDE_GATEWAY_SKIP_KEYCHAIN=1` | **테스트 전용**. macOS Keychain 우회 (원본 토큰 보호). |

### 로그 레벨
```bash
RUST_LOG=info ./claude-agent-rs   # 기본
RUST_LOG=debug ./claude-agent-rs  # CLI 통신 상세 로그
```

---

## 3. API 레퍼런스

### 3.1 관리

#### `GET /health`
서버 상태 확인.
```bash
curl http://localhost:8765/health
```
```json
{
  "status": "ok",
  "version": "0.1.0",
  "cli_available": true,
  "cli_path": "/usr/local/bin/claude",
  "active_sessions": 2,
  "max_sessions": 100
}
```

#### `GET /stats`
누적 통계.
```bash
curl http://localhost:8765/stats
```
```json
{
  "uptime_seconds": 3600,
  "total_queries": 142,
  "active_sessions": 2,
  "total_input_tokens": 45200,
  "total_output_tokens": 12300,
  "total_cost_usd": 0.107
}
```

#### `GET /config`
현재 서버 설정 조회.

#### Proxy 모드 모니터링 (`/v1/*`)

| 엔드포인트 | 설명 |
|-----------|------|
| `GET /v1/auth_status` | OAuth 토큰 유효성, 구독 종류, rate_limit tier |
| `GET /v1/rate_limit` | 5h/7d 사용률, 리셋 시각, `allowed/warning/rejected` 상태 |
| `GET /v1/proxy_stats` | 누적 요청/입출력 토큰, 동시 호출 여유 |

---

### 3.2 Proxy 모드 (`/v1/*`) — Direct Messages API

Claude Code OAuth 토큰으로 `api.anthropic.com/v1/messages`를 직접 호출합니다.
CLI subprocess 없이 빠르고, 7종 모델을 자유롭게 섞어쓸 수 있습니다.
상세 모델 별칭표는 [README.md](../README.md#모델-별칭) 참조.

#### `POST /v1/messages` — 단일 요청 (동기)
```bash
curl -X POST http://localhost:8765/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "haiku",
    "max_tokens": 100,
    "system": "계산기. 숫자만 답해.",
    "messages": [{"role": "user", "content": "2+2"}]
  }'
```
`max_tokens` 생략 시 기본 8000.
`POST /v1/messages`는 `model`이 필수입니다. 기본 모델 보정은 `/v1/sessions`
세션 경로에서만 적용됩니다.

#### `POST /v1/messages/stream` — SSE 스트리밍
동일 body로 `event: message_start`, `content_block_delta`, `message_stop` SSE 이벤트를 받습니다.

#### Proxy 세션 (멀티턴)

| 엔드포인트 | 설명 |
|-----------|------|
| `POST /v1/sessions` | 세션 생성. body: `{model, system?, max_tokens?, temperature?, tools?, tool_choice?, betas?}` |
| `GET /v1/sessions` | 세션 목록 |
| `GET /v1/sessions/:id` | 세션 상태 + 메시지 배열 |
| `DELETE /v1/sessions/:id` | 세션 삭제 |
| `POST /v1/sessions/:id/msg` | 메시지 전송 (이전 대화 자동 포함) |
| `POST /v1/sessions/:id/msg/stream` | SSE 스트리밍 버전 |

tool_use 라운드트립은 `POST /v1/sessions/:id/msg` body에 `{"is_tool_result": true, "content": [{"type":"tool_result","tool_use_id":"...","content":"..."}]}` 를 실어보냅니다 (상세: [README.md](../README.md#tool_use-라운드트립)).

---

### 3.3 단일 쿼리 (세션 없음, CLI wrap)

#### `POST /query`
한 번 질의하고 결과를 JSON으로 받음.

```bash
curl -X POST http://localhost:8765/query \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "2+2는?",
    "options": {
      "max_turns": 1,
      "permission_mode": "plan"
    }
  }'
```
```json
{
  "session_id": "uuid-...",
  "result": "4",
  "subtype": "success",
  "cost_usd": 0.004,
  "usage": {
    "input_tokens": 512,
    "output_tokens": 5
  },
  "num_turns": 1,
  "duration_ms": 2400
}
```

#### `POST /query/stream`
SSE(Server-Sent Events)로 실시간 스트리밍.

```bash
curl -N -X POST http://localhost:8765/query/stream \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "options": {"max_turns": 1, "permission_mode": "plan"}}'
```

응답 (SSE 형식):
```
data: {"type":"system","session_id":"...","subtype":"init","tools":[...],"model":"claude-opus-4-6[1m]"}

data: {"type":"assistant","session_id":"...","message":{"content":[{"type":"text","text":"Hello!"}],...}}

data: {"type":"result","session_id":"...","subtype":"success","result":"Hello!",...}

data: [DONE]
```

---

### 3.4 세션 (Multi-turn 대화, CLI wrap)

#### `POST /sessions` — 세션 생성
```bash
curl -X POST http://localhost:8765/sessions \
  -H "Content-Type: application/json" \
  -d '{"options": {"permission_mode": "plan"}}'
```
```json
{"session_id": "abc-123", "state": "initializing"}
```

#### `POST /sessions/:id/send` — 메시지 전송
```bash
curl -X POST http://localhost:8765/sessions/abc-123/send \
  -H "Content-Type: application/json" \
  -d '{"message": "내 이름은 Alex야"}'
```
응답: `202 Accepted` (비동기 — 결과는 SSE로 수신)

이미지 첨부:
```json
{
  "message": "이 이미지를 분석해줘",
  "image_base64": "iVBOR...",
  "image_media_type": "image/png"
}
```

#### `GET /sessions/:id/stream` — SSE 구독
```bash
curl -N http://localhost:8765/sessions/abc-123/stream
```
세션의 모든 이벤트를 실시간으로 수신. 기존 히스토리도 먼저 재전송됨.

#### `GET /sessions/:id/messages` — 히스토리 조회
```bash
curl "http://localhost:8765/sessions/abc-123/messages?limit=50&offset=0&include_system=false"
```
```json
{
  "session_id": "abc-123",
  "total": 10,
  "messages": [...]
}
```

#### `GET /sessions` — 세션 목록
```bash
curl http://localhost:8765/sessions
```

#### `DELETE /sessions/:id` — 세션 삭제
```bash
curl -X DELETE http://localhost:8765/sessions/abc-123
```

#### `POST /sessions/:id/fork` — 세션 분기
기존 세션의 대화를 복제하여 새 세션 생성.
```bash
curl -X POST http://localhost:8765/sessions/abc-123/fork
```
```json
{"session_id": "new-456", "state": "initializing"}
```

#### `POST /sessions/:id/interrupt` — 응답 중단
```bash
curl -X POST http://localhost:8765/sessions/abc-123/interrupt
```

---

### 3.5 Hook 시스템

Claude가 도구(Edit, Bash 등)를 실행하기 전 hook 이벤트가 발생. 두 가지 처리 방식:

**A. 서버사이드 자동 규칙** — 세션 생성 시 설정:
```json
{
  "options": {
    "hook_rules": [
      {"event": "PreToolUse", "tool_pattern": "Bash", "action": {"block": {"reason": "Bash 차단"}}},
      {"event": "PreToolUse", "tool_pattern": "Read", "action": "approve"},
      {"event": "PreToolUse", "tool_pattern": "*", "action": "defer"}
    ]
  }
}
```
- `block` — 도구 실행 차단
- `approve` — 자동 승인
- `defer` — SSE로 클라이언트에 위임

**B. 클라이언트 응답** — SSE에서 `hook_request` 수신 시 timeout 내 응답:
```bash
# SSE에서 수신 (request_id는 CLI 제어 프로토콜의 control_request.request_id):
# data: {"type":"hook_request","request_id":"req-001","callback_id":"hook_0",
#        "hook_event_name":"PreToolUse","tool_name":"Edit","tool_use_id":"toolu_..."}

# hook_timeout_secs(기본 30초) 안에 응답 — decision + reason 혹은 response raw 중 하나:
curl -X POST http://localhost:8765/sessions/abc-123/hook_response \
  -H "Content-Type: application/json" \
  -d '{"request_id": "req-001", "decision": "approve"}'

# 또는 control_response.response 전체를 직접 지정:
curl -X POST http://localhost:8765/sessions/abc-123/hook_response \
  -H "Content-Type: application/json" \
  -d '{"request_id": "req-001", "response": {"decision": "block", "reason": "policy"}}'
```

Permission prompt도 동일하게 세션 스트림으로 surface됩니다:

```bash
# data: {"type":"permission_request","request_id":"req-perm-001",
#        "tool_name":"Bash","input":{"command":"..."}} 

curl -X POST http://localhost:8765/sessions/abc-123/permission_response \
  -H "Content-Type: application/json" \
  -d '{"request_id": "req-perm-001", "behavior": "allow"}'
```

`POST /query`, `POST /query/stream`는 stateless 경로라 deferred hook callback과
tool permission prompt를 대화형으로 처리하지 않습니다. 이 경로에서는 해당 요청이
자동 block/deny 처리됩니다.

decision 값:
| 값 | 설명 |
|----|------|
| `approve` | 도구 실행 허용 |
| `block` | 도구 실행 차단 (reason 권장) |
| `defer` | CLI 기본 동작 |

**타임아웃 시 기본 동작은 자동 `block` 입니다** (승인이 아닙니다 — 응답이 없으면
도구가 실행되지 않습니다). 대기 시간은 `hook_timeout_secs`(기본 30초), 타임아웃
동작은 `hook_timeout_action`(`block` | `approve`, 기본 `block`)으로 요청마다
바꿉니다.

타임아웃이 지난 뒤 `hook_response` 를 보내면 `408 hook_timeout` 이 돌아옵니다.
이 판정은 `hook_timeout_secs` 와 같은 값을 씁니다.

---

### 3.6 MCP 서버 연결

외부 MCP(Model Context Protocol) 서버를 세션에 연결:

```json
{
  "options": {
    "mcp_servers": {
      "filesystem": {
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
      },
      "remote-api": {
        "type": "sse",
        "url": "http://localhost:3001/sse"
      }
    }
  }
}
```

---

## 4. options 전체 필드

`POST /query`, `POST /sessions` 의 `options` 필드:

| 필드 | 타입 | 기본값 | 설명 |
|------|------|--------|------|
| `system_prompt` | string | null | 시스템 프롬프트 |
| `model` | string | null | 모델 (예: `claude-sonnet-4-6`, 또는 별칭 `sonnet`) |
| `fallback_model` | string | null | 주 모델 불가 시 대체 모델 |
| `permission_mode` | string | `"default"` | `default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk` |
| `max_turns` | number | null | 최대 턴 수 |
| `max_budget_usd` | number | null | 비용 한도 (USD) |
| `allowed_tools` | string[] | null | 허용 도구 목록 |
| `disallowed_tools` | string[] | null | 차단 도구 목록 |
| `mcp_servers` | object | null | MCP 서버 설정 (3.6 참조) |
| `hook_rules` | array | null | 서버사이드 훅 규칙 |
| `include_hook_events` | bool | false | CLI hook lifecycle event 포함 (`--include-hook-events`) |
| `hook_timeout_secs` | number | `30` | deferred hook callback 대기 시간 |
| `hook_timeout_action` | string | `"block"` | hook timeout 시 동작: `block`, `approve` |
| `resume` | string | null | 이전 세션 재개 (CLI session_id) |
| `continue_conversation` | bool | false | 가장 최근 세션 계속 |
| `fork_session` | string | null | resume/continue 시 새 session ID로 분기 (`--fork-session`) |
| `cwd` | string | null | CLI 작업 디렉토리 |
| `env` | object | null | CLI 환경변수 |
| `cli_path` | string | null | claude CLI 경로 (기본: PATH 탐색) |
| `add_dirs` | string[] | null | CLI에 추가로 노출할 작업 디렉토리(`--add-dir`) |
| `include_partial_messages` | bool | false | 스트리밍 중간 메시지 포함 |
| `output_format` | string | null | CLI wrap 경로에서는 `stream-json`만 지원 |
| `agents` | object | null | subagent 정의 맵. `initialize` control_request로 전달 |
| `betas` | string[] | null | 베타 기능 활성화 |
| `setting_sources` | string[] | `[""]` | 설정 소스 (빈 배열=격리, `user/project/local` 혼합) |

---

## 5. 에러 응답

모든 에러는 동일한 형식:
```json
{
  "error": {
    "code": "session_not_found",
    "message": "Session abc123 not found"
  }
}
```

| code | HTTP | 발생 상황 |
|------|------|----------|
| `cli_not_found` | 503 | claude CLI 미설치 |
| `cli_connection` | 502 | subprocess spawn 실패 |
| `process_error` | 502 | CLI 비정상 종료 |
| `json_decode` | 502 | CLI 출력 파싱 실패 |
| `session_not_found` | 404 | 존재하지 않는 session_id |
| `invalid_state` | 409 | 잘못된 상태에서 요청 |
| `rate_limited` | 429 | 동시 세션 한도 초과 |
| `hook_timeout` | 408 | hook 응답 timeout (`hook_timeout_secs`, 기본 30초) |
| `internal_error` | 500 | 내부 오류 |

---

## 6. 사용 예시

### Python 클라이언트
```python
import requests

BASE = "http://localhost:8765"

# 단일 쿼리
resp = requests.post(f"{BASE}/query", json={
    "prompt": "파이썬으로 피보나치 함수 작성해줘",
    "options": {"max_turns": 3, "permission_mode": "plan"}
})
print(resp.json()["result"])
```

### SSE 스트리밍 (Python)
```python
import requests

resp = requests.post(f"{BASE}/query/stream", json={
    "prompt": "hello",
    "options": {"max_turns": 1, "permission_mode": "plan"}
}, stream=True)

for line in resp.iter_lines():
    if line:
        decoded = line.decode()
        if decoded.startswith("data: "):
            print(decoded[6:])
```

### Multi-turn 세션
```python
import requests, time

# 세션 생성
session = requests.post(f"{BASE}/sessions", json={
    "options": {"permission_mode": "plan"}
}).json()
sid = session["session_id"]

# 첫 메시지
requests.post(f"{BASE}/sessions/{sid}/send", json={"message": "내 이름은 Alex야"})
time.sleep(5)

# 두 번째 메시지
requests.post(f"{BASE}/sessions/{sid}/send", json={"message": "내 이름이 뭐야?"})
time.sleep(5)

# 히스토리 확인
messages = requests.get(f"{BASE}/sessions/{sid}/messages").json()
for m in messages["messages"]:
    if m["type"] == "assistant":
        text = m["message"]["content"][0].get("text", "")
        print(f"Claude: {text}")

# 세션 삭제
requests.delete(f"{BASE}/sessions/{sid}")
```

---

## 7. 주의사항

- **Claude Code 구독 필요**: API 키가 아닌 CLI 세션 토큰 사용
- **동시 세션 제한**: 기본 100개 (config.toml로 변경 가능)
- **유휴 세션 자동 정리**: 기본 30분
- **subprocess 복구 불가**: CLI가 crash하면 새 세션 생성 필요
- **`permission_mode: "plan"`** 권장: 도구 실행 없이 응답만 받을 때
- **`setting_sources: [""]`** 기본 설정: CLI의 프로젝트 설정/훅을 무시하여 빠른 응답
