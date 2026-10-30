pub mod admin;
pub mod codex;
pub mod codex_app;
pub mod hooks;
pub mod local_mlx;
pub mod openai;
pub mod openai_oauth;
pub mod proxy;
pub mod proxy_sessions;
pub mod query;
pub mod sessions;

use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::codex::store::CodexSessionStore;
use crate::codex_app::store::CodexAppSessionStore;
use crate::config::AppConfig;
use crate::local_mlx_proxy::LocalMlxProxyState;
use crate::openai_oauth::OpenAiOAuthState;
use crate::openai_proxy::OpenAiProxyState;
use crate::proxy::ProxyState;
use crate::proxy_session::ProxySessionStore;
use crate::session::store::SessionStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub sessions: Arc<SessionStore>,
    pub codex_sessions: Arc<CodexSessionStore>,
    pub codex_app_sessions: Arc<CodexAppSessionStore>,
    pub start_time: std::time::Instant,
    pub stats: Arc<tokio::sync::Mutex<Stats>>,
    pub proxy: Option<Arc<ProxyState>>,
    /// Why the direct API proxy is off, when `proxy` is `None`. Shown in the
    /// 501 the proxy routes answer with.
    pub proxy_unavailable: Option<String>,
    pub proxy_sessions: Option<Arc<ProxySessionStore>>,
    pub local_mlx: Option<Arc<LocalMlxProxyState>>,
    pub openai: Option<Arc<OpenAiProxyState>>,
    pub openai_oauth: Option<Arc<OpenAiOAuthState>>,
}

pub use crate::core::stats::Stats;

// ── Shared error helpers ──────────────────────────────────────────

pub fn error_response(status: u16, code: &str, message: &str) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    from_gateway(
        (
            status_code,
            Json(json!({
                "type": "error",
                "error": {
                    "type": code,
                    "message": message,
                }
            })),
        )
            .into_response(),
    )
}

/// Response header saying who produced an error: `gateway` (this process
/// refused or failed) or `upstream` (Anthropic answered, and the gateway passed
/// it on). Without it a 429 from the gateway's own cache and one from
/// Anthropic look the same.
pub const ERROR_SOURCE_HEADER: &str = "x-error-source";

/// Mark an error response as the gateway's own.
pub fn from_gateway(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(ERROR_SOURCE_HEADER, HeaderValue::from_static("gateway"));
    resp
}

/// Pass an upstream answer on. When it is an error it is marked as upstream's,
/// so it can be told from the gateway's own; the body is not touched.
pub fn relay_upstream(status: StatusCode, body: serde_json::Value) -> Response {
    let mut resp = (status, Json(body)).into_response();
    if status.is_client_error() || status.is_server_error() {
        resp.headers_mut()
            .insert(ERROR_SOURCE_HEADER, HeaderValue::from_static("upstream"));
    }
    resp
}

/// What a client needs to tell one page of a list from the whole list.
///
/// A message list is cut to `limit` entries starting at `offset`; `total` alone
/// does not say that a cut happened. These fields do: `returned` is how many
/// entries came back, `has_more` whether any follow, and `next_offset` where to
/// ask next. They are merged into the response beside `total` and `messages`.
pub fn page_info(total: usize, offset: usize, limit: usize, returned: usize) -> serde_json::Value {
    let end = offset.saturating_add(returned);
    let has_more = end < total;
    json!({
        "offset": offset,
        "limit": limit,
        "returned": returned,
        "has_more": has_more,
        "next_offset": has_more.then_some(end),
    })
}

/// Merge the fields of `page` into the JSON object `body`.
pub fn with_page_info(mut body: serde_json::Value, page: serde_json::Value) -> serde_json::Value {
    if let (Some(target), serde_json::Value::Object(fields)) = (body.as_object_mut(), page) {
        target.extend(fields);
    }
    body
}

/// The 501 every direct-proxy route answers when the proxy is not running, with
/// the reason it is not.
pub fn proxy_disabled_response(state: &AppState) -> Response {
    let reason = state
        .proxy_unavailable
        .as_deref()
        .unwrap_or("it was not started");
    error_response(
        501,
        "proxy_disabled",
        &format!("Direct API proxy is not enabled: {reason}"),
    )
}

/// Largest body [`json_errors`] will turn into an envelope. Anything declared
/// larger -- or not declared -- is passed through untouched: it is never read,
/// so it can never be cut.
const FRAMEWORK_ERROR_BODY_LIMIT: usize = 8 * 1024;

/// The Anthropic error type for an HTTP status, per the gateway protocol table.
fn error_type_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 405 | 415 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// Turn the errors the web framework answers on its own into the gateway's
/// error envelope.
///
/// An unknown path, a wrong method, a body that is not JSON and a missing
/// content type all produced an empty body or one bare line of text. This layer
/// recognises those -- an error status whose body is not JSON and was declared
/// small -- and answers `{"type":"error","error":{"type","message"}}` with a
/// message that says what was wrong (for a 404 or 405, which request and what is
/// allowed). Errors the handlers build themselves are already JSON and are left
/// alone, as is anything it cannot see the whole of.
async fn json_errors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;

    let status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let headers = resp.headers();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/json") || content_type.starts_with("text/event-stream")
    {
        return resp;
    }
    // The size must be known up front: from the header, or from the body itself
    // (an in-memory body knows its length before the transport adds the header).
    let declared: Option<u64> = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or_else(|| axum::body::HttpBody::size_hint(resp.body()).exact());
    if !declared.is_some_and(|n| n <= FRAMEWORK_ERROR_BODY_LIMIT as u64) {
        return resp;
    }

    let (parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, FRAMEWORK_ERROR_BODY_LIMIT).await else {
        // Declared small and not: nothing sensible left to say about its body.
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if !text.is_empty() {
        text
    } else {
        match status.as_u16() {
            404 => format!("No such endpoint: {method} {path}"),
            // The `Allow` header (added outside this layer) lists what is.
            405 => format!(
                "{method} is not allowed on {path}; the Allow header lists the methods that are"
            ),
            _ => status.canonical_reason().unwrap_or("Error").to_string(),
        }
    };

    let mut resp = error_response(status.as_u16(), error_type_for(status), &message);
    if let Some(allow) = parts.headers.get(header::ALLOW) {
        resp.headers_mut().insert(header::ALLOW, allow.clone());
    }
    resp
}

/// Mark a 429 for the caller: when to come back (`Retry-After`, whole seconds,
/// at least 1) if the time is known, and `x-should-retry: false`. The gateway
/// keeps refusing until then, so an SDK retry loop would only hit the same wall.
pub fn with_rate_limit_headers(mut resp: Response, until: Option<f64>) -> Response {
    if let Some(until) = until {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let secs = (until - now).ceil().max(1.0) as u64;
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    }
    no_retry(resp)
}

/// Tell the SDK not to retry this response (`x-should-retry: false`).
pub fn no_retry(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert("x-should-retry", HeaderValue::from_static("false"));
    resp
}

/// Render a [`GatewayError`](crate::error::GatewayError) as an HTTP response.
///
/// Distinct from [`error_response`], which takes a hand-built status/code/message
/// triple for the provider-proxy routes that have no `GatewayError` to hand.
pub fn gateway_error_response(e: &crate::error::GatewayError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    from_gateway((status, Json(crate::error::ErrorResponse::from(e))).into_response())
}

pub fn proxy_error_response(e: crate::proxy::ProxyError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let until = match &e {
        crate::proxy::ProxyError::RateLimited { resets_at, .. } => *resets_at,
        _ => None,
    };
    let is_rate_limit = matches!(e, crate::proxy::ProxyError::RateLimited { .. });
    let resp = (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": e.error_code(),
                "message": e.to_string(),
            }
        })),
    )
        .into_response();
    from_gateway(if is_rate_limit {
        with_rate_limit_headers(resp, until)
    } else {
        resp
    })
}

/// Refuse a request that asks for more of the CLI than the policy allows.
///
/// Returns the response to send, so handlers can `if let Some(r) = ... { return r; }`.
/// The rejection lists every offending field at once rather than one per round
/// trip, and never silently downgrades: a caller told its tools were
/// pre-approved when they were not would act on that.
pub fn reject_disallowed_claude_options(
    state: &AppState,
    options: &crate::options::ClaudeAgentOptions,
) -> Option<Response> {
    state
        .config
        .request_options
        .to_policy()
        .check_claude(options)
        .err()
        .map(|e| {
            gateway_error_response(&crate::error::GatewayError::OptionNotPermitted(
                e.to_string(),
            ))
        })
}

/// Codex counterpart of [`reject_disallowed_claude_options`].
pub fn reject_disallowed_codex_options(
    state: &AppState,
    options: &crate::codex::options::CodexOptions,
) -> Option<Response> {
    state
        .config
        .request_options
        .to_policy()
        .check_codex(options)
        .err()
        .map(|e| {
            gateway_error_response(&crate::error::GatewayError::OptionNotPermitted(
                e.to_string(),
            ))
        })
}

/// Request headers a browser client needs to send.
///
/// Spelled out rather than `Any` because `Access-Control-Allow-Headers: *` is
/// rejected by browsers on credentialed requests, and this list has to work
/// with or without credentials.
///
/// `last-event-id` is here for the fetch-streaming clients that resume a
/// session stream by hand — see `core::events`. (`EventSource` sends it too,
/// but browsers do not preflight it there.)
const ALLOWED_REQUEST_HEADERS: [HeaderName; 4] = [
    header::ACCEPT,
    header::AUTHORIZATION,
    header::CONTENT_TYPE,
    HeaderName::from_static("last-event-id"),
];

const ALLOWED_METHODS: [Method; 4] = [Method::GET, Method::POST, Method::DELETE, Method::OPTIONS];

/// Cache preflight results for an hour. The policy is fixed at startup, so
/// re-asking on every request buys nothing.
const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(3600);

fn build_cors_layer(config: &crate::config::ServerConfig) -> CorsLayer {
    let base = CorsLayer::new()
        .allow_methods(ALLOWED_METHODS)
        .allow_headers(ALLOWED_REQUEST_HEADERS)
        .max_age(PREFLIGHT_MAX_AGE);

    if config.cors_allow_any_origin {
        if config.cors_allow_credentials {
            // Browsers reject `Access-Control-Allow-Origin: *` together with
            // credentials, and tower-http panics when asked to emit both.
            // Dropping credentials keeps the server running with the weaker
            // of the two requests rather than the more dangerous one.
            tracing::error!(
                "cors_allow_any_origin and cors_allow_credentials cannot both be set; \
                 serving every origin WITHOUT credentials. List the origins you trust \
                 in cors_origins to use cookie auth."
            );
        }
        tracing::warn!(
            "CORS: every origin allowed. The gateway has no authentication of its own — \
             any page the user visits can drive it."
        );
        return base.allow_origin(Any);
    }

    // Report unusable entries loudly. Silently narrowing (or widening) the
    // policy because of a typo is how a gateway ends up open by accident.
    let mut allowed: Vec<HeaderValue> = Vec::new();
    for origin in &config.cors_origins {
        let origin = origin.trim();
        if origin.is_empty() {
            continue;
        }
        // An Origin header is scheme + host + optional port and never carries a
        // path, so an entry with one can never match. This is the usual way a
        // pasted site URL fails.
        if origin
            .split("//")
            .nth(1)
            .is_some_and(|rest| rest.contains('/'))
        {
            tracing::warn!(
                "CORS: origin {:?} contains a path; browsers send only scheme://host[:port], \
                 so this entry will never match",
                origin
            );
        }
        match HeaderValue::from_str(origin) {
            Ok(val) => allowed.push(val),
            Err(e) => tracing::error!("CORS: ignoring unusable origin {:?}: {}", origin, e),
        }
    }

    if allowed.is_empty() {
        tracing::warn!(
            "CORS: no usable origins configured — cross-origin browser callers will be \
             refused. Same-origin and non-browser clients are unaffected."
        );
    } else {
        tracing::info!(
            "CORS: {} origin(s) allowed, credentials {}",
            allowed.len(),
            if config.cors_allow_credentials {
                "on"
            } else {
                "off"
            }
        );
    }

    base.allow_credentials(config.cors_allow_credentials)
        .allow_origin(AllowOrigin::predicate(move |value, _| {
            origin_is_allowed(value, &allowed)
        }))
}

/// An origin matches an allow-list entry exactly, or as that entry plus a port.
///
/// The port separator is what keeps the prefix match honest: `http://localhost`
/// admits `http://localhost:3000` but not `http://localhost.evil.example`.
fn origin_is_allowed(origin: &HeaderValue, allowed: &[HeaderValue]) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    allowed.iter().any(|entry| {
        let Ok(entry) = entry.to_str() else {
            return false;
        };
        origin == entry
            || (origin.len() > entry.len()
                && origin.starts_with(entry)
                && origin.as_bytes()[entry.len()] == b':')
    })
}

pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(&state.config.server);
    Router::new()
        .merge(admin::routes())
        .merge(codex::routes())
        .merge(codex_app::routes())
        .merge(openai::routes())
        .merge(openai_oauth::routes())
        .merge(query::routes())
        .merge(sessions::routes())
        .merge(hooks::routes())
        .merge(local_mlx::routes())
        .merge(proxy::routes())
        .merge(proxy_sessions::routes())
        .layer(axum::middleware::from_fn(json_errors))
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn allowed(entries: &[&str]) -> Vec<HeaderValue> {
        entries
            .iter()
            .map(|e| HeaderValue::from_str(e).unwrap())
            .collect()
    }

    fn matches(origin: &str, entries: &[&str]) -> bool {
        origin_is_allowed(&HeaderValue::from_str(origin).unwrap(), &allowed(entries))
    }

    #[test]
    fn an_exact_origin_matches() {
        assert!(matches(
            "https://app.example.com",
            &["https://app.example.com"]
        ));
    }

    /// A bare host entry is meant to cover whatever port the dev server picked.
    #[test]
    fn a_host_entry_covers_any_port_on_it() {
        assert!(matches("http://localhost:5173", &["http://localhost"]));
        assert!(matches("http://localhost:3000", &["http://localhost"]));
    }

    /// The port separator is what stops the prefix match from admitting a
    /// look-alike domain that merely starts with an allowed one.
    #[test]
    fn a_look_alike_domain_is_refused() {
        assert!(!matches(
            "http://localhost.evil.example",
            &["http://localhost"]
        ));
        assert!(!matches(
            "https://app.example.com.evil.test",
            &["https://app.example.com"]
        ));
    }

    /// The scheme is part of an origin — allowing http does not allow https.
    #[test]
    fn the_scheme_must_match() {
        assert!(!matches(
            "https://app.example.com",
            &["http://app.example.com"]
        ));
    }

    #[test]
    fn an_empty_allow_list_matches_nothing() {
        assert!(!matches("https://app.example.com", &[]));
        assert!(!matches("null", &[]));
    }

    fn server_config(origins: &[&str], any: bool, credentials: bool) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8765,
            max_sessions: 10,
            cors_origins: origins.iter().map(|s| s.to_string()).collect(),
            cors_allow_any_origin: any,
            cors_allow_credentials: credentials,
        }
    }

    /// Building the layer must not panic on the one combination tower-http
    /// refuses to emit; the conflict is resolved by dropping credentials.
    #[test]
    fn allow_any_origin_with_credentials_does_not_panic() {
        let _ = build_cors_layer(&server_config(&[], true, true));
    }

    /// Clearing the origin list used to mean "allow everything". It now means
    /// what it reads like.
    /// Blank entries are noise, not an allow-anything signal.
    #[test]
    fn blank_entries_are_dropped() {
        let cfg = server_config(&["", "  ", "https://app.example.com"], false, false);
        let _ = build_cors_layer(&cfg);
        assert!(!matches(
            "https://evil.example",
            &["https://app.example.com"]
        ));
        assert!(matches(
            "https://app.example.com",
            &["https://app.example.com"]
        ));
    }

    /// A pasted site URL keeps its path; an Origin header never has one.
    #[test]
    fn an_entry_with_a_path_cannot_match() {
        assert!(!matches(
            "https://app.example.com",
            &["https://app.example.com/chat"]
        ));
    }

    #[test]
    fn an_empty_origin_list_builds_a_closed_policy() {
        let _ = build_cors_layer(&server_config(&[], false, false));
        assert!(!matches("https://anything.example", &[]));
    }

    #[test]
    fn a_cut_list_says_so() {
        // First 50 of 120.
        let p = page_info(120, 0, 50, 50);
        assert_eq!(p["has_more"], true);
        assert_eq!(p["next_offset"], 50);
        assert_eq!(p["returned"], 50);
        assert_eq!(p["limit"], 50);
    }

    #[test]
    fn the_last_page_has_nothing_after_it() {
        let p = page_info(120, 100, 50, 20);
        assert_eq!(p["has_more"], false);
        assert_eq!(p["next_offset"], serde_json::Value::Null);
        // A list that fits exactly is not cut either.
        assert_eq!(page_info(50, 0, 50, 50)["has_more"], false);
    }

    #[test]
    fn a_page_past_the_end_is_empty_and_not_cut() {
        let p = page_info(10, 500, 50, 0);
        assert_eq!(p["returned"], 0);
        assert_eq!(p["has_more"], false);
        // limit 0 returns nothing from a non-empty list: that is a cut.
        let z = page_info(10, 0, 0, 0);
        assert_eq!(z["has_more"], true);
        assert_eq!(z["next_offset"], 0);
    }

    #[test]
    fn page_fields_are_merged_beside_the_existing_ones() {
        let body = with_page_info(
            json!({"session_id": "s", "total": 3, "messages": [1, 2]}),
            page_info(3, 0, 2, 2),
        );
        assert_eq!(body["total"], 3);
        assert_eq!(body["messages"], json!([1, 2]));
        assert_eq!(body["has_more"], true);
        assert_eq!(body["next_offset"], 2);
    }
}
