# OpenAI 통합 개선 요건 정의서

> 작성일: 2026-05-04  
> 배경: sas-agent에서 OpenAI 모델(gpt-4o, o3 등)을 Anthropic 모델과 동일한 방식으로 사용하기 위한 gateway 개선 요건.

---

## 배경 및 동기

### sas-agent의 gateway 사용 방식

`sas-agent`는 `GatewayProvider` (Messages 모드)를 통해 gateway와 통신함:

```
sas-agent
  GatewayProvider (GatewayFormat::Messages)
    → POST /v1/messages   { model, system, messages, max_tokens }
    → gateway → Anthropic API
```

모델명(`haiku`, `sonnet`, `opus`)으로 Anthropic 모델을 선택함.  
OpenAI 모델을 쓰려면 gateway가 모델명을 보고 OpenAI 채널로 라우팅해줘야 함.  
**sas-agent 코드 변경 없이** 모델명만 바꿔서 OpenAI 모델을 쓸 수 있는 것이 목표.

### 현재 OpenAI 채널의 문제점

| 엔드포인트 | 현황 | 문제 |
|---|---|---|
| `/openai-oauth/v1/chat/completions` | OAuth 전용, upstream이 `chatgpt.com` 비표준 | 표준 API 아님 |
| `/openai/v1/responses` | `OPENAI_API_KEY` 전용, Responses API | ChatCompletion 포맷 아님 |
| `/v1/messages` (OpenAI 모델) | 미지원 | sas-agent 연동 불가 |

---

## 요건 1 (P0): `OPENAI_API_KEY` 기반 ChatCompletion 엔드포인트 추가

### 목적

`OPENAI_API_KEY`가 설정된 환경에서 표준 OpenAI Chat Completions API를 호출할 수 있는 엔드포인트 추가.

### 구현 내용

**새 엔드포인트:** `POST /openai/v1/chat/completions`

**동작:**
1. `OPENAI_API_KEY` 환경변수 확인 → `api.openai.com/v1/chat/completions` 호출
2. API 키 없으면 OAuth 폴백 → 기존 `openai_oauth::chat_completions()` 호출
3. 요청 body는 변환 없이 upstream으로 전달 (기존 OAuth 엔드포인트와 동일)

**라우팅 추가 위치:** `src/api/openai.rs` (기존 `/openai/v1/responses` 핸들러 옆)

```rust
// src/api/openai.rs에 추가
.route("/openai/v1/chat/completions", post(chat_completions_handler))
```

**핸들러 로직:**
```
1. state.openai (API key 기반) 있으면
   → openai_proxy::chat_completions(proxy_state, body) 호출
   → api.openai.com/v1/chat/completions 로 전달
2. state.openai_oauth 있으면 OAuth 폴백
3. 둘 다 없으면 501
```

**`openai_proxy.rs`에 추가 함수:**
```rust
pub async fn chat_completions(
    state: &OpenAiProxyState,
    body: Value,
) -> Result<(reqwest::Response, u16), GatewayError>
```
- `state.base_url` (기본: `https://api.openai.com`) + `/v1/chat/completions` 로 POST
- 헤더: `Authorization: Bearer {api_key}`, `Content-Type: application/json`

---

## 요건 2 (P1): `/v1/messages` 모델 기반 OpenAI 라우팅

### 목적

sas-agent가 `/v1/messages` 포맷으로 요청할 때, 모델명이 OpenAI 계열이면 자동으로 OpenAI ChatCompletion 채널로 변환·전달.

### 모델 감지 규칙

다음 패턴이 `model` 필드에 포함되면 OpenAI 라우팅:
- `gpt-` 로 시작
- `o1`, `o3`, `o4` 로 시작
- 설정 파일의 `openai_models` 목록에 포함

### Anthropic Messages → OpenAI ChatCompletion 변환

**요청 변환:**

```
Anthropic Messages 입력:
{
  "model": "gpt-4o",
  "system": [{"type":"text","text":"You are an agent.","cache_control":{...}}],
  "messages": [{"role":"user","content":"Do something."}],
  "max_tokens": 1000
}

OpenAI ChatCompletion 출력:
{
  "model": "gpt-4o",
  "messages": [
    {"role":"system","content":"You are an agent."},  // system 블록 → messages 첫 번째
    {"role":"user","content":"Do something."}
  ],
  "max_tokens": 1000
}
```

**변환 규칙:**
- `system` 필드 (문자열 또는 배열) → `{"role":"system","content":"..."}` 로 messages 맨 앞에 삽입
- `system` 배열의 경우 `text` 필드만 추출해서 join
- `cache_control` 필드 제거 (OpenAI 미지원)
- `max_tokens` → `max_tokens` (동일)

**응답 역변환:**

```
OpenAI ChatCompletion 응답:
{
  "choices": [{"message": {"content": "..."}, "finish_reason": "stop"}],
  "model": "gpt-4o",
  "usage": {"prompt_tokens": 100, "completion_tokens": 50}
}

Anthropic Messages 응답 형식으로 변환:
{
  "content": [{"type": "text", "text": "..."}],
  "model": "gpt-4o",
  "stop_reason": "end_turn",
  "usage": {"input_tokens": 100, "output_tokens": 50}
}
```

**변환 규칙:**
- `choices[0].message.content` → `content[0].text`
- `choices[0].finish_reason` → `stop_reason` (`stop` → `end_turn`, `length` → `max_tokens`)
- `usage.prompt_tokens` → `usage.input_tokens`
- `usage.completion_tokens` → `usage.output_tokens`

---

## 요건 3 (P1): 이미지 포맷 변환 (요건 2와 세트)

### 목적

요건 2의 변환 로직에서 이미지 content 블록을 Anthropic → OpenAI 포맷으로 변환.

### 변환 규칙

```
Anthropic 이미지 블록:
{
  "type": "image",
  "source": {
    "type": "base64",
    "media_type": "image/png",
    "data": "<base64_string>"
  }
}

OpenAI 이미지 블록:
{
  "type": "image_url",
  "image_url": {
    "url": "data:image/png;base64,<base64_string>"
  }
}
```

**적용 위치:** messages 배열 내 user message의 content가 배열일 때 각 블록 순회하여 변환.

---

## 요건 4 (P2): 모델 별칭 테이블

### 목적

sas-agent 설정에서 짧은 별칭으로 OpenAI 모델을 지정할 수 있도록.

### 구현 방안

환경변수 또는 설정 파일로 별칭 정의:

```toml
# gateway.toml (예시)
[model_aliases]
"gpt4o"   = "gpt-4o"
"o3mini"  = "o3-mini-2025-01-31"
"o3"      = "o3-2025-04-16"
```

또는 환경변수:
```
GATEWAY_MODEL_ALIAS_gpt4o=gpt-4o
GATEWAY_MODEL_ALIAS_o3mini=o3-mini-2025-01-31
```

**적용 위치:** `/v1/messages` 수신 직후 model 필드 resolve 단계.

---

## 구현 파일 맵

| 파일 | 변경 종류 | 내용 |
|------|-----------|------|
| `src/api/openai.rs` | Modify | `/openai/v1/chat/completions` 엔드포인트 추가 (요건 1) |
| `src/openai_proxy.rs` | Modify | `chat_completions()` 함수 추가 (요건 1) |
| `src/api/proxy.rs` | Modify | `/v1/messages` 핸들러에 OpenAI 분기 추가 (요건 2) |
| `src/transform.rs` | **New** | Anthropic ↔ OpenAI 포맷 변환 함수 (요건 2, 3) |
| `src/config.rs` | Modify | 모델 별칭 테이블 로딩 (요건 4) |

---

## 테스트 케이스 (구현 시 참고)

### 요건 1
- `OPENAI_API_KEY` 있을 때 `/openai/v1/chat/completions` → `api.openai.com` 전달 확인
- API 키 없고 OAuth 있을 때 OAuth 폴백 확인
- 둘 다 없을 때 501 반환 확인

### 요건 2
- `model: "gpt-4o"` 요청이 OpenAI 채널로 라우팅되는지 확인
- `model: "claude-sonnet-4-6"` 요청은 Anthropic 채널 유지 확인
- system 블록 변환 정확성 (문자열 / 배열 / cache_control 포함 케이스)
- 응답 역변환 정확성 (content, stop_reason, usage 필드)

### 요건 3
- base64 이미지 블록이 `image_url` 포맷으로 변환되는지 확인
- 텍스트 블록은 변환 없이 통과되는지 확인
- 이미지 + 텍스트 혼합 content 배열 처리 확인

---

## 우선순위 요약

| 요건 | 우선순위 | 예상 작업량 |
|------|--------|------------|
| 요건 1: API key ChatCompletion 엔드포인트 | **P0** | 소 (함수 1개 추가) |
| 요건 2: `/v1/messages` OpenAI 라우팅 | **P1** | 중 (변환 로직) |
| 요건 3: 이미지 포맷 변환 | **P1** | 소 (요건 2와 세트) |
| 요건 4: 모델 별칭 테이블 | **P2** | 소 (설정 파싱) |
