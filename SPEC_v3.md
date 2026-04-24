# claude-agent-rs — 확정 요구사항 명세 v3

> 작성: 2026-04-03 | 베이스: SPEC.md v2 + DESIGN.md  
> 상태: **Phase 1~5 구현 완료 (2026-04 기준)**  
> 목표: Claude Code CLI가 RALP 루프로 자율 구현 완료

---

## 📌 구현 후 업데이트 노트 (2026-04-25)

본 SPEC의 **Hook 프로토콜** 섹션은 구현 과정에서 CLI 실제 동작에 맞춰
재설계되었습니다. 현행 정상 스펙은 **README.md / docs/USAGE.md**를 참조하세요.

변경 요약:
- 최상위 메시지 `{"type":"hook_request","hook_id":...}` →
  SDK 제어 프로토콜 `{"type":"control_request","request_id":...,"request":{"subtype":"hook_callback",...}}`
- 응답 `{"type":"hook_response",...}` →
  `{"type":"control_response","response":{"subtype":"success","request_id":...,"response":{...}}}`
- CLI spawn 직후 `{"subtype":"initialize","hooks":{...}}` 제어 요청으로 콜백 등록
- 클라이언트 응답 엔드포인트 `POST /sessions/:id/hook_response` 는 `hook_id` 대신
  `request_id` 필드를 사용

SPEC 본문의 `hook_id`, `HookResponse`, `CliHookRequestEvent` 등 구 용어는
**역사적 참조**이며 현 코드와 일치하지 않습니다.

---

## ⚠️ SPEC.md v2에서 수정된 핵심 오류

> Claude Code가 읽기 전에 반드시 확인. 아래 사항이 이전 문서와 다름.

| # | 이전 SPEC.md v2 (잘못됨) | 이 문서 (정확함) | 근거 |
|---|--------------------------|-----------------|------|
| 1 | `transport.write(prompt + options JSON)` | options는 subprocess spawn 시 CLI 플래그로 전달. stdin에는 `system:init` 이후 user 메시지만 씀 | agent-sdk 소스 분석 |
| 2 | `RateLimitEvent` 메시지 타입 존재 | 존재하지 않음. 제거 | stream-json 실제 출력 확인 |
| 3 | subprocess crash → "Transport 재연결" | 재연결 불가. 새 subprocess 생성 = 새 session_id. 기존 세션 복구 불가 | 프로세스 격리 원칙 |
| 4 | `set_model()`, `set_permission_mode()` 런타임 변경 | stdin 명령으로 불가. 새 session 생성으로 대체 | CLI 플래그는 spawn 시에만 |
| 5 | Hook 콜백 = Rust 인터페이스 | REST API 컨텍스트: SSE 이벤트 emit → 클라이언트 POST 응답 round-trip | 서버-클라이언트 분리 |
| 6 | Hook 타임아웃 60초 | 30초 (일관성 + agent-sdk 관행) | - |
| 7 | "CLI 프로토콜 선행 조사 필요" | 조사 완료. 이 문서에 프로토콜 전체 명세 포함 | - |
| 8 | SDK MCP "in-process" 정의 없음 | Rust에서는 "서버 내장 툴": HTTP 핸들러로 등록, subprocess stdin을 통해 tool_result 반환 | 재정의 |

---

## 0. 프로젝트 개요

### 비전

Anthropic 공식 `claude-agent-sdk`(TypeScript/Python)의 모든 기능을 Rust로 포팅하여 **단일 바이너리 REST API 서버**로 서빙.

### 핵심 제약 (변경 불가)

1. **Claude Code CLI 경유 필수**: `claude` CLI를 subprocess로 spawn해서 stdin/stdout JSON으로 통신
2. **PoC 모드**: 로컬 claude CLI 세션 사용 → Claude Code 월정액 적용, API 키 불필요
3. **단일 바이너리**: Node.js 런타임은 CLI 실행에만 필요, 서버 자체는 Rust 바이너리
4. **기능 동등성**: Python/TS SDK와 동일 메시지 스키마 보장

---

## 1. subprocess 통신 프로토콜 (확정, 선행 조사 완료)

> **이 섹션이 구현의 토대. 코드 작성 전 완전 숙지 필수.**

### 1.1 subprocess spawn 명령

```bash
claude \
  --output-format stream-json \   # stdout: JSON lines
  --input-format stream-json \    # stdin: JSON lines
  [--verbose] \                   # debug 라인 stdout에 추가 (기본 비활성)
  [--model claude-sonnet-4-6] \
  [--permission-mode default|acceptEdits|bypassPermissions|plan|dontAsk] \
  [--system-prompt "..."] \
  [--setting-sources project,user,local] \
  [--resume <session_id>] \       # 세션 재개
  [--continue] \                  # 가장 최근 세션 계속
  [--max-turns <n>] \
  [--no-verbose] \                # debug 라인 비활성화 (권장)
  [--mcp-config <path>] \         # MCP 서버 설정 JSON 파일
  [--allowedTools Read,Write,Bash] \
  [--disallowedTools Edit]
  # --include-partial-messages 는 없음. --output-format stream-json이면 partial 자동 포함
```

**stdin/stdout 모두 `\n` 구분 JSON 라인 (NDJSON)**

### 1.2 stdout 이벤트 (CLI → Rust 서버)

```
# 순서: system:init → (assistant* → user*)* → result
# include_partial_messages 활성 시 stream_event가 assistant 앞에 섞임
```

**주의**: `--verbose` 없어도 debug 텍스트 라인이 나올 수 있음.  
파싱 규칙: **`{`로 시작하지 않는 라인은 `tracing::debug!`로 버리고 continue.**

#### system (subtype: init)
```json
{
  "type": "system",
  "subtype": "init",
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "tools": [{"name": "Read", "description": "..."}],
  "mcp_servers": [],
  "slash_commands": [],
  "model": "claude-sonnet-4-6"
}
```

#### system (subtype: compact_boundary)
```json
{
  "type": "system",
  "subtype": "compact_boundary",
  "session_id": "..."
}
```

#### assistant
```json
{
  "type": "assistant",
  "session_id": "...",
  "parent_tool_use_id": null,
  "message": {
    "id": "msg_01XFDUDYJgAACTU67reL2K",
    "role": "assistant",
    "content": [
      {"type": "text", "text": "안녕하세요!"},
      {"type": "tool_use", "id": "toolu_01", "name": "Read", "input": {"file_path": "/tmp/a.txt"}}
    ],
    "model": "claude-sonnet-4-6",
    "stop_reason": "tool_use",
    "usage": {
      "input_tokens": 1024,
      "output_tokens": 256,
      "cache_read_input_tokens": 512,
      "cache_creation_input_tokens": 0
    }
  }
}
```

#### user (tool_result 포함)
```json
{
  "type": "user",
  "session_id": "...",
  "parent_tool_use_id": null,
  "message": {
    "role": "user",
    "content": [
      {
        "type": "tool_result",
        "tool_use_id": "toolu_01",
        "content": [{"type": "text", "text": "파일 내용"}],
        "is_error": false
      }
    ]
  }
}
```

#### result
```json
{
  "type": "result",
  "subtype": "success",
  "session_id": "...",
  "result": "최종 텍스트 결과",
  "cost_usd": 0.0042,
  "total_cost_usd": 0.0042,
  "usage": {
    "input_tokens": 2048,
    "output_tokens": 512,
    "cache_read_input_tokens": 1024,
    "cache_creation_input_tokens": 0
  },
  "num_turns": 3,
  "duration_ms": 4200,
  "duration_api_ms": 3100
}
```

result.subtype 가능 값:
- `"success"` — 정상 완료
- `"error_during_generation"` — API 오류
- `"max_turns_reached"` — max_turns 초과
- `"max_budget_usd_exceeded"` — 예산 초과
- `"error_max_structured_output_retries"` — structured output 재시도 초과

#### stream_event (--include-partial-messages 플래그 사용 시)
```json
{
  "type": "stream_event",
  "session_id": "...",
  "parent_tool_use_id": null,
  "uuid": "uuid-v4",
  "stream_event": {
    "type": "content_block_delta",
    "index": 0,
    "delta": {"type": "text_delta", "text": "안녕"}
  }
}
```

stream_event.stream_event.type 가능 값:
`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`

**⚠️ 주의**: `--output-format stream-json`으로는 partial messages가 기본 포함되지 않음.  
streaming 활성화가 필요하면 추가 플래그 `--include-partial-messages` (또는 해당 env var) 필요.  
실제 플래그명은 agent-sdk 소스 또는 `claude --help`로 확인.

### 1.3 stdin 입력 (Rust 서버 → CLI)

**절대 규칙**: `system:init` 이벤트 수신 후에만 stdin에 쓸 것.  
초기화 전 쓰기 = CLI가 무시하거나 오동작.

#### 사용자 메시지
```json
{"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "hello"}]}}
```

이미지 포함:
```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": [
      {"type": "text", "text": "이 이미지를 분석해줘"},
      {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}}
    ]
  }
}
```

#### 중단
```json
{"type": "interrupt"}
```

### 1.4 Hook 이벤트 round-trip

> **이 부분이 SPEC.md v2에서 완전히 누락된 핵심 메커니즘.**

CLI가 hook 이벤트 발생 시:

1. CLI가 stdout에 hook_request 이벤트 출력 후 **stdin 블로킹 대기**
2. 서버가 SSE 이벤트로 클라이언트에 전달
3. 클라이언트가 `POST /sessions/{id}/hook_response` 호출
4. 서버가 stdin에 응답 JSON 쓰기
5. CLI 계속 실행

**stdout (CLI → 서버):**
```json
{
  "type": "hook_request",
  "hook_id": "hook-uuid-v4",
  "hook_event_name": "PreToolUse",
  "session_id": "...",
  "tool_name": "Edit",
  "tool_input": {"file_path": "/src/main.rs", "old_string": "...", "new_string": "..."}
}
```

hook_event_name 가능 값:
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `UserPromptSubmit`,  
`Stop`, `SubagentStart`, `SubagentStop`, `PreCompact`, `Notification`, `PermissionRequest`

**stdin (서버 → CLI) - hook 응답:**
```json
{
  "type": "hook_response",
  "hook_id": "hook-uuid-v4",
  "decision": "approve",
  "reason": null,
  "updated_input": null
}
```

decision 가능 값:
- `"approve"` — 도구 실행 허용
- `"block"` — 도구 실행 차단 (reason 포함 권장)
- `"defer"` — CLI가 기본 동작 수행

PreToolUse 추가 필드 (optional):
- `"updated_input"`: 도구 입력값 수정 (object)
- `"suppress_output"`: true 시 tool_result를 컨텍스트에서 숨김

**서버 타임아웃**: 30초 내 hook_response 없으면 자동 `approve` 응답 후 계속.  
타임아웃 발생 시 세션 히스토리에 `{"type":"hook_timeout","hook_id":"..."}` 기록.

### 1.5 세션 생명주기

```
spawn → system:init → [write user msg] → assistant* → result → 세션 유지 (다음 메시지 대기)
                                                                    ↑
                                                        세션 모드에서는 여기서 대기
                                                        다음 write(user msg)까지

close stdin → subprocess graceful exit (3초 대기 → SIGKILL)
```

**single-turn query**: result 받은 후 stdin close → subprocess 종료  
**multi-turn session**: result 받아도 stdin 열어두고 다음 사용자 메시지 대기

---

## 2. 전체 아키텍처

```
claude-agent-rs/
├── Cargo.toml
├── config.toml.example
├── src/
│   ├── lib.rs                   # pub mod 선언
│   ├── bin/
│   │   └── server.rs            # axum 서버 엔트리포인트
│   ├── transport/
│   │   ├── mod.rs               # Transport trait
│   │   └── cli.rs               # CliTransport (subprocess 관리)
│   ├── session/
│   │   ├── mod.rs               # Session, SessionState
│   │   └── store.rs             # SessionStore (DashMap)
│   ├── messages/
│   │   ├── mod.rs               # Message enum
│   │   ├── cli_output.rs        # CliOutputEvent (stdout 파싱용 내부 타입)
│   │   ├── cli_input.rs         # CliInputMessage (stdin 쓰기용 내부 타입)
│   │   └── content.rs           # ContentBlock types
│   ├── query.rs                 # query() 함수 (single-use)
│   ├── client.rs                # ClaudeSDKClient (session-based)
│   ├── options.rs               # ClaudeAgentOptions
│   ├── hooks/
│   │   ├── mod.rs               # Hook 이벤트 처리
│   │   └── server_rules.rs      # 서버 사이드 훅 규칙 (HTTP round-trip 없는 버전)
│   ├── permissions/
│   │   └── mod.rs               # PermissionMode + 평가 로직
│   ├── mcp/
│   │   ├── mod.rs               # McpManager
│   │   ├── config.rs            # McpServerConfig (stdio/sse/http)
│   │   └── builtin.rs           # Built-in MCP 툴 (Rust 내장 툴 핸들러)
│   ├── error.rs                 # GatewayError thiserror
│   ├── config.rs                # AppConfig, ServerConfig
│   └── api/
│       ├── mod.rs               # Router 조립
│       ├── query.rs             # POST /query
│       ├── sessions.rs          # /sessions/* CRUD + SSE stream
│       ├── hooks.rs             # POST /sessions/{id}/hook_response
│       └── admin.rs             # GET /health, GET/PUT /config
├── tests/
│   ├── fixtures/                # 실제 CLI 출력 fixture JSON 파일들
│   │   ├── system_init.json
│   │   ├── assistant_text.json
│   │   ├── assistant_tool_use.json
│   │   ├── result_success.json
│   │   └── result_error.json
│   ├── transport_test.rs        # Transport 단위 테스트 (MockTransport)
│   ├── messages_parse_test.rs   # fixture 기반 파싱 테스트
│   ├── session_store_test.rs
│   └── integration/
│       └── query_test.rs        # 실제 claude CLI 호출 (CI skip 조건 있음)
```

### 의존성 (Cargo.toml)

```toml
[package]
name = "claude-agent-rs"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "claude-agent-rs"
path = "src/bin/server.rs"

[lib]
name = "claude_agent"
path = "src/lib.rs"

[dependencies]
# Web
axum = { version = "0.7", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.5", features = ["cors", "trace"] }
tokio-stream = "0.1"
async-stream = "0.3"

# HTTP client (미래 API Key 모드용, MCP HTTP 클라이언트용)
reqwest = { version = "0.12", features = ["json", "stream"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Concurrency
dashmap = "5"
futures = "0.3"

# Subprocess I/O (line-by-line async)
tokio-util = { version = "0.7", features = ["codec"] }

# Config
config = "0.14"
dotenvy = "0.15"

# IDs
uuid = { version = "1", features = ["v4"] }

# CLI args
clap = { version = "4", features = ["derive"] }

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# Error
anyhow = "1"
thiserror = "1"

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
```

---

## 3. 타입 시스템 (완전 정의)

> **아래 타입이 구현의 출발점. 절대 partial 구현 금지.**

### 3.1 CLI stdout 파싱 타입 (messages/cli_output.rs)

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// CLI stdout에서 읽는 모든 이벤트 타입
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliOutputEvent {
    System(CliSystemEvent),
    Assistant(CliAssistantEvent),
    User(CliUserEvent),
    Result(CliResultEvent),
    StreamEvent(CliStreamEventWrapper),
    HookRequest(CliHookRequestEvent),
}

#[derive(Debug, Deserialize)]
pub struct CliSystemEvent {
    pub subtype: SystemSubtype,
    pub session_id: String,
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubtype {
    Init,
    CompactBoundary,
}

#[derive(Debug, Deserialize)]
pub struct CliAssistantEvent {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub message: CliAssistantMessage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CliAssistantMessage {
    pub id: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,        // String 또는 Vec<ContentBlock>
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CliUserEvent {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub message: Value,
}

#[derive(Debug, Deserialize)]
pub struct CliResultEvent {
    pub subtype: ResultSubtype,
    pub session_id: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<SessionUsage>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    pub duration_api_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    Success,
    ErrorDuringGeneration,
    MaxTurnsReached,
    MaxBudgetUsdExceeded,
    ErrorMaxStructuredOutputRetries,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CliStreamEventWrapper {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub uuid: Option<String>,
    pub stream_event: Value,
}

#[derive(Debug, Deserialize)]
pub struct CliHookRequestEvent {
    pub hook_id: String,
    pub session_id: String,
    pub hook_event_name: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
}
```

### 3.2 CLI stdin 쓰기 타입 (messages/cli_input.rs)

```rust
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliInputMessage {
    User {
        message: CliUserInput,
    },
    HookResponse {
        hook_id: String,
        decision: HookDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        suppress_output: Option<bool>,
    },
    Interrupt,
}

#[derive(Debug, Serialize)]
pub struct CliUserInput {
    pub role: String,    // 항상 "user"
    pub content: Vec<InputContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Debug, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,    // "base64"
    pub media_type: String,     // "image/png", "image/jpeg", 등
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Approve,
    Block,
    Defer,
}
```

### 3.3 공개 Message 타입 (messages/mod.rs)

> HTTP API / 라이브러리 사용자에게 노출되는 타입. CLI 내부 타입과 분리.

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// SSE 이벤트 및 query() 스트림 아이템
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    System {
        session_id: String,
        subtype: String,
        tools: Vec<Value>,
        model: Option<String>,
    },
    Assistant {
        session_id: String,
        parent_tool_use_id: Option<String>,
        message: AssistantMessage,
    },
    User {
        session_id: String,
        parent_tool_use_id: Option<String>,
        message: Value,
    },
    Result {
        session_id: String,
        subtype: String,
        result: Option<String>,
        error: Option<String>,
        cost_usd: Option<f64>,
        total_cost_usd: Option<f64>,
        usage: Option<SessionUsage>,
        num_turns: Option<u32>,
        duration_ms: Option<u64>,
    },
    StreamEvent {
        session_id: String,
        uuid: Option<String>,
        stream_event: Value,
    },
    HookRequest {
        hook_id: String,
        session_id: String,
        hook_event_name: String,
        tool_name: Option<String>,
        tool_input: Option<Value>,
    },
    Error {
        message: String,
        code: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}
// ContentBlock, TokenUsage, SessionUsage는 cli_output.rs와 공유 (re-export)
```

### 3.4 에러 타입 (error.rs)

```rust
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("CLI not found at {path}: {detail}")]
    CliNotFound { path: String, detail: String },

    #[error("CLI connection failed: {0}")]
    CliConnection(String),

    #[error("CLI process exited with code {exit_code}: {stderr}")]
    ProcessExit { exit_code: i32, stderr: String },

    #[error("CLI process crashed (no exit code): {detail}")]
    ProcessCrash { detail: String },

    #[error("JSON decode error for line `{line}`: {source}")]
    JsonDecode { line: String, source: serde_json::Error },

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session in wrong state: expected {expected}, got {actual}")]
    InvalidSessionState { expected: String, actual: String },

    #[error("Hook timeout (hook_id={hook_id}): auto-approved after 30s")]
    HookTimeout { hook_id: String },

    #[error("Concurrent session limit reached (max={max})")]
    SessionLimitReached { max: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::CliNotFound { .. } => 503,
            Self::CliConnection(_) | Self::ProcessExit { .. } | Self::ProcessCrash { .. } => 502,
            Self::JsonDecode { .. } => 502,
            Self::SessionNotFound(_) => 404,
            Self::InvalidSessionState { .. } => 409,
            Self::SessionLimitReached { .. } => 429,
            _ => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::CliNotFound { .. } => "cli_not_found",
            Self::CliConnection(_) => "cli_connection",
            Self::ProcessExit { .. } | Self::ProcessCrash { .. } => "process_error",
            Self::JsonDecode { .. } => "json_decode",
            Self::SessionNotFound(_) => "session_not_found",
            Self::InvalidSessionState { .. } => "invalid_state",
            Self::SessionLimitReached { .. } => "rate_limited",
            _ => "internal_error",
        }
    }
}
```

### 3.5 세션 상태 (session/mod.rs)

```rust
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use dashmap::DashMap;
use crate::messages::Message;

/// 세션의 현재 상태
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    /// subprocess 시작 중, system:init 대기
    Initializing,
    /// 유휴 (user 메시지 대기 중)
    Idle,
    /// Claude 응답 생성 중
    Running,
    /// hook_response 대기 중 (CLI가 stdin 블로킹)
    WaitingForHook { hook_id: String, deadline: std::time::Instant },
    /// result 수신 완료 (single-turn) 또는 다음 메시지 준비 완료
    Completed,
    /// subprocess 종료됨
    Dead,
}

pub struct Session {
    pub id: String,                          // 서버가 부여한 UUID
    pub cli_session_id: Option<String>,      // CLI system:init에서 받은 session_id
    pub state: Arc<Mutex<SessionState>>,
    pub created_at: std::time::Instant,
    pub options: ClaudeAgentOptions,

    /// stdin 쓰기 채널 (subprocess에 보내는 메시지)
    pub stdin_tx: mpsc::Sender<String>,

    /// stdout 이벤트 브로드캐스트 (여러 SSE 구독자 동시 지원)
    /// capacity 256: 느린 클라이언트가 있어도 다른 구독자 차단 안됨
    pub event_tx: broadcast::Sender<Message>,

    /// 세션 히스토리 (SSE 재연결 시 missed 이벤트 재전송용)
    pub history: Arc<Mutex<Vec<Message>>>,
}

pub struct SessionStore {
    sessions: DashMap<String, Arc<Session>>,
    max_sessions: usize,
}

impl SessionStore {
    pub fn new(max_sessions: usize) -> Self { ... }
    pub fn insert(&self, session: Arc<Session>) -> Result<(), GatewayError> { ... }
    pub fn get(&self, id: &str) -> Result<Arc<Session>, GatewayError> { ... }
    pub fn remove(&self, id: &str) -> bool { ... }
    pub fn count(&self) -> usize { self.sessions.len() }
    // background task: idle_timeout 초과 세션 정리
    pub async fn run_cleanup(&self, idle_timeout_secs: u64) { ... }
}
```

### 3.6 ClaudeAgentOptions (options.rs)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeAgentOptions {
    // 기본
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,      // Phase 5
    pub cwd: Option<std::path::PathBuf>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub cli_path: Option<std::path::PathBuf>,

    // 도구
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,

    // 권한
    pub permission_mode: Option<PermissionMode>,

    // MCP
    pub mcp_servers: Option<std::collections::HashMap<String, McpServerConfig>>,

    // 세션
    pub resume: Option<String>,              // --resume <session_id>
    pub continue_conversation: bool,         // --continue (가장 최근 세션)
    pub fork_session: Option<String>,        // 지정 세션 fork (Phase 5)

    // 제어
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,

    // 훅 (서버 사이드 규칙)
    pub hook_rules: Option<Vec<HookRule>>,

    // Subagents
    pub agents: Option<std::collections::HashMap<String, AgentDefinition>>,

    // 출력
    pub include_partial_messages: bool,      // stream_event 포함 여부
    pub output_format: Option<serde_json::Value>, // JSON Schema (Phase 5)

    // 고급
    pub betas: Option<Vec<String>>,
    pub setting_sources: Option<Vec<String>>,
    pub add_dirs: Option<Vec<std::path::PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    DontAsk,
}

impl Default for PermissionMode {
    fn default() -> Self { Self::Default }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        env: Option<std::collections::HashMap<String, String>>,
    },
    Sse {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
    },
    Http {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
    },
    // SDK MCP: Rust에서는 서버에 등록된 내장 툴 핸들러.
    // 외부 MCP 서버처럼 CLI에 노출되지만 실제 실행은 서버 내부에서 처리.
    // Phase 4에서 구현.
    Builtin {
        handler_name: String,    // 서버에 등록된 핸들러 이름
    },
}

/// 서버 사이드 훅 규칙 (HTTP round-trip 없이 평가)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRule {
    pub event: String,           // "PreToolUse", "PostToolUse", 등
    pub tool_pattern: Option<String>,  // "Bash", "Edit|Write", "*" (glob)
    pub action: HookAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Approve,
    Block { reason: Option<String> },
    Defer,               // 클라이언트에 SSE 이벤트로 위임
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub description: Option<String>,
    pub prompt: Option<String>,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
}
```

---

## 4. HTTP REST API (전체 명세)

### 4.1 라우트 맵

```
# 관리
GET  /health
GET  /config
GET  /stats

# 단일 쿼리 (세션 없음)
POST /query            → 동기 응답 (result만)
POST /query/stream     → SSE 스트림

# 세션 관리
POST   /sessions                          → {session_id}
GET    /sessions                          → [{session_id, state, created_at}]
DELETE /sessions/{id}                     → 204

# 세션 상호작용
POST /sessions/{id}/send                  body: {message, ?image_base64}
GET  /sessions/{id}/stream                → SSE 스트림
GET  /sessions/{id}/messages              → ?limit=50&offset=0&include_system=false
POST /sessions/{id}/fork                  → {session_id} (새 session)
POST /sessions/{id}/hook_response         body: 아래 참조
POST /sessions/{id}/interrupt             → 204
```

### 4.2 엔드포인트 상세

#### POST /query (non-streaming)

```
요청:
{
  "prompt": "string (필수)",
  "options": ClaudeAgentOptions (optional)
}

응답 200:
{
  "session_id": "...",
  "result": "최종 텍스트",
  "subtype": "success",
  "cost_usd": 0.0042,
  "usage": {"input_tokens": 1024, "output_tokens": 256},
  "num_turns": 1,
  "duration_ms": 3200
}
```

#### POST /query/stream (SSE)

```
요청: POST /query/stream (동일 body)

응답: Content-Type: text/event-stream

data: {"type":"system","session_id":"...","subtype":"init","tools":[...]}\n\n
data: {"type":"assistant","session_id":"...","message":{...}}\n\n
data: {"type":"result","session_id":"...","subtype":"success","result":"...",...}\n\n
data: [DONE]\n\n
```

#### POST /sessions (세션 생성)

```
요청:
{
  "options": ClaudeAgentOptions (optional)
}

응답 201:
{
  "session_id": "server-assigned-uuid",
  "state": "initializing"
}
```

#### POST /sessions/{id}/send

```
요청:
{
  "message": "string",
  "image_base64": "...",          // optional
  "image_media_type": "image/png" // optional, image_base64 있을 때만
}

응답 202: {} (비동기, 결과는 /stream으로)
```

#### GET /sessions/{id}/stream (SSE)

```
응답: Content-Type: text/event-stream

# SSE 재연결 지원: Last-Event-ID 헤더로 missed 이벤트 재전송
# 각 이벤트에 id 필드 포함 (세션 내 순번)

id: 1
data: {"type":"system","session_id":"...","subtype":"init",...}\n\n

id: 2
data: {"type":"assistant","session_id":"...", "message":{...}}\n\n

id: 3
data: {"type":"hook_request","hook_id":"...","hook_event_name":"PreToolUse",...}\n\n
# → 클라이언트는 30초 내에 POST /sessions/{id}/hook_response 해야 함

id: 4
data: {"type":"result","session_id":"...","subtype":"success",...}\n\n
```

#### POST /sessions/{id}/hook_response

```
요청:
{
  "hook_id": "string (필수)",
  "decision": "approve|block|defer",
  "reason": "string (block 시 권장)",
  "updated_input": {}   // PreToolUse에서 도구 입력 수정 시
}

응답:
- 202: 정상 처리
- 404: session 없음
- 409: 해당 hook_id 대기 중인 훅 없음
- 408: 이미 타임아웃됨
```

#### GET /sessions/{id}/messages

```
쿼리 파라미터:
- limit: 50 (default)
- offset: 0 (default)
- include_system: false (default)

응답 200:
{
  "session_id": "...",
  "total": 42,
  "messages": [Message, ...]
}
```

#### GET /health

```
응답 200:
{
  "status": "ok",
  "version": "0.1.0",
  "cli_available": true,
  "cli_path": "/usr/local/bin/claude",
  "active_sessions": 2,
  "max_sessions": 100
}
```

#### GET /stats

```
응답 200:
{
  "uptime_seconds": 3600,
  "total_queries": 142,
  "active_sessions": 2,
  "total_input_tokens": 45200,
  "total_output_tokens": 12300,
  "total_cost_usd": 0.107
}
```

### 4.3 에러 응답 (공통)

```json
{
  "error": {
    "code": "session_not_found",
    "message": "Session abc123 not found",
    "details": {}
  }
}
```

| code | HTTP | 발생 상황 |
|------|------|----------|
| `invalid_request` | 400 | 필수 필드 누락, 타입 오류 |
| `session_not_found` | 404 | 존재하지 않는 session_id |
| `invalid_state` | 409 | 잘못된 상태에서 명령 (e.g., 이미 Running인 세션에 send) |
| `rate_limited` | 429 | 동시 세션 한도 초과 |
| `cli_not_found` | 503 | claude CLI 미설치 |
| `cli_connection` | 502 | subprocess spawn 실패 |
| `process_error` | 502 | subprocess 비정상 종료 |
| `json_decode` | 502 | CLI stdout 파싱 실패 |
| `hook_timeout` | 408 | hook_response가 30초 내 미도착 |
| `internal_error` | 500 | 예상치 못한 오류 |

---

## 5. 핵심 구현 패턴

### 5.1 Transport trait

```rust
use async_trait::async_trait;
use tokio_stream::Stream;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&mut self) -> Result<(), GatewayError>;
    async fn write(&mut self, data: &str) -> Result<(), GatewayError>;
    fn read_messages(&self) -> Box<dyn Stream<Item = Result<CliOutputEvent, GatewayError>> + Send + Unpin>;
    async fn close(&mut self) -> Result<(), GatewayError>;
    fn is_ready(&self) -> bool;
    fn session_id(&self) -> Option<&str>;  // CLI system:init에서 받은 session_id
}
```

### 5.2 CliTransport 핵심 패턴

```rust
pub struct CliTransport {
    child: Option<tokio::process::Child>,
    stdin_tx: mpsc::Sender<String>,
    event_rx: mpsc::Receiver<Result<CliOutputEvent, GatewayError>>,
    init_received: bool,
    cli_session_id: Option<String>,
}

impl CliTransport {
    fn build_command(options: &ClaudeAgentOptions, config: &AppConfig) -> tokio::process::Command {
        let cli_path = options.cli_path.clone()
            .or_else(|| which::which("claude").ok())
            .unwrap_or_else(|| std::path::PathBuf::from("claude"));

        let mut cmd = tokio::process::Command::new(&cli_path);
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--input-format").arg("stream-json");
        cmd.arg("--no-verbose");   // debug 라인 억제

        if let Some(model) = &options.model {
            cmd.arg("--model").arg(model);
        }

        let perm_mode = options.permission_mode.as_ref()
            .map(|m| m.as_str())
            .unwrap_or("default");
        cmd.arg("--permission-mode").arg(perm_mode);

        if let Some(sp) = &options.system_prompt {
            cmd.arg("--system-prompt").arg(sp);
        }

        if let Some(session_id) = &options.resume {
            cmd.arg("--resume").arg(session_id);
        } else if options.continue_conversation {
            cmd.arg("--continue");
        }

        if let Some(max) = options.max_turns {
            cmd.arg("--max-turns").arg(max.to_string());
        }

        if let Some(tools) = &options.allowed_tools {
            cmd.arg("--allowedTools").arg(tools.join(","));
        }

        if let Some(tools) = &options.disallowed_tools {
            cmd.arg("--disallowedTools").arg(tools.join(","));
        }

        // MCP: temp JSON 파일 → --mcp-config
        // (mcp_servers 있을 때 tempfile에 쓰고 경로 전달)

        if let Some(betas) = &options.betas {
            for beta in betas {
                cmd.arg("--beta").arg(beta);
            }
        }

        if let Some(sources) = &options.setting_sources {
            cmd.arg("--setting-sources").arg(sources.join(","));
        } else {
            cmd.arg("--setting-sources").arg("");  // filesystem 설정 무시 (격리)
        }

        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        if let Some(env) = &options.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        cmd.stdin(std::process::Stdio::piped())
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::piped())
           .kill_on_drop(true);

        cmd
    }
}

// stdout 파서 태스크
async fn stdout_parser_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::Sender<Result<CliOutputEvent, GatewayError>>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();

        // 비JSON 라인 (debug 출력 등) skip
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            tracing::debug!("cli non-json stdout: {}", trimmed);
            continue;
        }

        let event = serde_json::from_str::<CliOutputEvent>(trimmed)
            .map_err(|e| GatewayError::JsonDecode {
                line: trimmed.to_string(),
                source: e,
            });

        if event_tx.send(event).await.is_err() {
            break;  // 수신자 없음 → 종료
        }
    }
}
```

### 5.3 세션 생성 + 이벤트 루프 패턴

```rust
pub async fn create_session(
    options: ClaudeAgentOptions,
    store: Arc<SessionStore>,
    config: Arc<AppConfig>,
) -> Result<Arc<Session>, GatewayError> {
    // 동시 세션 한도 체크
    if store.count() >= config.server.max_sessions {
        return Err(GatewayError::SessionLimitReached { max: config.server.max_sessions });
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);
    let (event_tx, _) = broadcast::channel::<Message>(256);
    let history = Arc::new(Mutex::new(Vec::<Message>::new()));
    let state = Arc::new(Mutex::new(SessionState::Initializing));

    let session = Arc::new(Session {
        id: session_id.clone(),
        cli_session_id: None,    // system:init 후 설정
        state: state.clone(),
        created_at: std::time::Instant::now(),
        options: options.clone(),
        stdin_tx,
        event_tx: event_tx.clone(),
        history: history.clone(),
    });

    store.insert(session.clone())?;

    // 이벤트 루프 태스크 spawn
    let session_clone = session.clone();
    tokio::spawn(async move {
        if let Err(e) = run_session_loop(session_clone, options, config).await {
            tracing::error!("session {} loop error: {}", session_id, e);
        }
    });

    Ok(session)
}

async fn run_session_loop(
    session: Arc<Session>,
    options: ClaudeAgentOptions,
    config: Arc<AppConfig>,
) -> Result<(), GatewayError> {
    let mut transport = CliTransport::new(options.clone(), config.clone());
    transport.connect().await?;

    let mut stream = transport.read_messages();
    let mut init_done = false;

    while let Some(event) = stream.next().await {
        let cli_event = match event {
            Ok(e) => e,
            Err(e) => {
                let msg = Message::Error { message: e.to_string(), code: e.error_code().to_string() };
                broadcast_and_record(&session, msg).await;
                continue;
            }
        };

        // CliOutputEvent → Message 변환
        let message = match cli_event {
            CliOutputEvent::System(sys) => {
                // system:init 수신 → 상태 전환 + cli_session_id 저장
                if sys.subtype == SystemSubtype::Init {
                    // SAFETY: session.cli_session_id는 init 이전에 None
                    // 실제 구현 시 Arc<Mutex<Option<String>>> 또는 OnceCell 사용
                    init_done = true;
                    *session.state.lock().await = SessionState::Idle;
                }
                Message::System { ... }
            }
            CliOutputEvent::Assistant(a) => {
                *session.state.lock().await = SessionState::Running;
                Message::Assistant { ... }
            }
            CliOutputEvent::Result(r) => {
                *session.state.lock().await = SessionState::Completed;
                Message::Result { ... }
            }
            CliOutputEvent::HookRequest(h) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                *session.state.lock().await = SessionState::WaitingForHook {
                    hook_id: h.hook_id.clone(),
                    deadline,
                };

                // 서버 사이드 훅 규칙 먼저 평가
                let auto_decision = evaluate_hook_rules(&session.options, &h);
                if let Some(decision) = auto_decision {
                    // 바로 stdin에 응답 (HTTP round-trip 없음)
                    let resp = CliInputMessage::HookResponse { ... };
                    transport.write(&serde_json::to_string(&resp)?).await?;
                    *session.state.lock().await = SessionState::Running;
                } else {
                    // SSE로 클라이언트에 위임 (30초 타임아웃)
                    // hook_response_handler가 stdin_tx로 응답 보내줌
                }

                Message::HookRequest { ... }
            }
            CliOutputEvent::StreamEvent(se) => Message::StreamEvent { ... },
            CliOutputEvent::User(_) => continue,  // 내부 tool_result, 외부 노출 옵션
        };

        broadcast_and_record(&session, message).await;
    }

    *session.state.lock().await = SessionState::Dead;
    Ok(())
}

async fn broadcast_and_record(session: &Session, message: Message) {
    session.history.lock().await.push(message.clone());
    let _ = session.event_tx.send(message);  // 구독자 없어도 무시
}
```

### 5.4 SSE 핸들러 패턴

```rust
use axum::response::sse::{Event, KeepAlive, Sse};

pub async fn stream_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    TypedHeader(last_event_id): Option<TypedHeader<LastEventId>>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, AppError> {
    let session = state.sessions.get(&session_id)?;

    // Last-Event-ID: missed 이벤트 재전송
    let start_idx = last_event_id
        .and_then(|h| h.0.parse::<usize>().ok())
        .unwrap_or(0);

    // 히스토리에서 missed 이벤트 + 신규 이벤트 스트림 합성
    let history = session.history.lock().await.clone();
    let missed = history.into_iter().skip(start_idx);

    let mut rx = session.event_tx.subscribe();

    let stream = async_stream::stream! {
        // missed 이벤트 먼저 전송
        for (idx, msg) in missed.enumerate() {
            let data = serde_json::to_string(&msg).unwrap_or_default();
            yield Ok(Event::default()
                .id((start_idx + idx).to_string())
                .data(data));
        }

        // 신규 이벤트
        let mut current_idx = session.history.lock().await.len();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let data = serde_json::to_string(&msg).unwrap_or_default();
                    yield Ok(Event::default()
                        .id(current_idx.to_string())
                        .data(data));
                    current_idx += 1;

                    // result 또는 dead → 스트림 종료
                    if matches!(msg, Message::Result { .. } | Message::Error { .. }) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // 느린 클라이언트: 건너뛴 이벤트 수 알림
                    let err = Message::Error {
                        message: format!("Lagged: {} events skipped", n),
                        code: "stream_lagged".to_string(),
                    };
                    yield Ok(Event::default().data(serde_json::to_string(&err).unwrap()));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
```

### 5.5 Hook response 처리 패턴

```rust
pub async fn hook_response(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<HookResponseRequest>,
) -> Result<StatusCode, AppError> {
    let session = state.sessions.get(&session_id)?;

    // 상태 확인
    let current_state = session.state.lock().await.clone();
    match &current_state {
        SessionState::WaitingForHook { hook_id, deadline } => {
            if hook_id != &body.hook_id {
                return Err(AppError::conflict("hook_id mismatch"));
            }
            if std::time::Instant::now() > *deadline {
                return Err(AppError::timeout("hook already timed out"));
            }
        }
        _ => return Err(AppError::conflict("session not waiting for hook")),
    }

    // stdin에 응답 전송
    let msg = CliInputMessage::HookResponse {
        hook_id: body.hook_id,
        decision: body.decision,
        reason: body.reason,
        updated_input: body.updated_input,
        suppress_output: None,
    };
    let json = serde_json::to_string(&msg).map_err(GatewayError::from)?;
    session.stdin_tx.send(json).await
        .map_err(|_| GatewayError::Internal("stdin closed".to_string()))?;

    *session.state.lock().await = SessionState::Running;
    Ok(StatusCode::ACCEPTED)
}
```

---

## 6. 구현 로드맵 (RALP 루프)

> **RALP = Read → Act → Loop (verify) → Pause(checkpoint)**  
> 각 Phase는 자율 실행 가능한 단위. Checkpoint에서만 사람 확인.

### Phase 1: Foundation (약 1일)

> **진입 조건**: `cargo new claude-agent-rs --lib` 완료  
> **목표**: CLI와 stdin/stdout JSON 통신 성공 + 타입 시스템 구축

**태스크 (순서 엄수)**

**T1-1**: Cargo.toml 설정
```bash
# 검증
cargo build 2>&1 | grep -c "error" && echo "FAIL" || echo "PASS"
```

**T1-2**: `src/messages/cli_output.rs` — 섹션 3.1의 CliOutputEvent 완전 구현  
**T1-3**: `src/messages/cli_input.rs` — 섹션 3.2의 CliInputMessage 완전 구현  
**T1-4**: `src/messages/content.rs` — ContentBlock, TokenUsage, SessionUsage  
**T1-5**: `src/error.rs` — GatewayError (섹션 3.4)  
**T1-6**: `src/options.rs` — ClaudeAgentOptions, PermissionMode, McpServerConfig (섹션 3.6)
```bash
# 검증
cargo check 2>&1 | grep -c "error" && echo "FAIL" || echo "PASS"
```

**T1-7**: `tests/fixtures/` — 실제 CLI 실행해서 fixture 수집
```bash
# fixture 수집 명령 (claude CLI 설치 전제)
claude --output-format stream-json --input-format stream-json \
  --no-verbose --setting-sources "" \
  --max-turns 1 <<< '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"say hi"}]}}' \
  > tests/fixtures/full_run.ndjson
```

**T1-8**: `tests/messages_parse_test.rs` — fixture 파싱 테스트
```rust
#[test]
fn test_parse_system_init() {
    let raw = include_str!("fixtures/system_init.json");
    let event: CliOutputEvent = serde_json::from_str(raw).unwrap();
    assert!(matches!(event, CliOutputEvent::System(_)));
}
// result, assistant, tool_use, hook_request fixture도 동일하게
```
```bash
# 검증
cargo test messages_parse 2>&1 | tail -5
# 기대: "test result: ok. N passed; 0 failed"
```

**T1-9**: `src/transport/mod.rs` + `src/transport/cli.rs` — Transport trait + CliTransport (섹션 5.2)  
**T1-10**: `src/config.rs` — AppConfig
```toml
[server]
host = "127.0.0.1"
port = 8765
max_sessions = 100

[cli]
bin_path = "claude"       # "" = auto-detect via PATH
session_idle_timeout_secs = 1800
```

**T1-11**: `src/query.rs` — 단일 query() 함수 (session 없이 subprocess 직접 구동)

**Phase 1 완료 검증**:
```bash
# smoke test: 실제 CLI 구동 (claude 설치 전제)
RUST_LOG=debug cargo run --bin claude-agent-rs -- --check-cli
# 기대: "claude CLI found at /usr/local/bin/claude (version x.x.x)"

cargo test 2>&1 | tail -10
# 기대: "test result: ok. N passed; 0 failed; 0 ignored"
```

---

### Phase 2: 세션 API (약 1일)

> **진입 조건**: Phase 1 완료 검증 통과  
> **목표**: multi-turn 세션 + SSE 스트리밍 동작

**T2-1**: `src/session/mod.rs` + `src/session/store.rs` — Session, SessionStore (섹션 3.5)  
**T2-2**: 세션 생성 로직 (섹션 5.3 run_session_loop 구현)  
**T2-3**: `src/bin/server.rs` — axum 서버 기본 구조 + AppState  
**T2-4**: `src/api/admin.rs` — `GET /health`, `GET /stats`  
**T2-5**: `src/api/query.rs` — `POST /query`, `POST /query/stream`  
**T2-6**: `src/api/sessions.rs` — `POST /sessions`, `DELETE /sessions/{id}`, `POST /sessions/{id}/send`, `GET /sessions/{id}/stream`  

**Phase 2 완료 검증**:
```bash
# 서버 실행
cargo run &
sleep 2

# health check
curl -s http://localhost:8765/health | jq '.status'
# 기대: "ok"

# single query (non-streaming)
curl -s -X POST http://localhost:8765/query \
  -H "Content-Type: application/json" \
  -d '{"prompt":"2+2는 얼마야? 한 줄로만 답해"}' | jq '.result'
# 기대: "4" 또는 유사

# SSE 스트리밍
curl -sN -X POST http://localhost:8765/query/stream \
  -H "Content-Type: application/json" \
  -d '{"prompt":"hello"}' 2>&1 | head -20
# 기대: data: {"type":"system"...}, data: {"type":"assistant"...}, data: {"type":"result"...}

# multi-turn session
SESSION_ID=$(curl -s -X POST http://localhost:8765/sessions \
  -H "Content-Type: application/json" \
  -d '{}' | jq -r '.session_id')
echo "session: $SESSION_ID"

# 첫 메시지 전송
curl -s -X POST "http://localhost:8765/sessions/$SESSION_ID/send" \
  -H "Content-Type: application/json" \
  -d '{"message":"내 이름을 Alex라고 불러줘"}' 

# SSE 확인 (별도 터미널)
curl -sN "http://localhost:8765/sessions/$SESSION_ID/stream" | head -10

# 두 번째 메시지 (세션 유지 확인)
sleep 3
curl -s -X POST "http://localhost:8765/sessions/$SESSION_ID/send" \
  -H "Content-Type: application/json" \
  -d '{"message":"내 이름이 뭐야?"}'
```

**Checkpoint 1** ← 사람 확인: multi-turn 컨텍스트 유지 확인

---

### Phase 3: Hook + Permission (약 1일)

> **진입 조건**: Phase 2 Checkpoint 통과  
> **목표**: hook SSE round-trip 동작, 서버 사이드 규칙 평가

**T3-1**: `src/hooks/mod.rs` — hook 이벤트 수신 + SSE emit + 30초 타임아웃  
**T3-2**: `src/hooks/server_rules.rs` — `evaluate_hook_rules()` (섹션 5.3 참조)
```rust
// 규칙 평가 우선순위: block > approve > defer
// 패턴: glob 매칭 ("Bash", "Edit|Write", "*")
fn evaluate_hook_rules(options: &ClaudeAgentOptions, hook: &CliHookRequestEvent) -> Option<HookDecision>
```
**T3-3**: `src/api/hooks.rs` — `POST /sessions/{id}/hook_response` (섹션 5.5)  
**T3-4**: `src/permissions/mod.rs` — PermissionMode 평가 로직  
**T3-5**: `src/api/sessions.rs`에 `POST /sessions/{id}/interrupt` 추가  

**Phase 3 완료 검증**:
```bash
# hook round-trip 테스트: acceptEdits 모드로 세션 생성 후 파일 쓰기 요청
SESSION_ID=$(curl -s -X POST http://localhost:8765/sessions \
  -H "Content-Type: application/json" \
  -d '{"options":{"permission_mode":"default","hook_rules":[{"event":"PreToolUse","tool_pattern":"Write","action":{"Block":{"reason":"차단 테스트"}}}]}}' \
  | jq -r '.session_id')

curl -s -X POST "http://localhost:8765/sessions/$SESSION_ID/send" \
  -H "Content-Type: application/json" \
  -d '{"message":"/tmp/test.txt 파일에 hello라고 써줘"}'

# SSE에서 hook_request 이벤트 확인
# 기대: {"type":"hook_request","hook_event_name":"PreToolUse","tool_name":"Write",...}
# 이후 block 결과로 Write 미실행 확인
```

---

### Phase 4: MCP (약 1일)

> **진입 조건**: Phase 3 완료  
> **목표**: 외부 MCP 서버 연결 동작

**T4-1**: `src/mcp/config.rs` — McpServerConfig → temp JSON 파일 생성 + --mcp-config 플래그 연결  
**T4-2**: `src/mcp/mod.rs` — McpManager (서버 목록 관리, status 추적)  
**T4-3**: MCP stdio 서버 연결 테스트
```bash
# test: filesystem MCP 서버 사용
SESSION_ID=$(curl -s -X POST http://localhost:8765/sessions \
  -H "Content-Type: application/json" \
  -d '{
    "options": {
      "mcp_servers": {
        "filesystem": {
          "type": "stdio",
          "command": "npx",
          "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        }
      }
    }
  }' | jq -r '.session_id')
```

**T4-4**: `src/api/sessions.rs`에 `GET /sessions/{id}/messages` 추가 (히스토리 페이지네이션)  

---

### Phase 5: 세션 고급 기능 (약 0.5일)

> **목표**: resume, fork, 히스토리 조회

**T5-1**: `POST /sessions/{id}/fork` 구현
- 기존 세션의 cli_session_id를 `--resume`으로 새 subprocess 생성
- 새 server-side session_id 발급

**T5-2**: `GET /sessions` 목록 조회  
**T5-3**: 유휴 세션 자동 정리 background task
```rust
// AppState 초기화 시 spawn
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        sessions.run_cleanup(config.cli.session_idle_timeout_secs).await;
    }
});
```

**Phase 5 완료 검증**:
```bash
# fork 테스트
SESSION_A=$(curl -s -X POST http://localhost:8765/sessions | jq -r '.session_id')
curl -s -X POST "http://localhost:8765/sessions/$SESSION_A/send" \
  -d '{"message":"기억해: 숫자 42"}' -H "Content-Type: application/json"
sleep 5

SESSION_B=$(curl -s -X POST "http://localhost:8765/sessions/$SESSION_A/fork" \
  | jq -r '.session_id')

# A: 계속 대화
curl -s -X POST "http://localhost:8765/sessions/$SESSION_A/send" \
  -d '{"message":"방금 말한 숫자가 뭐야?"}' -H "Content-Type: application/json"

# B: fork 지점에서 다른 방향
curl -s -X POST "http://localhost:8765/sessions/$SESSION_B/send" \
  -d '{"message":"그 숫자에 1 더하면?"}' -H "Content-Type: application/json"
```

---

### Phase 6: 운영 안정화 (약 0.5일)

**T6-1**: `GET /config`, `PUT /config` 엔드포인트 (max_sessions, cli_path 런타임 변경)  
**T6-2**: stderr 로깅 개선 (CLI stderr → tracing::warn!)  
**T6-3**: graceful shutdown (SIGTERM → 진행 중인 세션 완료 대기 or 30초 후 강제 종료)  
**T6-4**: 바이너리 크기 최적화
```toml
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
```
**T6-5**: `cargo test --all` 최종 실행 + CI 환경에서 통합 테스트 skip 조건
```rust
#[tokio::test]
#[ignore = "requires claude CLI"]   // cargo test --ignored 로 실행
async fn test_real_cli_query() { ... }
```

**최종 검증**:
```bash
cargo build --release
ls -lh target/release/claude-agent-rs  # ≤ 10MB 확인

# smoke test suite
./target/release/claude-agent-rs &
sleep 1

curl -s http://localhost:8765/health | jq .
curl -s -X POST http://localhost:8765/query \
  -H "Content-Type: application/json" \
  -d '{"prompt":"ping"}' | jq .result

pkill claude-agent-rs
```

---

**Checkpoint 2 (최종)** ← 사람 확인: 전체 기능 시나리오 검증

---

## 7. 기능 요구사항 (FR) 매핑

### Core FR 목록 (위 로드맵에 매핑)

| FR | 설명 | Phase | 상태 |
|----|------|-------|------|
| FR-C01 | Transport Layer (CLI 통신) | P1 | - |
| FR-C02 | query() async stream | P2 | - |
| FR-C03 | ClaudeSDKClient (stateful) | P2 | - |
| FR-C04 | ClaudeAgentOptions 전체 | P1~P5 | - |
| FR-C05 | Message Type System | P1 | - |
| FR-C06 | ContentBlock Types | P1 | - |
| FR-C07 | MCP 통합 | P4 | - |
| FR-C08 | Hook System | P3 | - |
| FR-C09 | Permission System | P3 | - |
| FR-C10 | Session Management | P2, P5 | - |
| FR-C11 | Streaming | P2 | - |
| FR-C12 | Subagent / Orchestration | P6 이후 | - |
| FR-C13 | Context Management | P6 이후 | - |
| FR-C14 | Error Handling | P1 | - |
| FR-C15 | Authentication | P1 (CLI 위임) | - |
| FR-C16 | Cost/Usage Tracking | P2 (ResultMessage 파싱) | - |
| FR-C17 | HTTP REST API Gateway | P2~P5 | - |

### Extended FR (Phase 6 이후)

| FR | 설명 | 우선순위 |
|----|------|---------|
| FR-E01 | Structured Output | High |
| FR-E02 | Extended Thinking | High |
| FR-E11 | Effort Parameter | High |
| FR-E03 | Background Tasks | Medium |
| FR-E04 | Session History API | Medium |
| FR-E12 | Fallback Model | Medium |
| FR-E05 | Filesystem Features | Medium |
| FR-E06~E10 | 기타 | Low |

---

## 8. 실패 설계

| 지점 | 장애 유형 | 복구 전략 | 감지 방법 |
|------|----------|----------|----------|
| CLI 미설치 | 구성 오류 | startup 시 `which claude` 실패 → 503 + 설치 안내 | 서버 시작 시 검증 |
| subprocess crash | 프로세스 실패 | ProcessCrash 에러 emit → 세션 Dead 상태 → 클라이언트가 새 세션 생성 | exit 이벤트 + stdout EOF |
| stdin 쓰기 실패 | 파이프 끊김 | mpsc::send 에러 → 세션 Dead 처리 | send() Err 반환 |
| stdout JSON 파싱 실패 | 프로토콜 오류 | JsonDecode 에러 → 해당 라인 skip + tracing::warn! | serde_json 에러 |
| Hook 타임아웃 (30초) | 클라이언트 미응답 | 자동 approve + HookTimeout 이벤트 기록 | Instant + Duration |
| 동시 세션 한도 | 리소스 고갈 | 429 반환, Semaphore 필요 없음 (DashMap.len() 체크) | insert() 시 체크 |
| broadcast lagged | 느린 SSE 구독자 | RecvError::Lagged 처리 → 클라이언트에 skip 알림 | recv() 에러 처리 |
| 메모리 누수 | 장기 세션 히스토리 | idle_timeout_secs 초과 세션 자동 정리 | background task |

> **"subprocess 재연결" 개념 없음**: crash된 subprocess는 복구 불가. 클라이언트가 새 session을 생성해야 함.

---

## 9. 코딩 규칙 (Claude Code CLI 준수 필수)

```
1. unwrap() / expect() 금지 — 모든 에러는 ? 또는 map_err()
2. std::sync::Mutex 금지 — tokio::sync::Mutex 또는 DashMap 사용
3. std::process::Command 금지 — tokio::process::Command 사용
4. blocking I/O in async context 금지 — tokio::fs::* 사용
5. spawn() 내부 panic 전파 금지 — inspect_err(|e| tracing::error!(...)) 패턴
6. CLI stdout: '{' 시작 아닌 라인은 tracing::debug! 후 continue
7. system:init 수신 전 stdin 쓰기 금지
8. broadcast::channel capacity = 256 (세션당)
9. mpsc::channel capacity = 32 (stdin 쓰기)
10. 모든 public 타입: #[derive(Debug, Clone, Serialize, Deserialize)]
```

---

## 10. 참고 자료

- Agent SDK TypeScript 레퍼런스: https://platform.claude.com/docs/en/agent-sdk/typescript
- Agent SDK Python 레퍼런스: https://platform.claude.com/docs/en/agent-sdk/python
- V2 Session API: https://platform.claude.com/docs/en/agent-sdk/typescript-v2-preview
- Streaming 출력: https://platform.claude.com/docs/en/agent-sdk/streaming-output
- stream-json 비공식 문서: https://github.com/udhaykumarbala/claude-code-parser
- Hooks 가이드: https://platform.claude.com/docs/en/agent-sdk/ (hooks 항목)
