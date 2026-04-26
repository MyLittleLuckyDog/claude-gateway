# claude-gateway

Claude Code CLI를 래핑하는 Rust 네이티브 REST API 게이트웨이.
단일 바이너리로 배포되며, Claude Code 구독만으로 API 키 없이 동작합니다.

아키텍처 확장 방향은 [docs/MULTI_PROVIDER_ARCHITECTURE.md](/Volumes/juryu_home/with_AI/projects/06.DenoV8POC/01.Tools/claude-gateway/docs/MULTI_PROVIDER_ARCHITECTURE.md:1)를 참고하세요.
현재 기준은 `기존 Claude 경로 유지 + Codex 축 추가 + 얇은 공통층 추출`입니다.

## 두 가지 모드

| 모드 | 경로 | 설명 |
|------|------|------|
| **Proxy (Direct API)** | `/v1/*` | OAuth 토큰으로 Messages API 직접 호출. 빠르고 모델 선택 자유 |
| **CLI Wrap** | `/query`, `/sessions` | Claude Code CLI를 subprocess로 실행. 도구 내장 |

```
                          ┌─ /v1/*  ──▶  api.anthropic.com (OAuth Bearer)
Client ──HTTP──▶ gateway ─┤
                          └─ /query ──▶  claude CLI (subprocess)
```

## Experimental Codex 모드

현재는 Claude 경로와 별도로 `Codex` headless 경로를 실험적으로 제공한다.

- `/codex/query`
- `/codex/query/stream`
- `/codex/sessions`
- `/codex/sessions/:id/send`
- `/codex/sessions/:id/stream`
- `/codex/sessions/:id/messages`

이 경로는 `codex exec --json` / `codex exec resume --json`을 래핑한다.
즉 Claude처럼 장시간 붙어 있는 interactive subprocess가 아니라,
turn 단위 non-interactive 실행을 세션처럼 묶는 방식이다.

현재 범위:
- 단발 query
- 멀티턴 session resume
- command execution / agent message / token usage 스트리밍

현재 제한:
- Codex approval callback bridge는 아직 없다.
- 무인모드 기준으로 `approval_policy=never` 사용을 권장한다.
- `approval_policy=on-request`, `untrusted`, `on-failure` 같은 interactive approval
  정책은 현재 `exec` transport에서 명시적으로 거부된다.
- Codex approval bridge를 붙이려면 `exec --json`이 아니라 `app-server` 또는
  `exec-server` backend가 필요하다.
- Claude의 `hook_request` / `permission_request`와 1:1 parity를 목표로 하지 않는다.

## 사전 요구사항

| 항목 | 설명 |
|------|------|
| **Claude Code** | 설치 + 로그인 완료 (`claude` 명령 실행 가능) |
| **Claude Code 구독** | Max / Pro / Team / Enterprise |
| **Node.js** | v18+ (CLI 모드 전용) |

```bash
claude --version   # CLI 설치 확인
```

## 빌드 & 실행

```bash
cargo build --release
./target/release/claude-agent-rs                # 기본 127.0.0.1:8765
./target/release/claude-agent-rs --port 9000    # 포트 변경
./target/release/claude-agent-rs --check-cli    # CLI 확인만
```

서버 시작 시 자동으로:
1. Keychain에서 OAuth 토큰 로드
2. Quota 사전 확인 (rate limit 상태 캐시)

```
INFO OAuth token loaded (subscription: max, tier: default_claude_max_20x)
INFO Quota pre-check complete: status=allowed, 5h=22%, 7d=2%
INFO Starting claude-agent-rs on 127.0.0.1:8765
```

### 설정

`config.toml` (선택):

```toml
[server]
host = "127.0.0.1"
port = 8765
max_sessions = 100
cors_origins = ["http://localhost", "http://127.0.0.1"]  # 빈 배열 = 모두 허용

[cli]
bin_path = ""                    # 빈 문자열 = PATH 자동 탐색
session_idle_timeout_secs = 1800 # 30분 (CLI wrap 세션)

[proxy]
enabled = true                   # /v1/* 라우트 활성화
max_concurrent = 1               # 동시 API 호출 수 (보수적 기본)
max_proxy_sessions = 50          # 프록시 세션 상한
session_idle_timeout_secs = 1800 # 30분 (프록시 세션)
```

환경변수 오버라이드: `CLAUDE_GATEWAY__<SECTION>__<FIELD>` 형식.
예: `CLAUDE_GATEWAY__SERVER__PORT=9000`, `CLAUDE_GATEWAY__PROXY__MAX_CONCURRENT=4`.

기타 인식되는 환경변수:

| 변수 | 용도 |
|------|------|
| `RUST_LOG` | 로그 레벨 (`info`, `debug`, `claude_agent=debug`, …) |
| `CLAUDE_CONFIG_DIR` | Claude Code 설정 디렉터리 (토큰/credentials 위치) |
| `CLAUDE_CODE_CUSTOM_OAUTH_URL` | 사내망 OAuth 엔드포인트 override (allowlist됨) |
| `USE_LOCAL_OAUTH`, `USE_STAGING_OAUTH`, `USER_TYPE` | Anthropic 내부 테스트용 OAuth 런타임 선택 |
| `CLAUDE_LOCAL_OAUTH_API_BASE` | 로컬 OAuth 서버 베이스 URL |
| `CLAUDE_GATEWAY_SKIP_KEYCHAIN=1` | **테스트 전용**. macOS Keychain 접근 차단. |

---

## Proxy 모드 API (`/v1/*`)

OAuth 토큰으로 Anthropic Messages API를 직접 호출합니다.
CLI subprocess 오버헤드 없이 빠르고, Haiku/Sonnet/Opus 자유 선택 가능.

### 모델 별칭

| 별칭 | 정규 ID |
|------|---------|
| `haiku`, `haiku4.5`, `claude-haiku` | `claude-haiku-4-5-20251001` |
| `sonnet4` | `claude-sonnet-4-20250514` |
| `sonnet4.5` | `claude-sonnet-4-5-20250929` |
| `sonnet`, `claude-sonnet` | `claude-sonnet-4-6` |
| `opus4` | `claude-opus-4-20250514` |
| `opus4.5` | `claude-opus-4-5-20251101` |
| `opus`, `claude-opus` | `claude-opus-4-6` |

정규 ID를 직접 써도 되고, 접두사 매치(`claude-sonnet-4-6-...`)도 통합니다.
`POST /v1/messages`는 `model`이 필수입니다. 기본 모델 보정은 `/v1/sessions`
세션 경로에서만 적용됩니다.

### 단일 요청 (Stateless)

```bash
# 동기 응답
curl -X POST http://localhost:8765/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "haiku",
    "max_tokens": 100,
    "system": "계산기. 숫자만 답해.",
    "messages": [{"role": "user", "content": "2+2"}]
  }'

# SSE 스트리밍
curl -N -X POST http://localhost:8765/v1/messages/stream \
  -H "Content-Type: application/json" \
  -d '{
    "model": "sonnet",
    "max_tokens": 500,
    "messages": [{"role": "user", "content": "Rust의 장점"}]
  }'
```

`max_tokens` 생략 시 기본값 8,000 적용.

### 멀티턴 세션

서버가 messages 배열을 관리합니다. 클라이언트는 매 턴 새 메시지만 보냅니다.

```bash
# 1. 세션 생성
curl -X POST http://localhost:8765/v1/sessions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "haiku",
    "system": "너는 수학 튜터. 간결하게 답해."
  }'
# → {"id": "abc-123", "model": "claude-haiku-4-5-20251001", ...}

# 2. 메시지 전송 (이전 대화 자동 포함)
curl -X POST http://localhost:8765/v1/sessions/abc-123/msg \
  -H "Content-Type: application/json" \
  -d '{"content": "피타고라스 정리가 뭐야?"}'

# 3. 후속 질문 (대화 맥락 유지)
curl -X POST http://localhost:8765/v1/sessions/abc-123/msg \
  -H "Content-Type: application/json" \
  -d '{"content": "그걸로 빗변 5, 한 변 3인 삼각형 풀어봐"}'

# 4. 세션 상태 조회
curl http://localhost:8765/v1/sessions/abc-123

# 5. 세션 목록
curl http://localhost:8765/v1/sessions

# 6. 세션 삭제
curl -X DELETE http://localhost:8765/v1/sessions/abc-123

# 7. 세션 내 메시지 스트리밍 (SSE)
curl -N -X POST http://localhost:8765/v1/sessions/abc-123/msg/stream \
  -H "Content-Type: application/json" \
  -d '{"content": "스트리밍으로 답해봐"}'
```

#### 세션 생성 옵션

```json
{
  "model": "haiku",
  "system": "시스템 프롬프트",
  "max_tokens": 1000,
  "temperature": 0.7,
  "tools": [{"name": "...", "description": "...", "input_schema": {...}}],
  "tool_choice": {"type": "auto"},
  "betas": ["extra-beta-header"]
}
```

#### tool_use 라운드트립

```bash
# 1. tool 정의된 세션 생성
curl -X POST http://localhost:8765/v1/sessions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "haiku",
    "system": "Use get_weather when asked about weather.",
    "tools": [{
      "name": "get_weather",
      "description": "Get weather for a city",
      "input_schema": {
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"]
      }
    }]
  }'

# 2. 질문 → stop_reason: "tool_use" 응답
curl -X POST http://localhost:8765/v1/sessions/{id}/msg \
  -d '{"content": "서울 날씨 어때?"}'
# → content: [{type: "tool_use", id: "toolu_xxx", name: "get_weather", input: {city: "서울"}}]

# 3. tool 결과 반환
curl -X POST http://localhost:8765/v1/sessions/{id}/msg \
  -H "Content-Type: application/json" \
  -d '{
    "is_tool_result": true,
    "content": [{
      "type": "tool_result",
      "tool_use_id": "toolu_xxx",
      "content": "서울: 맑음, 18도"
    }]
  }'
# → assistant가 tool 결과를 기반으로 자연어 응답
```

### 모니터링

```bash
# OAuth 토큰 상태
curl http://localhost:8765/v1/auth_status
# → {"authenticated": true, "subscription_type": "max", "token_valid": true, ...}

# Rate limit 사용률
curl http://localhost:8765/v1/rate_limit
# → {"status": "allowed", "utilization_5h": 0.22, "utilization_7d": 0.02, ...}

# 프록시 누적 통계
curl http://localhost:8765/v1/proxy_stats
# → {"total_requests": 15, "total_input_tokens": 3200, "total_output_tokens": 1500, ...}
```

## Experimental OpenAI API 모드 (`/openai/v1/*`)

`OPENAI_API_KEY`가 설정되어 있으면 OpenAI Responses API에 가까운 얇은 프록시를
제공한다.

- `POST /openai/v1/responses`
- `POST /openai/v1/responses/stream`
- `GET /openai/v1/models`
- `GET /openai/v1/proxy_stats`

현재 범위:
- stateless Responses 호출
- streaming passthrough
- models 조회

현재 제한:
- OpenAI 세션/대화 상태는 서버가 따로 관리하지 않는다.
- OpenAI tool loop는 caller가 처리해야 한다.
- 현재는 Responses API 중심이며 Chat Completions 호환 표면은 없다.

예시:

```bash
curl -X POST http://localhost:8765/openai/v1/responses \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-5.4-mini",
    "input": "Reply with exactly: hi"
  }'
```

---

## Codex 모드 API (`/codex/*`)

Codex CLI를 headless non-interactive 채널로 사용한다.

### 단일 요청

```bash
curl -X POST http://localhost:8765/codex/query \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Reply with exactly: hi",
    "options": {
      "sandbox": "read-only",
      "approval_policy": "never"
    }
  }'
```

응답에는 `thread_id`, 마지막 `output_text`, `usage`, 그리고 JSONL에서 변환한 `events`
배열이 포함된다.

### 스트리밍

```bash
curl -N -X POST http://localhost:8765/codex/query/stream \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Run pwd and print the directory path only.",
    "options": {
      "sandbox": "read-only",
      "approval_policy": "never"
    }
  }'
```

### 세션

```bash
# 1. 세션 생성
curl -X POST http://localhost:8765/codex/sessions \
  -H "Content-Type: application/json" \
  -d '{
    "options": {
      "sandbox": "read-only",
      "approval_policy": "never"
    }
  }'

# 2. 첫 턴
curl -X POST http://localhost:8765/codex/sessions/{id}/send \
  -H "Content-Type: application/json" \
  -d '{"message": "Reply with exactly: first"}'

# 3. 메시지/이벤트 조회
curl http://localhost:8765/codex/sessions/{id}/messages

# 4. 다음 턴
curl -X POST http://localhost:8765/codex/sessions/{id}/send \
  -H "Content-Type: application/json" \
  -d '{"message": "Reply with exactly: second"}'
```

세션은 내부적으로 Codex `thread_id`를 저장하고 다음 턴에서 `exec resume`를 사용한다.

### Codex 옵션

지원되는 주요 옵션:

- `system_prompt`
- `model`
- `cwd`
- `env`
- `cli_path`
- `add_dirs`
- `profile`
- `sandbox`
- `approval_policy`
- `full_auto`
- `dangerously_bypass_approvals_and_sandbox`
- `search`
- `ephemeral`
- `ignore_user_config`
- `ignore_rules`
- `skip_git_repo_check`

권장 기본값:

- `sandbox`: `read-only` 또는 `workspace-write`
- `approval_policy`: `never`

### 트래픽 관리 규칙

| 상황 | 동작 |
|------|------|
| 서버 시작 | Quota 사전 확인 (rate limit 상태 캐시) |
| 매 요청 전 | 캐시된 상태 확인 → `rejected`면 API 호출 없이 거부 + 리셋 시간 안내 |
| 리셋 시간 경과 | 자동 해제, 다음 요청 허용 |
| 429 (rate limit) | 재시도 없이 즉시 반환 + 상태 캐시 |
| 529 (overloaded) | 최대 3회 지수 백오프 재시도 (500ms base) |
| 401 | 토큰 캐시 무효화 → `claude /login` 안내 |
| 컨텍스트 200K 근접 | 세션 메시지 추가 거부 + 새 세션 안내 |
| 세션 30분 미사용 | 자동 정리 |

---

## CLI Wrap 모드 (`/query`, `/sessions`)

Claude Code CLI를 subprocess로 실행합니다. 파일 읽기/쓰기, bash 실행 등 CLI 도구가 필요할 때 사용.

### 단일 쿼리

```bash
# 동기
curl -X POST http://localhost:8765/query \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "options": {"max_turns": 1, "permission_mode": "plan"}}'

# SSE 스트리밍
curl -N -X POST http://localhost:8765/query/stream \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "options": {"max_turns": 1, "permission_mode": "plan"}}'
```

### 세션 (Multi-turn)

```bash
# 생성
curl -X POST http://localhost:8765/sessions \
  -d '{"options": {"permission_mode": "plan"}}'

# 메시지 전송
curl -X POST http://localhost:8765/sessions/{id}/send \
  -d '{"message": "내 이름은 Alex야"}'

# SSE 구독 (히스토리 포함)
curl -N http://localhost:8765/sessions/{id}/stream

# 히스토리 / 목록 / 삭제 / 분기 / 중단
GET    /sessions/{id}/messages?limit=50
GET    /sessions
DELETE /sessions/{id}
POST   /sessions/{id}/fork
POST   /sessions/{id}/interrupt
```

### Hook 시스템

세션 옵션에 `hook_rules`를 넣으면 gateway가 CLI spawn 직후 `initialize`
제어 요청으로 각 규칙을 콜백 ID로 등록합니다. 이후 CLI가 `PreToolUse` 등의
`hook_callback`을 제어 프로토콜로 올려보내면 서버 규칙이 우선 평가되어
`approve` / `block` / `defer`를 결정합니다. 우선순위: **block > approve > defer**.

`defer`로 매칭되거나 매칭되는 규칙이 없으면 해당 이벤트가 `type: hook_request`
SSE 메시지로 클라이언트에 전달되고, 지정 시간 안에 아래 엔드포인트로 응답해야
합니다. 기본값은 `30초 후 auto-block`이며, 요청별 `hook_timeout_secs` /
`hook_timeout_action`으로 override할 수 있습니다.

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

```bash
# 클라이언트 defer 응답 (요청은 SSE로 받은 request_id 사용)
curl -X POST http://localhost:8765/sessions/{id}/hook_response \
  -H "Content-Type: application/json" \
  -d '{"request_id": "<from stream>", "decision": "block", "reason": "manual deny"}'
```

도구 permission prompt도 세션 스트림으로 surface되며, 별도 응답 엔드포인트를
사용합니다.

```bash
curl -X POST http://localhost:8765/sessions/{id}/permission_response \
  -H "Content-Type: application/json" \
  -d '{"request_id": "<from stream>", "behavior": "allow"}'
```

`POST /query`, `POST /query/stream`는 stateless 경로라 deferred hook callback이나
tool permission prompt를 대화형으로 처리할 수 없습니다. 이 경로에서는 해당 요청이
자동 block/deny 처리됩니다. interactive approval이 필요하면 `/sessions`를 사용해야
합니다.

### 관리 API

```bash
GET /health     # 서버 상태
GET /stats      # 누적 통계
GET /config     # 현재 설정
```

---

## 아키텍처

```
src/
├── bin/server.rs          # 엔트리포인트 (axum 서버)
├── api/
│   ├── proxy.rs           # /v1/messages (Direct API)
│   ├── proxy_sessions.rs  # /v1/sessions (Multi-turn)
│   ├── sessions.rs        # /sessions (CLI wrap)
│   ├── query.rs           # /query (CLI wrap)
│   ├── hooks.rs           # /sessions/:id/hook_response
│   └── admin.rs           # /health, /stats, /config
├── auth.rs                # OAuth 토큰 (Keychain 읽기 전용)
├── models.rs              # 모델 카탈로그, 상수, 별칭
├── proxy.rs               # Messages API 프록시 + rate limit + retry
├── proxy_session.rs       # 프록시 세션 store
├── session/               # CLI wrap 세션
├── transport/             # CLI subprocess 통신
├── messages/              # CLI NDJSON 메시지 타입
├── hooks/                 # Hook 규칙 + 타임아웃
├── mcp/                   # MCP 서버 설정
├── config.rs              # TOML + 환경변수 설정
├── options.rs             # ClaudeAgentOptions
└── error.rs               # 에러 타입
```

## 라이선스

MIT
