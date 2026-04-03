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
max_sessions = 100

[cli]
bin_path = ""                    # 빈 문자열 = PATH에서 자동 탐색
session_idle_timeout_secs = 1800 # 30분 유휴 시 세션 자동 정리
```

환경변수로도 설정 가능:
```bash
CLAUDE_GATEWAY__SERVER__PORT=9000 ./claude-agent-rs
```

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

---

### 3.2 단일 쿼리 (세션 없음)

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

### 3.3 세션 (Multi-turn 대화)

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

### 3.4 Hook 시스템

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

**B. 클라이언트 응답** — SSE에서 `hook_request` 수신 시 30초 내 응답:
```bash
# SSE에서 수신:
# data: {"type":"hook_request","hook_id":"hook-001","hook_event_name":"PreToolUse","tool_name":"Edit",...}

# 30초 내 응답:
curl -X POST http://localhost:8765/sessions/abc-123/hook_response \
  -H "Content-Type: application/json" \
  -d '{"hook_id": "hook-001", "decision": "approve"}'
```

decision 값:
| 값 | 설명 |
|----|------|
| `approve` | 도구 실행 허용 |
| `block` | 도구 실행 차단 (reason 권장) |
| `defer` | CLI 기본 동작 |

30초 타임아웃 시 자동 `approve`.

---

### 3.5 MCP 서버 연결

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
| `model` | string | null | 모델 (예: `claude-sonnet-4-6`) |
| `permission_mode` | string | `"default"` | `default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk` |
| `max_turns` | number | null | 최대 턴 수 |
| `max_budget_usd` | number | null | 비용 한도 (USD) |
| `allowed_tools` | string[] | null | 허용 도구 목록 |
| `disallowed_tools` | string[] | null | 차단 도구 목록 |
| `mcp_servers` | object | null | MCP 서버 설정 (위 참조) |
| `hook_rules` | array | null | 서버사이드 훅 규칙 |
| `resume` | string | null | 이전 세션 재개 (CLI session_id) |
| `continue_conversation` | bool | false | 가장 최근 세션 계속 |
| `cwd` | string | null | CLI 작업 디렉토리 |
| `env` | object | null | CLI 환경변수 |
| `cli_path` | string | null | claude CLI 경로 (기본: PATH 탐색) |
| `include_partial_messages` | bool | false | 스트리밍 중간 메시지 포함 |
| `betas` | string[] | null | 베타 기능 활성화 |
| `setting_sources` | string[] | `[""]` | 설정 소스 (빈 배열=격리) |

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
| `hook_timeout` | 408 | hook 응답 30초 초과 |
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
