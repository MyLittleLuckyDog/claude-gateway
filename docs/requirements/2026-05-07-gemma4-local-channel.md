# 로컬 Gemma 4 채널 — 게이트웨이 노출 요구사항

- 작성일: 2026-05-07
- 선행 컨텍스트:
  - `2026-05-06-openai-oauth-stagehand-integration.md` (OpenAI 채널 1차)
  - `2026-05-06-followup-reasoning-effort-normalization.md` (OpenAI 채널 후속)
- 대상 게이트웨이: `http://127.0.0.1:8765`

## 현재 상태 (점검 결과)

다음 경로를 모두 시도했으나 **404**:
```
/gemma/v1/models, /gemma3/v1/models, /gemma4/v1/models,
/google-gemma/v1/models, /google-genai/v1/models, /genai/v1/models,
/local/v1/models, /local-llm/v1/models, /local-gemma/v1/models,
/ollama/v1/models, /ollama/api/tags,
/llamacpp/v1/models, /llama/v1/models, /lm/v1/models, /lmcli/v1/models,
/lmstudio/v1/models, /inference/v1/models,
/vertex/v1/models, /vertexai/v1/models,
/gemini/v1/models, /google-gemini/v1/models, /google/v1/models
```

기존에 노출된 모델은 그대로 6종 (`/openai/v1/models` = `/openai-oauth/v1/models` = Codex OAuth):
`gpt-5.5, gpt-5.4, gpt-5.4-mini, gpt-5.3-codex, gpt-5.2, codex-auto-review`.

**즉, 로컬 Gemma 4 백엔드는 띄워져 있으나 게이트웨이가 그쪽으로 라우팅하지 않습니다.**

## 요구사항

### [필수] 1. 라우트 노출

신규 prefix를 정해 다음 두 엔드포인트를 노출:

```
GET  /gemma/v1/models
POST /gemma/v1/chat/completions
```

또는 prefix를 일반화하려면 `/local/v1/...` 권장 (향후 다른 로컬 모델 추가 대비).

**검수 기준**:
- `GET /gemma/v1/models`가 OpenAI 표준 형식 반환:
  ```json
  {"object":"list","data":[{"id":"gemma-4-27b","object":"model","owned_by":"google","created":0}]}
  ```
- `POST /gemma/v1/chat/completions` 가 OpenAI Chat Completions 표준 응답 반환

### [필수] 2. OpenAI Chat Completions 인터페이스

Stagehand가 사용하는 Vercel AI SDK는 `@ai-sdk/openai` 또는 `@ai-sdk/google` 둘 중 하나를 통해 호출됩니다. **OpenAI 호환 인터페이스가 가장 호환성이 높음** (AI SDK가 OpenAI 표준 스펙으로 빌드됨, Codex OAuth 채널에서 겪었던 스키마 누락 문제를 처음부터 회피 가능).

**요청 페이로드 (OpenAI Chat Completions)**:
```json
{
  "model": "gemma-4-27b",
  "messages": [
    {"role": "system", "content": "You are concise."},
    {"role": "user", "content": "Hello"}
  ],
  "tools": [...],
  "tool_choice": "auto",
  "stream": false,
  "temperature": 0.7
}
```

**응답 페이로드 (OpenAI Chat Completions, non-streaming)**:
```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "created": 1778075839,
  "model": "gemma-4-27b",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "...",
      "tool_calls": [...]
    },
    "finish_reason": "stop"
  }],
  "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
}
```

스트리밍 (`stream: true`)도 필수 — AI SDK 일부 코드패스가 SSE만 사용.

### [필수] 3. 도구 호출 (Tool Calling) 지원

Stagehand의 `agent.execute()`는 도구 호출 멀티턴이 핵심. Codex OAuth 채널은 이 부분에서 막혔습니다 (이전 문서 참고). Gemma 4 채널은 처음부터 다음을 만족해야 합니다:

- 요청의 `tools[]` 필드 그대로 모델에 전달
- 모델이 함수를 호출하면 응답 `choices[0].message.tool_calls[]`로 표준 형식 반환:
  ```json
  "tool_calls": [{
    "id": "call_xxx",
    "type": "function",
    "function": {"name": "click", "arguments": "{\"selector\":\"...\"}"}
  }]
  ```
- `finish_reason: "tool_calls"`로 표시

**Gemma 4 본체가 OpenAI 형식 tool calling을 직접 지원하지 않으면**, 게이트웨이가 다음을 변환해야 함:
- (요청) OpenAI tools → 모델 native function/tool 형식
- (응답) 모델 native tool output → OpenAI `tool_calls`

실패 시 증상: agent가 1 step에서 "Task execution completed" 같은 빈 응답으로 즉시 종료 (Codex OAuth 채널에서 정확히 이 패턴을 관찰함).

### [필수] 4. 응답 스키마 완전성

Codex OAuth 채널에서 보였던 누락 필드를 처음부터 채울 것:
- 각 메시지/응답에 고유 `id` (string)
- `usage` 객체에 `prompt_tokens`, `completion_tokens`, `total_tokens` 모두 포함
- 비어있어도 `[]` 또는 `null`로 명시 (undefined 금지 — Zod 검증 실패 원인)

### [선택] 5. 인증 헤더

로컬 모델이라 인증 불필요한 경우, 클라이언트가 보낸 `Authorization: Bearer ...` 헤더는 무시. 단, 헤더가 비어있어도 거부하지 않을 것 (AI SDK가 항상 더미 키 보냄).

### [선택] 6. CORS / 디버그 헤더

```
x-gateway-route: gemma
x-gateway-backend: <upstream-url>
```
같은 디버그 헤더가 있으면 추후 트러블슈팅에 유용.

## Stagehand 통합 검증 시나리오

게이트웨이 작업 완료 후 다음 스니펫이 통과해야 합니다.

```typescript
import { Stagehand } from "@browserbasehq/stagehand";

const stagehand = new Stagehand({
  env: "LOCAL",
  model: {
    modelName: "openai/gemma-4-27b",         // openai/ prefix는 AI SDK 라우팅용
    apiKey: "local-gateway",
    baseURL: "http://127.0.0.1:8765/gemma/v1",
  },
});
await stagehand.init();
const page = stagehand.context.pages()[0];
await page.goto("https://example.com");

// 1) 단발 LLM 호출 — extract
const { extraction } = await stagehand.extract("extract the heading text");
console.log(extraction); // 기대: "Example Domain"

// 2) 도구 호출 — observe (단일 turn)
const observed = await stagehand.observe("the more information link");
console.log(observed[0]); // 기대: {method:"click", selector:"xpath=...", ...}

// 3) Agent — 멀티턴 도구 호출 (가장 까다로움)
await page.goto("https://github.com/browserbase/stagehand", {
  waitUntil: "domcontentloaded",
});
const agent = stagehand.agent({
  model: {
    modelName: "openai/gemma-4-27b",
    apiKey: "local-gateway",
    baseURL: "http://127.0.0.1:8765/gemma/v1",
  },
});
const result = await agent.execute({
  instruction:
    "Open the Pull requests tab. Find the most recently opened PR. " +
    "Reply with PR number, title, and author.",
  maxSteps: 15,
});
console.log(result.message);
console.log(`steps: ${result.actions?.length}`);
// 기대: steps >= 3, message에 PR 정보 포함
```

## curl 회귀 테스트

```bash
# 모델 목록
curl -s http://127.0.0.1:8765/gemma/v1/models | jq '.data[].id'
# 기대: "gemma-4-27b" (또는 실제 띄운 모델 id)

# 단순 채팅
curl -s -X POST http://127.0.0.1:8765/gemma/v1/chat/completions \
  -H "content-type: application/json" \
  -d '{
    "model":"gemma-4-27b",
    "messages":[{"role":"user","content":"reply pong"}],
    "stream":false
  }' | jq '.choices[0].message.content'
# 기대: "pong"

# 도구 호출
curl -s -X POST http://127.0.0.1:8765/gemma/v1/chat/completions \
  -H "content-type: application/json" \
  -d '{
    "model":"gemma-4-27b",
    "messages":[{"role":"user","content":"What is the weather in Seoul?"}],
    "tools":[{
      "type":"function",
      "function":{
        "name":"get_weather",
        "description":"Get current weather",
        "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
      }
    }],
    "tool_choice":"auto",
    "stream":false
  }' | jq '.choices[0].message.tool_calls'
# 기대: [{"id":"call_...", "type":"function", "function":{"name":"get_weather","arguments":"{\"city\":\"Seoul\"}"}}]
```

## 확인 부탁드리는 사항

게이트웨이 작업 시작 전 다음 정보가 필요합니다:

1. **로컬 Gemma 4 서버의 백엔드** — Ollama? llama.cpp? LM Studio? Vertex AI 로컬? 다른?
2. **백엔드의 호스트:포트** — 예: `http://127.0.0.1:11434`
3. **백엔드가 노출하는 인터페이스** — OpenAI 호환? Ollama API? 자체?
4. **모델 식별자(id)** — 게이트웨이를 통해 노출할 정확한 모델명
5. **도구 호출 지원 여부** — 백엔드가 OpenAI tool spec을 직접 처리하는지

위 정보가 확인되면 게이트웨이 라우트 구현이 단순 패스스루(거의 0줄 수준)에서 변환 어댑터(중간 정도) 사이에서 결정됩니다.

## 우선순위

| 항목 | 작업량 | 우선순위 |
|---|---|---|
| 1. 라우트 노출 + 모델 목록 + 단순 채팅 패스스루 | 작음 | **P0** — 우선 단발 호출만 되도 가치 큼 |
| 2. 스트리밍 SSE 지원 | 작음~중 | P1 |
| 3. 도구 호출 변환 (Codex OAuth 교훈 적용) | 중~큼 | P1 — agent 시나리오 필수 |
| 4. 응답 스키마 완전성 (id/usage/etc) | 작음 | P0 — 처음부터 챙겨야 후속 패치 비용 ↓ |
