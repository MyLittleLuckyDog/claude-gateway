# Runtime Status

이 문서는 현재 `claude-gateway`의 CLI wrap / SDK-style control flow 구현 상태를
소스 기준으로 정리한 운영 메모다. README/USAGE보다 더 구현 지향적인 상태 문서다.

## 현재 포지션

- `Proxy mode` (`/v1/*`)
  - OAuth로 Anthropic Messages API를 직접 호출한다.
  - tool schema/tool_result 라운드트립은 caller가 책임진다.
  - gateway는 로컬 tool executor가 아니다.

- `CLI wrap mode` (`/query`, `/sessions`)
  - 로컬 `claude` CLI를 subprocess로 실행한다.
  - headless runtime adapter 역할을 한다.
  - hook / permission control flow는 이 경로에서 처리된다.

## 구현 완료 범위

### Hook flow

- `initialize` control_request 전송
- `hook_rules`를 callback ID로 등록
- `hook_callback` 수신
- 서버 규칙 우선 평가
  - `approve`
  - `block`
  - `defer`
- `defer` 시 `hook_request` 이벤트를 세션 스트림에 surface
- `POST /sessions/:id/hook_response`로 응답 반영
- timeout 시 자동 처리
  - 기본: `block`
  - 요청별 override: `hook_timeout_secs`, `hook_timeout_action`

### Permission flow

- CLI wrap 기본값으로 `--permission-prompt-tool stdio` 사용
- `can_use_tool` control_request 수신
- `permission_request` 이벤트를 세션 스트림에 surface
- `POST /sessions/:id/permission_response`로 응답 반영
  - `behavior=allow|deny`
  - `updatedInput`
  - `message`

### Stateless query behavior

- `/query`, `/query/stream`도 `initialize`를 전송한다.
- 다만 stateless 경로는 interactive callback endpoint가 없으므로:
  - deferred hook callback은 자동 block
  - tool permission prompt는 자동 deny
- interactive approval이 필요하면 `/sessions`를 써야 한다.

## E2E 확인 결과

다음 항목은 실제 로컬 실행으로 확인했다.

1. `PreToolUse/Bash -> hook_request` surface
2. hook timeout 시 `auto-block` 동작
3. write-capable Bash에서 `permission_request` surface
4. `permission_response` 후 turn 재개 및 tool 실행 완료

확인 중 드러나 같이 수정한 항목:

- 내부 `initialize`용 `control_response`가 사용자 스트림에 새던 문제 수정
- `--host` / `--port` CLI 기본값이 config/env override를 가리던 문제 수정

## 요청별 옵션

아래 옵션은 작업별로 다르게 줄 수 있다.

- `include_hook_events`
- `hook_timeout_secs`
- `hook_timeout_action`
- `permission_prompt_tool`
- `fallback_model`
- `max_budget_usd`
- `include_partial_messages`
- `fork_session`
- `add_dirs`
- `agents`

## 현재 기본값

- `permission_prompt_tool`: `stdio`
- `hook_timeout_secs`: `30`
- `hook_timeout_action`: `block`
- `output_format`:
  - CLI wrap 경로에서는 `stream-json`만 허용

## 남은 개선 항목

아래는 핵심 기능 미구현이라기보다 품질/확장 항목이다.

1. `hook_response` 성공 재개 E2E를 최신 코드 기준으로 다시 한 번 명시 확인
2. 문서 예시에 `permission_request` / `hook_timeout_action=approve` 샘플 추가 보강
3. 운영 로그 필드 정리
4. 레퍼런스 전체 대비 아직 연결되지 않은 옵션 표면 추가 sweep

## 검증 명령

```bash
cargo test -- --nocapture
cargo clippy --tests -- -D warnings
```
