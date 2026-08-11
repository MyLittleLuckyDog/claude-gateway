# Phase 2 — 얇은 공통층 추출

[MULTI_PROVIDER_ARCHITECTURE.md](MULTI_PROVIDER_ARCHITECTURE.md) 의 Phase 2 를 실제
소스 중복량으로 검증하고, 그중 안전한 범위를 `src/core/` 로 뽑아낸 기록이다.

문서의 원칙은 "**실제 중복이 생긴 뒤** 공통층 추출"이다. 아래 측정은 그 조건이
이미 충족됐음을 보여준다.

## 1. 측정 — 무엇이 실제로 중복인가

### 세션 store 3종은 사실상 같은 파일

타입명을 정규화하고 diff 를 뜬 결과:

| 비교 | 다른 줄 |
|---|---|
| `codex/store.rs` ↔ `codex_app/store.rs` | 63줄 중 **2줄** (import 경로, 로그 문자열) |
| `codex/store.rs` ↔ `session/store.rs` | 63줄 중 ~7줄 (위 + `count()` 유무) |

`insert` / `get` / `list` / `remove` / `run_cleanup`, `DashMap` + `max_sessions` +
`op_lock` 조합, epoch-ms 계산까지 동일하다.

### 그 밖의 복사본 (추출 전 기준)

| 패턴 | 복사본 | 위치 |
|---|---|---|
| `fn error_response(e: &GatewayError)` — 글자까지 동일 | 4 | `api/{sessions,codex,codex_app,hooks}.rs` |
| history push → 500 트림 → `event_tx.send` | 5 | `client.rs` · `hooks/mod.rs` · `codex/mod.rs`(2) · `codex_app/mod.rs` |
| SSE "히스토리 재생 → broadcast 추종 → Lagged 처리" | 3 | `api/{sessions,codex,codex_app}.rs` |
| `MAX_*_HISTORY_SIZE = 500` | 3 | `session/mod.rs` · `codex/session.rs` · `codex_app/session.rs` |

세션 타입 3종의 공통 필드도 명확하다 — `id` / `state: Arc<Mutex<_>>` /
`created_at: Instant` / `last_activity_ms: AtomicU64` /
`event_tx: broadcast::Sender<Arc<E>>` / `history: Arc<Mutex<VecDeque<Arc<E>>>>`.
상태 enum 3개 모두 `Idle` / `Running` / `Dead` + provider별 대기 variant 구조다.

## 2. 공통층에 넣지 않기로 한 것

문서의 Phase 2 목록과 의도적으로 다른 두 가지다.

### Proxy 세션 — 종류가 다르다

문서는 "session 상태 관리"를 통째로 공통이라 했지만, `ProxySession` 은 장수
subprocess 세션이 아니라 **stateless API 위에 얹은 대화 버퍼**다.

| | Claude / Codex / Codex-app | ProxySession |
|---|---|---|
| 저장소 | `DashMap` | `RwLock<HashMap<_, Arc<Mutex<_>>>>` |
| 히스토리 | `VecDeque<Arc<Event>>` + broadcast | `Vec<Value>` 대화 버퍼, broadcast 없음 |
| 상태 | 상태 머신 | 없음 |
| 시간 | epoch **millis** | epoch **secs** |
| 에러 | `GatewayError` | `Result<_, String>` |

억지로 합치면 공통층이 오염된다. 문서 확장 원칙 4번(provider-native 표면 허용)을
여기 적용한다.

### approval — 표면만 같고 기전이 다르다

- **Claude**: `SessionState::WaitingForHook { request_id, deadline }` + 별도 타임아웃
  task(`hook_timeout_handle`) + `ControlResponseOut` 을 stdin 으로. 타임아웃 시
  자동 block/approve.
- **Codex app**: `pending_requests: HashMap<String, oneshot::Sender<Value>>` +
  `pending_approval` + JSON-RPC id 매칭. 타임아웃 개념 없음.

공통인 건 "승인 요청이 세션 스트림에 뜨고 POST 로 답한다"는 **와이어 포맷**뿐이다.
지금 trait 로 묶으면 Claude 의 deadline/timeout 개념이 공통층에 샌다. 문서의 비-공통
목록(`hook_*`, `control_request/response`)과 같은 판단이다.

## 3. 실제로 뽑은 것

```
src/core/
  mod.rs       공통층 범위 선언 + now_epoch_ms()
  events.rs    MAX_HISTORY + record_and_broadcast() + sse_replay_then_follow()
  session.rs   ManagedSession trait + SessionStore<S>
```

`api/mod.rs` 에는 `gateway_error_response(&GatewayError)` 를 추가했다. 같은 모듈의
기존 `error_response(status, code, message)` 와 이름이 겹치지 않도록 별도 이름을
썼다 — 후자는 `GatewayError` 가 없는 provider 프록시 라우트(`proxy`, `openai`,
`local_mlx`)가 쓴다.

세 store 파일은 각각 **27줄의 얇은 어댑터**만 남았다 — `SessionStore<S>` 타입 별칭과
자기 세션 타입에 대한 `ManagedSession` 구현이 전부다.

```rust
pub type CodexSessionStore = crate::core::session::SessionStore<CodexSession>;

#[async_trait]
impl ManagedSession for CodexSession {
    fn id(&self) -> &str { &self.id }
    fn last_activity_ms(&self) -> u64 { /* … */ }
    fn kind() -> &'static str { "Codex" }
    async fn is_terminal(&self) -> bool {
        *self.state.lock().await == CodexSessionState::Dead
    }
}
```

**상태 enum 은 trait 밖에 남는다.** provider 는 "종료 상태인가"라는 boolean 만
공통층에 노출하므로, `WaitingForHook` / `WaitingForApproval` 같은 대기 variant 가
공통층에 새지 않는다.

측정 결과: 1·2단계 **13개 파일 +93 / −225**, 3단계 **7개 파일 +61 / −250**
(+ `core/session.rs` 275줄, 그중 절반이 테스트).

### 보존한 quirk 하나

`insert` 의 용량 검사는 점유 수만 본다. 따라서 store 가 가득 찼을 때는 **이미 있는
id 를 재삽입해도 거부**된다. 세션 id 는 매번 새로 만드는 UUID 라 실제로는 도달하지
않는 경로이므로, 순수 리팩터의 원칙대로 원래 동작을 그대로 두고 테스트로 못박았다
(`at_capacity_even_a_same_id_insert_is_refused`).

## 4. 남은 단계

| 단계 | 내용 | 상태 |
|---|---|---|
| 1 | 공통 에러 응답 (4→1) | ✅ 완료 |
| 2 | `core::events` — record/broadcast + SSE (8→2) | ✅ 완료 |
| 3 | `SessionStore<S>` + `ManagedSession` trait (3→1) | ✅ 완료 |
| 4 | cleanup 의 await-holding-guard 수정 | ✅ 3에 포함 |

Phase 2 는 여기서 마친다. 남은 후보는 아래 "다음에 볼 것" 참고.

### 4단계 — 고친 위험

3단계 이전, 세 store 의 `run_cleanup` 은 **DashMap 이터레이터를 들고 await** 했다:

```rust
for entry in self.sessions.iter() {          // shard read guard 보유
    let state = session.state.lock().await;  // ← 여기서 await
```

`DashMap::iter()` 는 샤드 read 가드를 잡고, dashmap 의 샤드 락은 **블로킹 sync
RwLock** 이다. 다른 task 가 같은 샤드에 `insert`/`remove` 하려 하면 워커 스레드가
통째로 블록되고, 그 사이 cleanup task 는 `state` 뮤텍스를 기다린다. 확정 데드락으로
재현시키지는 못했으므로 잠재 위험으로 분류하지만, 워커 수가 적을 때 스톨할 수 있다.
clippy 의 `await_holding_lock` 은 DashMap 가드를 잡아내지 못해 조용히 통과한다.

통합 store 는 후보를 **먼저 스냅샷**해 이터레이터를 놓은 뒤 상태를 검사한다:

```rust
let candidates: Vec<Arc<S>> = self.sessions.iter().map(|r| r.value().clone()).collect();
for session in candidates {
    if expired || session.is_terminal().await { /* … */ }
}
```

회귀 방지로 계약 테스트에
`cleanup_runs_concurrently_with_inserts_and_removes` 를 뒀다 — cleanup 과 삽입/삭제를
동시에 돌리고 10초 타임아웃을 건다.

## 검증

| 항목 | 결과 |
|---|---|
| store 계약 테스트 | 39개 — 3단계 **전후 모두 변경 없이 통과**(동작 보존의 증거) |
| 전체 테스트 | 159 passed |
| `cargo clippy --all-targets -- -D warnings` | 경고 0 |
| 실사격 | `max_sessions=2` 초과 시 429 · 라이브 턴 `system`→`assistant`→`result` · DELETE 후 재생성 201 |

## 다음에 볼 것

Phase 2 범위는 아니지만 측정 중 눈에 띈 것:

- `api/{sessions,codex,codex_app}.rs` 의 CRUD 핸들러(생성/목록/삭제/messages 페이지네이션)가
  아직 provider별로 거의 같은 모양이다. 다만 응답 JSON 필드가 provider마다 달라
  (`thread_id`, `turn_id`, `include_system`) 공통화 이득이 store만큼 크지 않다.
- `codex/store.rs` 와 `codex_app/store.rs` 의 `CodexOptions` 공유는 이미 되어 있다.

## 5. 하지 말 것

문서의 **Phase 3(공통 `/providers/:name/...` 표면)은 아직 하지 않는다.** provider
분리 URL 이 여전히 디버깅·문서화에 유리하고, 공통층이 검증되기 전이다.

## 변경 이력

- **2026-08-11** — 1·2단계 적용, 3·4단계 설계 확정
