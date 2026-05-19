# OpenAI OAuth 채널 — Stagehand 연동을 위한 수정 요구사항

- 작성일: 2026-05-06
- 요청자: stagehand-v3 환경 통합 작업 (juryu)
- 대상 게이트웨이: `http://127.0.0.1:8765`
- 영향 범위: `/openai-oauth/v1/*` 라우트만 (Anthropic 채널 `/v1/messages`는 정상 동작 중)

## 배경

stagehand-v3 (Browserbase 사의 AI 브라우저 자동화 프레임워크)에서
LLM 호출을 본 게이트웨이를 통해 라우팅하려고 합니다. 현재 Anthropic 채널은
정상 동작하지만, OpenAI OAuth 채널에 두 가지 차단 요인이 있어
사용 자체가 불가능합니다.

| 채널 | 상태 |
|---|---|
| `/v1/messages` (Anthropic) | ✅ 정상 — `anthropic/claude-haiku-4-5` 등 사용 검증 완료 |
| `/openai-oauth/v1/responses` (OpenAI OAuth) | ❌ 본 문서에서 다룸 |

## 재현 방법

게이트웨이가 떠 있는 상태에서 다음 curl 명령들을 실행해 동일 에러를 재현할 수 있습니다.

```bash
# 1) 정상 형식이라도 무조건 500
curl -s -X POST http://127.0.0.1:8765/openai-oauth/v1/responses \
  -H "content-type: application/json" \
  -d '{
    "model": "gpt-5.4-mini",
    "stream": true,
    "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}]
  }'
# → {"error":{"code":"internal_error","message":"Internal error: OAuth responses JSON decode failed: error decoding response body"}}

# 2) reasoning.effort 추가해도 동일
curl -s -X POST http://127.0.0.1:8765/openai-oauth/v1/responses \
  -H "content-type: application/json" \
  -d '{
    "model": "gpt-5.4",
    "stream": true,
    "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
    "reasoning": {"effort":"low"}
  }'
# → 동일 에러

# 3) gpt-5.5 / gpt-5.4-mini / gpt-5.3-codex / gpt-5.2 / codex-auto-review
#    모든 모델에서 동일 에러 발생
```

검증한 입력 변형 (모두 동일 500 에러로 귀결):
- `input` 문자열 → `400 "Input must be a list"` (의도된 거부, 실제 본 이슈 아님)
- `input` 배열 + `stream: false` → `400 "Stream must be set to true"` (의도된 거부)
- `input` 배열 + `stream: true` (정상 형태) → **500 "OAuth responses JSON decode failed"**
- `instructions`, `reasoning.effort`, `store: false` 등 추가 필드 조합 → 동일

추정: 업스트림 OpenAI OAuth 응답 본문을 JSON으로 디코딩하는 단계에서 실패.
업스트림이 SSE(Server-Sent Events) 또는 비-JSON 형식으로 회신하는데,
게이트웨이가 본문 전체를 단일 JSON으로 파싱하려고 시도하는 것으로 보임.

## 요구사항 (우선순위 순)

### [필수 1] `/openai-oauth/v1/responses` 정상화

**목표**: 위 재현 명령이 OpenAI Responses API 표준 응답을 반환해야 함.

**검수 기준**:
- `stream: true` 요청 시 SSE 청크가 정상 스트리밍됨 (`event: response.output_text.delta` 등)
- `stream: false` 도 받아들여야 함 (Stagehand가 사용하는 AI SDK는 비-스트리밍 호출도 함). 만약 업스트림이 항상 stream을 강제한다면, 게이트웨이가 SSE를 누적해서 단일 JSON 응답으로 합쳐 반환하는 어댑팅 필요.
- 에러 응답은 OpenAI 표준 포맷 유지: `{"error":{"type":"...","code":"...","message":"..."}}`

### [필수 2] `/openai-oauth/v1/chat/completions` 어댑터 추가

**왜 필요한가**:
Stagehand 내부에서는 Vercel AI SDK (`@ai-sdk/openai`)를 사용합니다.
이 SDK가 `openai("gpt-5.4-mini")` 호출 시 기본적으로 향하는 엔드포인트는
**`POST /v1/chat/completions`** 입니다. Responses API는
`openai.responses(model)` 별도 팩토리를 명시해야만 사용되며,
Stagehand의 LLM 라우팅 코드(`packages/core/lib/v3/llm/LLMProvider.ts`)는
표준 팩토리만 호출하므로 코드를 포크하지 않는 한 Responses API를 사용할 수 없습니다.

**구현 방향**:
게이트웨이가 `/openai-oauth/v1/chat/completions` 요청을 받아
내부적으로 Responses API로 변환·중계하는 어댑터를 제공하면
Stagehand는 단순히 baseURL만 `http://127.0.0.1:8765/openai-oauth/v1`로
지정하는 것으로 즉시 사용 가능합니다.

**요청 변환 예시**:

OpenAI Chat Completions 요청 (Stagehand가 보내는 형태):
```json
{
  "model": "gpt-5.4-mini",
  "messages": [
    {"role": "system", "content": "You are concise."},
    {"role": "user", "content": "Hello"}
  ],
  "tools": [...],
  "tool_choice": "auto",
  "stream": false,
  "temperature": 0.0
}
```

Responses API로 변환 (게이트웨이 내부에서):
```json
{
  "model": "gpt-5.4-mini",
  "instructions": "You are concise.",
  "input": [
    {"role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
  ],
  "tools": [...],
  "tool_choice": "auto",
  "stream": true,
  "reasoning": {"effort": "low"}
}
```

**응답 변환**:
Responses API SSE → Chat Completions JSON (또는 SSE)으로 합치기.
주요 매핑:
- `response.output_text.delta` → `choices[0].delta.content` (스트리밍)
- `response.completed` → `choices[0].finish_reason: "stop"`
- 도구 호출(`response.output_item.added` 중 `type: "function_call"`) → `choices[0].message.tool_calls[]`
- usage 필드 그대로 매핑

### [필수 3] `/openai-oauth/v1/models` 수정

**현재 동작**:
```bash
curl http://127.0.0.1:8765/openai-oauth/v1/models
# → 400 "Field required: query.client_version"
```

**문제**: 업스트림에 `client_version` 쿼리 파라미터를 강제하는데
게이트웨이가 패스스루하지 않음.

**해결**:
- 게이트웨이가 자체적으로 적절한 기본 `client_version` 값 (예: `1.0.0` 또는
  `~/.codex/models_cache.json` 기준 값)을 업스트림으로 전달
- 또는 `~/.codex/models_cache.json`을 캐시로 사용해 업스트림 호출 없이 응답
- OpenAI 표준 형식으로 반환: `{"object":"list","data":[{"id":"gpt-5.4-mini","object":"model",...}, ...]}`

### [선택] 인증 헤더 무시 정책 명시

Stagehand 등 SDK는 거의 항상 `Authorization: Bearer <key>` 또는
`api-key: <key>` 헤더를 자동으로 붙입니다. 게이트웨이가 OAuth 채널에서
로컬 OAuth 토큰을 대신 사용한다면, 클라이언트가 보낸 키 헤더는
**무시(또는 검증만)** 되어야 합니다. 현재 어떻게 처리되는지 명문화 필요.

## 참고: 클라이언트 측 검증 방법

게이트웨이 수정 후 다음 단계로 검증할 수 있습니다.

### 1) curl 회귀 테스트

```bash
# Chat Completions (Stagehand가 실제 보내는 형태)
curl -s -X POST http://127.0.0.1:8765/openai-oauth/v1/chat/completions \
  -H "content-type: application/json" \
  -H "authorization: Bearer dummy" \
  -d '{
    "model": "gpt-5.4-mini",
    "messages": [{"role":"user","content":"reply pong"}],
    "stream": false
  }'
# 기대: {"id":"...","choices":[{"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{...}}

# 모델 목록
curl -s http://127.0.0.1:8765/openai-oauth/v1/models | jq '.data[].id'
# 기대: "gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex", "gpt-5.2", "codex-auto-review"
```

### 2) Stagehand 통합 검증

stagehand-v3 워크스페이스에서 (이미 환경 셋업 완료된 상태):

```typescript
import { Stagehand } from "@browserbasehq/stagehand";

const stagehand = new Stagehand({
  env: "LOCAL",
  model: {
    modelName: "openai/gpt-5.4-mini",
    apiKey: "local-gateway",
    baseURL: "http://127.0.0.1:8765/openai-oauth/v1",
  },
});
await stagehand.init();
const page = stagehand.context.pages()[0];
await page.goto("https://example.com");
const { extraction } = await stagehand.extract("extract the heading text");
console.log(extraction); // 기대: "Example Domain"
```

이 스크립트가 에러 없이 `Example Domain`을 출력하면 통합 성공.

## 부록: 현재 정상 동작하는 채널 (참고용)

```bash
# Anthropic Messages — 정상 동작
curl -s -X POST http://127.0.0.1:8765/v1/messages \
  -H "content-type: application/json" \
  -d '{"model":"claude-haiku-4-5","max_tokens":20,"messages":[{"role":"user","content":"ping"}]}'
# → 정상 응답
```

Stagehand에서:
```typescript
model: {
  modelName: "anthropic/claude-haiku-4-5",
  apiKey: "local-gateway",
  baseURL: "http://127.0.0.1:8765/v1",
}
```

이 형태가 이미 검증되어 있으므로, OpenAI OAuth 채널도
유사한 사용성을 목표로 하면 됩니다.
