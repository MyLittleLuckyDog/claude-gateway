use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::config::AppConfig;
use crate::core::events::record_and_broadcast;
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

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn create_session(
    options: ClaudeAgentOptions,
    store: Arc<SessionStore>,
    config: Arc<AppConfig>,
) -> Result<Arc<Session>, GatewayError> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
    let (event_tx, _) = broadcast::channel::<Arc<Message>>(1024);
    let history = Arc::new(Mutex::new(VecDeque::<Arc<Message>>::new()));
    let state = Arc::new(Mutex::new(SessionState::Initializing));

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
    });

    store.insert(session.clone())?;

    let session_clone = session.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = run_session_loop(session_clone, options, config_clone, stdin_rx).await {
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
) -> Result<(), GatewayError> {
    let session_id_str = session.id.clone();

    // Wait for the first stdin message before spawning CLI
    let first_msg = match stdin_rx.recv().await {
        Some(msg) => msg,
        None => {
            tracing::info!(
                "session {} stdin closed before first message",
                session_id_str
            );
            *session.state.lock().await = SessionState::Dead;
            return Ok(());
        }
    };

    tracing::debug!(
        "session {}: first message received, spawning CLI",
        session_id_str
    );

    let mut transport = CliTransport::new(options.clone(), (*config).clone());
    transport.connect().await?;

    let mut event_rx = transport
        .event_receiver()
        .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))?;

    // Register hook callbacks with the CLI via an initialize control_request
    // BEFORE we relay the user's first message. Without this step the CLI
    // never routes PreToolUse events back to us and hook_rules are dead.
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

    transport.write(&first_msg).await?;

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(Ok(cli_event)) => {
                        // Lock-free activity timestamp
                        session.last_activity_ms.store(now_epoch_ms(), Ordering::Relaxed);

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
                            CliOutputEvent::Result(_) => {
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
                                                *session.state.lock().await = SessionState::Running;
                                            }
                                            AutoResolveOutcome::DeferToClient => {
                                                let deadline = std::time::Instant::now()
                                                    + std::time::Duration::from_secs(30);
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

                        let message = Arc::new(cli_output_to_message(cli_event));
                        broadcast_and_record(&session, message).await;
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
        }
    }

    *session.state.lock().await = SessionState::Dead;
    let _ = transport.close().await;
    Ok(())
}

async fn broadcast_and_record(session: &Session, message: Arc<Message>) {
    record_and_broadcast(&session.history, &session.event_tx, message).await;
}
