use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::config::AppConfig;
use crate::core::events::{record_and_broadcast, Seq};
use crate::core::now_epoch_ms;
use crate::core::stats::Stats;
use crate::error::GatewayError;
use crate::hooks::{self, AutoResolveOutcome};
use crate::messages::cli_control::{ControlRequestOut, ControlRequestPayload};
use crate::messages::cli_output::{CliOutputEvent, SystemSubtype};
use crate::messages::Message;
use crate::options::ClaudeAgentOptions;
use crate::query::cli_output_to_message;
use crate::session::store::SessionStore;
use crate::session::{Session, SessionState};
use crate::transport::cli::CliTransport;
use crate::transport::Transport;

pub async fn create_session(
    options: ClaudeAgentOptions,
    store: Arc<SessionStore>,
    config: Arc<AppConfig>,
    stats: Arc<Mutex<Stats>>,
) -> Result<Arc<Session>, GatewayError> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
    let (event_tx, _) = broadcast::channel::<Seq<Message>>(1024);
    let history = Arc::new(Mutex::new(VecDeque::<Seq<Message>>::new()));
    let state = Arc::new(Mutex::new(SessionState::Initializing));
    let cancel = Arc::new(tokio::sync::Notify::new());

    let session = Arc::new(Session {
        id: session_id.clone(),
        cli_session_id: Arc::new(Mutex::new(None)),
        state: state.clone(),
        created_at: std::time::Instant::now(),
        last_activity_ms: std::sync::atomic::AtomicU64::new(now_epoch_ms()),
        options: options.clone(),
        stdin_tx: stdin_tx.clone(),
        event_tx: event_tx.clone(),
        history: history.clone(),
        hook_timeout_handle: Arc::new(Mutex::new(None)),
        next_seq: std::sync::atomic::AtomicU64::new(0),
        cancel: cancel.clone(),
    });

    store.insert(session.clone())?;

    let session_clone = session.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) =
            run_session_loop(session_clone, options, config_clone, stdin_rx, stats).await
        {
            tracing::error!("session {} loop error: {}", session_id, e);
        }
    });

    Ok(session)
}

async fn run_session_loop(
    session: Arc<Session>,
    options: ClaudeAgentOptions,
    config: Arc<AppConfig>,
    mut stdin_rx: mpsc::Receiver<String>,
    stats: Arc<Mutex<Stats>>,
) -> Result<(), GatewayError> {
    let session_id_str = session.id.clone();
    // The CLI reports total_cost_usd as a running total for this session, so
    // only the increment per turn is charged. Scoped to the loop, so it dies
    // with the session.
    let mut seen_cost = 0.0;

    // Start the CLI now rather than on the first message. Everything the spawn
    // needs is in `options`, which the caller supplied at creation, and the
    // process takes ~400ms to become ready — time an interactive client spends
    // waiting for someone to finish typing. Waiting put that on the critical
    // path of the first answer instead.
    //
    // The cost is that an abandoned session holds a `claude` process until the
    // idle sweep takes it, where before it held only a struct.
    tracing::debug!("session {}: spawning CLI", session_id_str);

    let mut transport = CliTransport::new(options.clone(), (*config).clone());

    // Failing to start is now a creation-time event, so it has to reach the
    // client rather than only the server log. Returning `?` here would skip the
    // `Dead` at the end of this function, and `is_terminal` only recognises
    // `Dead` — the session would sit in `Initializing`, answering nothing,
    // until the idle sweep eventually noticed it.
    let start = transport.connect().await.and_then(|()| {
        transport
            .event_receiver()
            .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))
    });
    let mut event_rx = match start {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!("session {} failed to start CLI: {}", session_id_str, e);
            broadcast_and_record(
                &session,
                Arc::new(Message::Error {
                    message: e.to_string(),
                    code: e.error_code().to_string(),
                }),
            )
            .await;
            *session.state.lock().await = SessionState::Dead;
            return Err(e);
        }
    };

    // Register hook callbacks with the CLI via an initialize control_request
    // before any user message reaches it. Without this step the CLI never
    // routes PreToolUse events back to us and hook_rules are dead. Ordering is
    // guaranteed because nothing reads stdin_rx until the loop below.
    let callback_map: HashMap<String, usize> = match hooks::build_initialize_request(&options) {
        Some((init_payload, cbmap)) => {
            let req_id = format!("init-{}", uuid::Uuid::new_v4());
            let init = ControlRequestOut::new(req_id.clone(), init_payload);
            match serde_json::to_string(&init) {
                Ok(s) => {
                    if let Err(e) = transport.write(&s).await {
                        tracing::warn!("initialize control_request failed: {}", e);
                    } else {
                        tracing::debug!(
                            "session {}: sent initialize request_id={}",
                            session_id_str,
                            req_id
                        );
                    }
                }
                Err(e) => tracing::warn!("initialize serialize failed: {}", e),
            }
            cbmap
        }
        None => HashMap::new(),
    };

    // The first user message is no longer special: the loop's stdin arm relays
    // it exactly as it relays every later one.
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(Ok(cli_event)) => {
                        // Lock-free activity timestamp
                        session.last_activity_ms.store(now_epoch_ms(), Ordering::Relaxed);

                        // Set when a hook_rules entry answers this event's
                        // callback, so the surfaced hook_request can say the
                        // client has nothing to do.
                        let mut hook_auto_resolved = false;

                        // Batch state mutations: single lock acquisition per event
                        match &cli_event {
                            CliOutputEvent::System(sys) if sys.subtype == SystemSubtype::Init => {
                                let mut state = session.state.lock().await;
                                *session.cli_session_id.lock().await = Some(sys.session_id.clone());
                                *state = SessionState::Idle;
                            }
                            CliOutputEvent::Assistant(_) => {
                                *session.state.lock().await = SessionState::Running;
                            }
                            CliOutputEvent::Result(r) => {
                                let cost = crate::core::stats::cost_delta(
                                    &mut seen_cost,
                                    r.total_cost_usd,
                                );
                                {
                                    let mut stats = stats.lock().await;
                                    stats.total_session_turns += 1;
                                    stats.record_turn(r.usage.as_ref(), cost);
                                }
                                *session.state.lock().await = SessionState::Idle;
                            }
                            CliOutputEvent::ControlRequest(ref ctl) => {
                                match &ctl.request {
                                    ControlRequestPayload::HookCallback(hc) => {
                                        let outcome = hooks::try_auto_resolve_hook(
                                            &options,
                                            &ctl.request_id,
                                            hc,
                                        );
                                        match outcome {
                                            AutoResolveOutcome::Respond(json) => {
                                                tracing::info!(
                                                    "hook_callback {} (rule={}) auto-resolved",
                                                    ctl.request_id,
                                                    callback_map
                                                        .get(&hc.callback_id)
                                                        .map(|i| i.to_string())
                                                        .unwrap_or_else(|| "?".to_string())
                                                );
                                                if let Err(e) = transport.write(&json).await {
                                                    tracing::error!(
                                                        "write auto-resolved hook response: {}",
                                                        e
                                                    );
                                                }
                                                hook_auto_resolved = true;
                                                *session.state.lock().await = SessionState::Running;
                                            }
                                            AutoResolveOutcome::DeferToClient => {
                                                // Same source as the watchdog below — see
                                                // hooks::hook_timeout.
                                                let deadline = std::time::Instant::now()
                                                    + hooks::hook_timeout(&options);
                                                *session.state.lock().await =
                                                    SessionState::WaitingForHook {
                                                        request_id: ctl.request_id.clone(),
                                                        deadline,
                                                    };
                                                let handle = hooks::spawn_hook_timeout(
                                                    session.clone(),
                                                    ctl.request_id.clone(),
                                                );
                                                *session.hook_timeout_handle.lock().await =
                                                    Some(handle);
                                            }
                                        }
                                    }
                                    ControlRequestPayload::CanUseTool(_) => {
                                        if let ControlRequestPayload::CanUseTool(req) = &ctl.request {
                                            *session.state.lock().await =
                                                SessionState::WaitingForPermission {
                                                    request_id: ctl.request_id.clone(),
                                                    original_input: req.input.clone(),
                                                };
                                        }
                                    }
                                    ControlRequestPayload::Unknown => {
                                        // Not implemented yet — reject so the CLI doesn't hang.
                                        let err = crate::messages::cli_control::ControlResponseOut::error(
                                            ctl.request_id.clone(),
                                            "subtype not supported by server",
                                        );
                                        if let Ok(s) = serde_json::to_string(&err) {
                                            let _ = transport.write(&s).await;
                                        }
                                    }
                                }
                            }
                            CliOutputEvent::ControlResponse(_) => {
                                // Response to a request WE issued (e.g. initialize).
                                // We don't currently block on these — future work may
                                // track request_id ↔ oneshot for stricter handshakes.
                                continue;
                            }
                            CliOutputEvent::Unknown => continue,
                            _ => {}
                        }

                        let mut message = cli_output_to_message(cli_event);
                        if let Message::HookRequest { auto_resolved, .. } = &mut message {
                            *auto_resolved = hook_auto_resolved;
                        }
                        broadcast_and_record(&session, Arc::new(message)).await;
                    }
                    Some(Err(e)) => {
                        let msg = Arc::new(Message::Error {
                            message: e.to_string(),
                            code: e.error_code().to_string(),
                        });
                        broadcast_and_record(&session, msg).await;
                    }
                    None => {
                        tracing::info!("session {} CLI process exited", session_id_str);
                        break;
                    }
                }
            }
            stdin_msg = stdin_rx.recv() => {
                match stdin_msg {
                    Some(data) => {
                        if let Err(e) = transport.write(&data).await {
                            tracing::error!("session {} stdin write error: {}", session_id_str, e);
                            break;
                        }
                    }
                    None => {
                        tracing::info!("session {} stdin closed", session_id_str);
                        break;
                    }
                }
            }
            _ = session.cancel.notified() => {
                tracing::info!("session {} cancelled, stopping CLI", session_id_str);
                break;
            }
        }
    }

    *session.state.lock().await = SessionState::Dead;
    let _ = transport.close().await;
    Ok(())
}

async fn broadcast_and_record(session: &Session, message: Arc<Message>) {
    record_and_broadcast(
        &session.history,
        &session.event_tx,
        &session.next_seq,
        message,
    )
    .await;
}
