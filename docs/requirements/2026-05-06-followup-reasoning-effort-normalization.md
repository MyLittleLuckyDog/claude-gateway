# 후속 요구사항 — `reasoning.effort: "minimal"` 정규화

- 작성일: 2026-05-06 (PID 10866 게이트웨이 검증 후)
- 선행 문서: `2026-05-06-openai-oauth-stagehand-integration.md`

## 진척 상황 (감사 인사 먼저)

이전 요구사항 3건이 모두 반영되어 동작 확인했습니다:

- ✅ `/openai-oauth/v1/models` — 6개 모델 정상 목록 반환
- ✅ `/openai-oauth/v1/responses` (stream true/false) — 정상 응답
- ✅ `/openai-oauth/v1/chat/completions` (stream true/false) — 어댑터 정상 동작
  - `gpt-5.4-mini`로 `"reply with the single word: pong"` → `"pong"` 정상 회신 확인

## 새로 발견된 차단 요인

**Stagehand → AI SDK → 게이트웨이 → 업스트림** 통합 검증 중 발생.

### 재현
```bash
# AI SDK는 gpt-5 계열 모델을 reasoning 모델로 분류해
# 자동으로 reasoning.effort: "minimal" 을 주입함
curl -s -X POST http://127.0.0.1:8765/openai-oauth/v1/responses \
  -H "content-type: application/json" \
  -d '{
    "model": "gpt-5.4-mini",
    "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
    "stream": false,
    "reasoning": {"effort": "minimal"}
  }'
```

### 응답
```json
{
  "error": {
    "code": "unsupported_value",
    "type": "invalid_request_error",
    "param": "reasoning.effort",
    "message": "Unsupported value: 'minimal' is not supported with the 'gpt-5.4-mini' model. Supported values are: 'none', 'low', 'medium', 'high', and 'xhigh'."
  }
}
```

업스트림(OpenAI Codex OAuth)은 `'none' | 'low' | 'medium' | 'high' | 'xhigh'`만 허용합니다. 표준 OpenAI Responses API의 `'minimal'`은 **이 OAuth 채널의 모델군에서는 비유효 값**입니다.

## 왜 클라이언트 측에서 막을 수 없는가

`@ai-sdk/openai` (Vercel AI SDK 5.x)는 모델 ID가 `gpt-5*`로 시작하면 reasoning 모델로 간주하여 `reasoning.effort: "minimal"`을 **기본값으로 자동 주입**합니다. 호출 측에서 `providerOptions.openai.reasoningEffort`를 명시 설정하지 않으면 항상 들어갑니다.

Stagehand는 LLM 호출을 내부에서 추상화하므로(`packages/core/lib/v3/llm/aisdk.ts`, `LLMProvider.ts`) 사용자 코드에서 reasoning 옵션을 그때그때 주입하기 어렵습니다. 게다가 `extract`/`observe`/`act`/`agent` 각 핸들러가 별도로 LLM을 호출하기 때문에 모든 호출 지점을 일일이 패치해야 합니다.

→ **게이트웨이에서 한 번 정규화하는 편이 압도적으로 단순**합니다.

## 요구사항

### [필수] reasoning.effort 정규화 매핑

`/openai-oauth/v1/responses`와 `/openai-oauth/v1/chat/completions` 두 엔드포인트 모두에서, **업스트림으로 포워딩하기 직전**에 다음 매핑을 적용:

| 클라이언트 입력 | 업스트림으로 보낼 값 |
|---|---|
| `"minimal"` | `"low"` *(또는 필드 제거)* |
| `"none"` | `"none"` |
| `"low"` | `"low"` |
| `"medium"` | `"medium"` |
| `"high"` | `"high"` |
| `"xhigh"` | `"xhigh"` |
| (필드 없음) | (필드 없음) |

권장 구현: `"minimal"` → `"low"` 매핑 (업스트림 비용/지연 가장 가까움).

### [선택] 디버그 헤더

매핑이 일어났음을 알리는 응답 헤더를 추가하면 추후 디버깅에 유용합니다:
```
x-gateway-rewrite: reasoning.effort=minimal->low
```

## 검증

게이트웨이 수정 후, 같은 stagehand-v3 워크스페이스에서 다음 스니펫이 통과해야 합니다:

```typescript
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
// 기대: "Example Domain"
```

curl 회귀:
```bash
curl -s -X POST http://127.0.0.1:8765/openai-oauth/v1/responses \
  -H "content-type: application/json" \
  -d '{
    "model": "gpt-5.4-mini",
    "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
    "stream": false,
    "reasoning": {"effort": "minimal"}
  }'
# 기대: 정상 응답 (게이트웨이가 minimal→low로 변환 후 포워딩)
```

## 참고: 본 호출이 거치는 경로

```
stagehand.extract(...)
  → packages/core/lib/v3/handlers/extractHandler
  → packages/core/lib/v3/llm/aisdk.ts (AISdkClient)
  → ai 패키지 generateObject()
  → @ai-sdk/openai createOpenAI(...).responses("gpt-5.4-mini")
      ↑ 여기서 reasoning.effort: "minimal" 자동 주입
  → POST {baseURL}/responses
  → 게이트웨이 /openai-oauth/v1/responses
  → 업스트림 OpenAI Codex OAuth (현재 minimal 거부)
```

게이트웨이가 정규화 한 단계만 추가하면 전체 체인이 동작합니다.
