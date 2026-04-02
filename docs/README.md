# claude-gateway

> Rust 네이티브 Claude Code CLI 래핑 REST API 게이트웨이

Claude Code CLI(`claude`)를 subprocess로 호출하여 HTTP API로 노출.
Claude Code 구독 토큰 풀 사용. Python 불필요, **2.1MB 단일 바이너리**.

---

## 아키텍처

```
[클라이언트]
    ↓ HTTP POST /chat
[claude-gateway]  (axum + tokio, Rust 단일 바이너리)
    ↓ subprocess (spawn_blocking)
[claude CLI]      (Claude Code, 이미 설치됨)
    ↓ HTTPS
[Anthropic API]   (Claude Code 토큰 풀 과금)
```

## 사전 조건

- Claude Code CLI 설치: `npm install -g @anthropic-ai/claude-code`
- Claude Code 로그인 완료: `claude login`

## 빌드

```bash
cargo build --release
# 출력: target/release/claude-gateway (2.1MB)
```

## 실행

```bash
claude-gateway                                # 기본 (포트 8100, haiku)
claude-gateway --port 8200                    # 포트 변경
claude-gateway --model claude-sonnet-4-6      # 모델 변경
```

## API

### GET /health

```bash
curl http://localhost:8100/health
```

```json
{
  "status": "ok",
  "model": "claude-haiku-4-5",
  "claude_cli": true
}
```

### POST /chat

```bash
curl -X POST http://localhost:8100/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "1+1은?",
    "system_prompt": "한 단어로만 답하세요.",
    "model": "claude-haiku-4-5"
  }'
```

```json
{
  "response": "둘"
}
```

| 필드 | 타입 | 필수 | 설명 |
|------|------|:----:|------|
| `prompt` | string | ✅ | 사용자 프롬프트 |
| `system_prompt` | string | | 시스템 프롬프트 |
| `model` | string | | 모델 오버라이드 (기본: 서버 설정값) |

### POST /reset

```bash
curl -X POST http://localhost:8100/reset
```

```json
{ "status": "ok", "message": "session reset" }
```

## 내부 동작

1. `/chat` 요청 수신
2. `tokio::spawn_blocking`으로 별도 스레드에서 CLI 실행
3. `claude -p "prompt" --output-format stream-json --verbose --model MODEL`
4. stdout에서 JSON 라인 파싱 (`type: "assistant"` → 텍스트 추출)
5. 응답 반환

## 의존성

| 크레이트 | 용도 |
|----------|------|
| axum 0.8 | HTTP 서버 (tokio 팀 개발) |
| tokio | 비동기 런타임 |
| serde / serde_json | JSON 직렬화 |
| tower-http | CORS 미들웨어 |
| tracing | 구조화 로깅 |

## 비용

Claude Code 구독 토큰 풀에서 차감. API 키 별도 과금 아님.

| 모델 | Input | Output |
|------|-------|--------|
| claude-haiku-4-5 | $1.00/1M | $5.00/1M |
| claude-sonnet-4-6 | $3.00/1M | $15.00/1M |
| claude-opus-4-6 | $5.00/1M | $25.00/1M |
