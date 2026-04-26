# Multi-Provider Architecture

이 문서는 `claude-gateway`를 `Claude 전용 게이트웨이`에서
`Top Tier coding/runtime gateway`로 확장하기 위한 설계 메모다.
현재 소스가 기준이며, 먼저 `Codex` 축을 추가하고 이후 `Google` 계열을
수용할 수 있는 구조를 목표로 한다.

## 제품 포지션

사용자 관점의 목표는 단순하다.

- 자동화 파이프라인에서 누구나 쉽게 붙일 수 있어야 한다.
- Anthropic / OpenAI / Google 상위권 모델을 비교하거나 교체하기 쉬워야 한다.
- 가능하면 API에 가까운 표면도 같이 제공해야 한다.

이 기준에서 provider는 두 면으로 나뉜다.

1. `agent/runtime` 면
   - 로컬 CLI 또는 SDK 성격의 headless runtime
   - 세션, 스트리밍, approval, tool 실행 같은 행위를 가짐
2. `API` 면
   - provider의 HTTP API에 가까운 표면
   - tool loop는 caller가 맡을 수 있음

현재 `claude-gateway`는 이미 이 두 면을 갖고 있다.

- `CLI wrap mode`
  - `/query`, `/sessions`
  - 로컬 `claude` subprocess 기반
- `Proxy mode`
  - `/v1/*`
  - Anthropic Messages API 기반

## 확장 원칙

### 1. 기존 Claude 경로는 유지

현재 Claude 경로는 구현량이 많고, 이미 다음 흐름을 포함한다.

- `initialize`
- `hook_request` / `hook_response`
- `permission_request` / `permission_response`
- OAuth refresh / `/v1/*` proxy

따라서 초기에 전면 추상화하지 않는다.

### 2. Codex 축은 별도 추가

`Codex`는 `Claude`와 비슷한 점이 있지만 프로토콜과 승인 흐름이 다르다.
처음부터 공통 trait 하나로 밀어 넣으면 Claude 고유 개념이 공통층을 오염시킬
가능성이 크다.

따라서 1차는 별도 축으로 추가한다.

- 기존:
  - Claude endpoint -> Claude runtime
- 추가:
  - Codex endpoint -> Codex runtime

### 3. 공통층은 얇게만 추출

공통화는 아래처럼 실제로 공통인 영역에만 적용한다.

- session store
- event fanout / stream broadcast
- approval request/response 상태
- 공통 에러 응답 일부
- provider 선택과 lifecycle 관리

반대로 아래는 억지 공통화하지 않는다.

- Claude의 `hook_*`, `control_request`, `control_response`
- Codex 고유 approval/workflow
- provider별 tool semantics

### 4. provider-native 표면을 허용

공통 endpoint만으로 모든 기능을 덮으려 하지 않는다.
필요하면 provider 전용 endpoint를 추가한다.

이 원칙이 있어야 `얇은 호환 계층`이 유지된다.

## Top Tier 범위

현재 목표 provider는 세 개다.

1. `Anthropic`
2. `OpenAI`
3. `Google`

장기 그림은 `3 provider x 2 surface`다.

| Provider | Agent/Runtime | API |
|----------|---------------|-----|
| Anthropic | Claude CLI/SDK 성격 | Anthropic API |
| OpenAI | Codex CLI 성격 | OpenAI API |
| Google | Gemini CLI/agent 성격 | Google API |

## 1차 범위: Codex 축 추가

### 목표

기존 Claude 기능을 깨지 않고, Codex를 별도 축으로 붙인다.

### 초기 endpoint 전략

초기에는 분리형 endpoint가 안전하다.

- 기존 유지
  - `/query`
  - `/sessions`
  - `/v1/*`
- Codex 추가
  - `/codex/query`
  - `/codex/sessions`
  - `/codex/sessions/:id/send`
  - `/codex/sessions/:id/stream`
  - 필요 시 `/codex/sessions/:id/approval_response`

이 단계에서는 provider를 URL로 분리하는 편이 디버깅과 문서화가 쉽다.

### Codex에서 맞출 최소 기능

1. 단발 질의
   - prompt 입력
   - 텍스트 응답
2. 세션 실행
   - 세션 생성
   - 입력 전송
   - 스트리밍 이벤트 수신
3. approval 흐름
   - tool 실행 전 승인 요청 surface
   - allow / deny / 수정 응답

### Codex에서 굳이 1:1로 맞추지 않을 것

- Claude의 `hook_request`
- Claude의 `can_use_tool` 명칭
- Claude control protocol 세부 포맷

공통층에서는 더 일반적인 개념으로 다룬다.

- `approval_request`
- `approval_response`
- `agent_event`
- `session_event`

단, 기존 Claude endpoint는 하위 호환을 위해 현재 명칭을 유지한다.

## 권장 모듈 방향

1차에서는 큰 리팩터링보다 배치 정리가 목적이다.

예상 구조:

- `src/providers/claude/...`
- `src/providers/codex/...`
- `src/core/session/...`
- `src/core/events/...`
- `src/core/approval/...`
- `src/api/claude/...`
- `src/api/codex/...`
- `src/api/common/...`

처음부터 모든 파일을 옮기기보다, 새로 생기는 Codex 축부터 이 방향을 따르고
기존 Claude 코드는 필요한 만큼만 이동한다.

## 단계별 구현 순서

### Phase 0. 기준선 고정

- 현재 Claude 경로 유지
- 현재 문서 유지/보강
- 현재 테스트 유지

### Phase 1. Codex vertical slice

- Codex runtime spawn 실험
- `/codex/query`
- `/codex/sessions`
- `/codex/sessions/:id/send`
- `/codex/sessions/:id/stream`
- 최소 approval flow

이 단계 목표는 “Codex도 gateway 뒤에서 headless로 돈다”를 입증하는 것이다.

### Phase 2. 얇은 공통층 추출

- session 상태 관리
- approval 상태 관리
- 이벤트 fanout
- 공통 에러 응답

이 단계에서는 이미 붙어 있는 Claude/Codex 두 축에서 중복이 보이는 부분만
추출한다.

### Phase 3. 표면 정리

- `/providers/:name/...` 같은 공통 표면 검토
- provider-native endpoint 유지 여부 결정
- 공통 문서 표면 정리

이 단계 전까지는 provider 분리 URL을 유지하는 편이 안전하다.

### Phase 4. Google 축 검토

Google 쪽 runtime/API를 같은 틀로 추가할 수 있는지 검토한다.
이 시점에는 공통층이 충분히 검증된 뒤여야 한다.

## 리팩터링 원칙

아래 방식은 피한다.

- 먼저 거대한 `Backend trait`를 설계
- 기존 Claude 코드를 한 번에 그 trait 뒤로 이동
- 그 위에 Codex를 얹는 방식

이 방식은 추상화가 Claude 중심으로 왜곡될 가능성이 크다.

대신 아래 방식으로 간다.

1. Codex 축을 별도 추가
2. 실제 중복이 생긴 뒤 공통층 추출
3. 공통성이 검증된 다음 정리

## 성공 기준

Codex 1차 추가가 끝났다고 보려면 아래가 충족되어야 한다.

1. Claude 경로가 깨지지 않는다.
2. Codex가 `/codex/query`와 `/codex/sessions`에서 실제로 동작한다.
3. 최소 approval flow가 headless로 동작한다.
4. 사용자는 provider별 내부 차이를 몰라도 자동화에 붙일 수 있다.

## 현재 결정 사항

- 채택: `기존 유지 + Codex 축 추가 + 얇은 공통층 추출`
- 비채택: 선행 대규모 전면 리팩터링
- 허용: 필요 시 provider-native endpoint 추가
- 제품 범위: `Anthropic`, `OpenAI`, `Google`
