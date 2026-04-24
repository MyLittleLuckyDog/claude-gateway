//! Direct Messages API proxy — forwards requests to api.anthropic.com
//! using the OAuth token from Claude Code's stored credentials.
//!
//! Traffic rules (aligned with CLI behavior):
//! - 429 (rate limit): NO retry for subscribers. Return rich error with reset time.
//! - 529 (overloaded): Retry up to 3 times with exponential backoff.
//! - 401: Invalidate token cache, return auth error.
//! - Pre-flight: Check cached rate limit status before sending request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::auth;
use crate::models;

const OAUTH_BETA_HEADER: &str = models::OAUTH_BETA_HEADER;
const ANTHROPIC_API_BASE: &str = models::ANTHROPIC_API_BASE;
const ANTHROPIC_VERSION: &str = models::ANTHROPIC_VERSION;

/// Max retries for 529 (overloaded) errors — matches CLI's MAX_529_RETRIES
const MAX_OVERLOAD_RETRIES: u32 = 3;

/// Base delay for exponential backoff (ms) — matches CLI's BASE_DELAY_MS
const BASE_DELAY_MS: u64 = 500;

/// Max backoff delay (ms)
const MAX_DELAY_MS: u64 = 32_000;

// ── Rate Limit State ───────────────────────────────────────────────

/// Rate limit state extracted from API response headers.
/// Mirrors the unified rate limit headers from Anthropic API.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitStatus {
    /// Overall status: "allowed" | "allowed_warning" | "rejected"
    pub status: Option<String>,
    /// 5-hour window utilization (0.0 - 1.0)
    pub utilization_5h: Option<f64>,
    /// 7-day window utilization (0.0 - 1.0)
    pub utilization_7d: Option<f64>,
    /// Unix timestamp (seconds) when the active limit resets
    pub resets_at: Option<f64>,
    /// Whether a fallback model is available
    pub fallback_available: Option<bool>,
    /// Which limit is the bottleneck: "five_hour" | "seven_day" | "seven_day_opus" | "seven_day_sonnet"
    pub rate_limit_type: Option<String>,
    /// Overage (extra usage) status
    pub overage_status: Option<String>,
    /// Reason overage is disabled, if applicable
    pub overage_disabled_reason: Option<String>,
}

impl RateLimitStatus {
    /// Is the user currently rate-limited?
    ///
    /// Mirrors CLI logic (claudeAiLimits.ts): user is effectively using overage
    /// when status=rejected AND overage_status is "allowed" or "allowed_warning".
    /// In that case the standard limit is hit but overage credits are available.
    pub fn is_rejected(&self) -> bool {
        if self.status.as_deref() != Some("rejected") {
            return false;
        }
        let using_overage = matches!(
            self.overage_status.as_deref(),
            Some("allowed") | Some("allowed_warning")
        );
        !using_overage
    }

    /// Human-readable summary of rate limit state
    pub fn rejection_message(&self) -> String {
        let limit_name = match self.rate_limit_type.as_deref() {
            Some("five_hour") => "session limit (5h)",
            Some("seven_day") => "weekly limit (7d)",
            Some("seven_day_opus") => "Opus weekly limit",
            Some("seven_day_sonnet") => "Sonnet weekly limit",
            _ => "usage limit",
        };

        let reset_info = self
            .resets_at
            .map(|ts| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let remaining_secs = (ts - now).max(0.0) as u64;
                let hours = remaining_secs / 3600;
                let minutes = (remaining_secs % 3600) / 60;
                if hours > 0 {
                    format!(" · resets in {}h {}m", hours, minutes)
                } else {
                    format!(" · resets in {}m", minutes)
                }
            })
            .unwrap_or_default();

        format!("Rate limited: {limit_name}{reset_info}")
    }

    /// Warning message if approaching limit
    pub fn warning_message(&self) -> Option<String> {
        if self.status.as_deref() != Some("allowed_warning") {
            return None;
        }
        let pct_5h = self.utilization_5h.map(|u| (u * 100.0) as u32);
        let pct_7d = self.utilization_7d.map(|u| (u * 100.0) as u32);

        match (pct_5h, pct_7d) {
            (Some(p5), _) if p5 >= 70 => Some(format!("Warning: {}% of session limit used", p5)),
            (_, Some(p7)) if p7 >= 50 => Some(format!("Warning: {}% of weekly limit used", p7)),
            _ => None,
        }
    }
}

// ── Proxy State ────────────────────────────────────────────────────

/// Proxy-level concurrency, stats, and rate limit tracking
pub struct ProxyState {
    pub client: reqwest::Client,
    /// Serialize concurrent requests (conservative: 1 at a time)
    pub semaphore: Semaphore,
    pub total_requests: AtomicU64,
    pub total_input_tokens: AtomicU64,
    pub total_output_tokens: AtomicU64,
    pub rate_limit: tokio::sync::RwLock<RateLimitStatus>,
}

impl ProxyState {
    pub fn new(max_concurrent: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            semaphore: Semaphore::new(max_concurrent),
            total_requests: AtomicU64::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            rate_limit: tokio::sync::RwLock::new(RateLimitStatus::default()),
        }
    }

    /// Extract all rate limit info from response headers and update state.
    pub async fn update_rate_limits(&self, headers: &reqwest::header::HeaderMap) {
        let mut rl = self.rate_limit.write().await;

        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-status") {
            rl.status = Some(v);
        }
        if let Some(v) = header_f64(headers, "anthropic-ratelimit-unified-5h-utilization") {
            rl.utilization_5h = Some(v);
        }
        if let Some(v) = header_f64(headers, "anthropic-ratelimit-unified-7d-utilization") {
            rl.utilization_7d = Some(v);
        }
        if let Some(v) = header_f64(headers, "anthropic-ratelimit-unified-reset") {
            rl.resets_at = Some(v);
        }
        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-fallback") {
            rl.fallback_available = Some(v == "available");
        }
        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-representative-claim") {
            rl.rate_limit_type = Some(v);
        }
        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-overage-status") {
            rl.overage_status = Some(v);
        }
        if let Some(v) = header_str(
            headers,
            "anthropic-ratelimit-unified-overage-disabled-reason",
        ) {
            rl.overage_disabled_reason = Some(v);
        }
    }

    /// Check if we should block the request before sending (pre-flight).
    /// Returns an error message if rate-limited.
    /// Auto-clears rejected state if the reset time has passed.
    pub async fn pre_flight_check(&self) -> Option<String> {
        {
            let rl = self.rate_limit.read().await;
            if !rl.is_rejected() {
                return None;
            }

            // Check if reset time has passed — if so, clear the rejection
            if let Some(resets_at) = rl.resets_at {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if now >= resets_at {
                    drop(rl);
                    // Clear rejection — next request will get fresh status
                    let mut rl_mut = self.rate_limit.write().await;
                    rl_mut.status = Some("allowed".to_string());
                    tracing::info!("Rate limit reset time passed, allowing requests");
                    return None;
                }
            }

            Some(rl.rejection_message())
        }
    }
}

fn header_str(headers: &reqwest::header::HeaderMap, key: &str) -> Option<String> {
    headers.get(key)?.to_str().ok().map(|s| s.to_string())
}

fn header_f64(headers: &reqwest::header::HeaderMap, key: &str) -> Option<f64> {
    headers.get(key)?.to_str().ok()?.parse().ok()
}

// ── Headers ────────────────────────────────────────────────────────

/// Beta headers the CLI sends for non-Haiku Claude Code queries. Without
/// these, Anthropic's upstream returns a generic `{type:"rate_limit_error",
/// message:"Error"}` with `x-should-retry: true` and no rate-limit headers,
/// which is a malformed-request signal (not a real rate limit). Haiku is
/// exempted — the CLI explicitly excludes these for Haiku in betas.ts:240.
///
/// See `src/utils/betas.ts:getAllModelBetas()` in the reference source:
///   - `claude-code-20250219`       non-Haiku, always
///   - `interleaved-thinking-…`     modelSupportsISP (Sonnet-4/Opus-4)
///   - `context-management-…`       modelSupportsContextManagement
///   - `prompt-caching-scope-…`     firstParty always
const NON_HAIKU_BETAS: &[&str] = &[
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "advanced-tool-use-2025-11-20",
    "effort-2025-11-24",
];

/// True if the given model ID is in the Haiku family. The CLI decides by
/// checking whether the canonical model name contains "haiku".
fn is_haiku_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("haiku")
}

fn build_headers(
    token: &str,
    extra_betas: Option<&[String]>,
    model: Option<&str>,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("Invalid token: {e}"))?,
    );
    headers.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );

    let mut betas = vec![OAUTH_BETA_HEADER.to_string()];

    // Non-Haiku models require the full Claude Code beta set on subscriber
    // plans — see NON_HAIKU_BETAS comment above. Haiku stays minimal.
    let is_haiku = model.map(is_haiku_model).unwrap_or(false);
    if !is_haiku {
        for b in NON_HAIKU_BETAS {
            betas.push((*b).to_string());
        }
    }

    if let Some(extras) = extra_betas {
        for b in extras {
            if !betas.contains(b) {
                betas.push(b.clone());
            }
        }
    }
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_str(&betas.join(",")).map_err(|e| format!("Invalid beta header: {e}"))?,
    );

    // accept — the Anthropic Node SDK sets this on every request.
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );

    // anthropic-dangerous-direct-browser-access — the CLI sends this on
    // every Messages API call. Without it, OAuth-backed subscriber traffic
    // for non-Haiku models is rejected upstream with a generic
    // `{type:"rate_limit_error",message:"Error"}`. Verified against the
    // real CLI via NODE_OPTIONS fetch hook (see DEBUG_SONNET_429.md).
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        HeaderValue::from_static("true"),
    );

    headers.insert("x-app", HeaderValue::from_static("cli"));

    // User-Agent — CLI server-side logging filters on the `claude-cli` prefix.
    // See src/utils/http.ts:getUserAgent() in the reference source.
    let ua = crate::client_identity::user_agent();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_str(&ua).map_err(|e| format!("Invalid user agent: {e}"))?,
    );

    // X-Stainless-* headers — auto-injected by the Anthropic Node SDK.
    // Values mirror what the bundled Claude Code CLI (2.1.x) reports. The CLI
    // does NOT send x-client-request-id or x-stainless-helper-method on the
    // Messages endpoint; we omit them too.
    headers.insert("x-stainless-lang", HeaderValue::from_static("js"));
    headers.insert(
        "x-stainless-package-version",
        HeaderValue::from_static("0.74.0"),
    );
    headers.insert("x-stainless-runtime", HeaderValue::from_static("node"));
    headers.insert(
        "x-stainless-runtime-version",
        HeaderValue::from_static("v23.11.0"),
    );
    headers.insert("x-stainless-arch", HeaderValue::from_static("x64"));
    headers.insert("x-stainless-os", HeaderValue::from_static(stainless_os()));
    headers.insert("x-stainless-retry-count", HeaderValue::from_static("0"));
    headers.insert("x-stainless-timeout", HeaderValue::from_static("600"));

    Ok(headers)
}

/// Best-effort OS classification for the `x-stainless-os` header.
fn stainless_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "MacOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    }
}

// ── Retry Logic (for 529 only) ─────────────────────────────────────

/// Calculate backoff delay with jitter for retry attempt
fn retry_delay_ms(attempt: u32) -> u64 {
    let base = (BASE_DELAY_MS as f64) * 2.0_f64.powi(attempt as i32 - 1);
    let capped = base.min(MAX_DELAY_MS as f64);
    let jitter = capped * 0.25 * rand_f64();
    (capped + jitter) as u64
}

/// Monotonic counter to ensure distinct jitter values on rapid calls
static JITTER_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Simple pseudo-random [0, 1) for jitter — avoids pulling in rand crate.
/// Uses an atomic counter mixed with time and thread ID to produce distinct
/// values even when called in rapid succession on the same thread.
fn rand_f64() -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    JITTER_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    std::time::Instant::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    (hasher.finish() % 10000) as f64 / 10000.0
}

/// Parse retry-after header from response
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let val = headers.get("retry-after")?.to_str().ok()?;
    val.parse::<u64>().ok().map(|s| s * 1000) // seconds → ms
}

// ── Quota Pre-check ────────────────────────────────────────────────

/// Lightweight quota check at startup — sends a minimal request (max_tokens=1)
/// to populate rate limit state from response headers.
/// Matches CLI's `checkQuotaStatus()` behavior.
pub async fn check_quota_at_startup(state: &Arc<ProxyState>) {
    let token = match auth::get_oauth_token() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Quota pre-check skipped (no token): {}", e);
            return;
        }
    };

    let headers = match build_headers(&token.access_token, None, Some(models::HAIKU_4_5.id)) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("Quota pre-check skipped (header error): {}", e);
            return;
        }
    };

    // The CLI uses `/v1/messages?beta=true` (the Node SDK's
    // `client.beta.messages.create()` endpoint). Without `?beta=true` the
    // request is routed to the legacy Messages handler that rejects
    // non-Haiku OAuth traffic with 429 `{message:"Error"}`.
    let url = format!("{ANTHROPIC_API_BASE}/v1/messages?beta=true");
    let mut body = serde_json::json!({
        "model": models::HAIKU_4_5.id,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "q"}]
    });
    let quota_session_id = crate::client_identity::new_session_id();
    crate::client_identity::inject_metadata(&mut body, &quota_session_id);

    tracing::info!("Running quota pre-check...");

    match state
        .client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            state.update_rate_limits(resp.headers()).await;

            let rl = state.rate_limit.read().await;
            tracing::info!(
                "Quota pre-check complete: status={}, 5h={:.0}%, 7d={:.0}%, type={:?}",
                rl.status.as_deref().unwrap_or("unknown"),
                rl.utilization_5h.unwrap_or(0.0) * 100.0,
                rl.utilization_7d.unwrap_or(0.0) * 100.0,
                rl.rate_limit_type,
            );

            if let Some(warning) = rl.warning_message() {
                tracing::warn!("{}", warning);
            }

            if status == 429 {
                tracing::warn!("Currently rate-limited: {}", rl.rejection_message());
            }
        }
        Err(e) => {
            tracing::warn!("Quota pre-check failed: {}", e);
        }
    }
}

// ── API Calls ──────────────────────────────────────────────────────

/// Synchronous (non-streaming) Messages API call.
/// Handles: pre-flight rate limit check, 529 retry, 401 token invalidation.
///
/// `session_id` must be provided by the caller: use a fresh UUID per request
/// for the stateless direct proxy, or a stable per-session UUID for
/// multi-turn proxy sessions. See `client_identity` module docs for why.
pub async fn messages_sync(
    state: &Arc<ProxyState>,
    mut body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(serde_json::Value, u16), ProxyError> {
    // Inject metadata.user_id for rate-limit attribution.
    crate::client_identity::inject_metadata(&mut body, session_id);

    // Non-Haiku models require the billing-header system block or the server
    // rejects with a generic 429 "Error". Haiku accepts requests without it.
    if body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|m| !is_haiku_model(m))
        .unwrap_or(false)
    {
        crate::client_identity::inject_billing_system_block(&mut body);
    }

    // Pre-flight: check cached rate limit
    if let Some(msg) = state.pre_flight_check().await {
        let resets_at = state.rate_limit.read().await.resets_at;
        return Err(ProxyError::RateLimited {
            message: Some(msg),
            resets_at,
        });
    }

    let mut token = auth::get_oauth_token_fresh()
        .await
        .map_err(|e| ProxyError::Auth { message: e })?;

    let _permit = state
        .semaphore
        .acquire()
        .await
        .map_err(|_| ProxyError::Internal("Semaphore closed".to_string()))?;

    // Re-check after acquiring semaphore: a concurrent request may have just
    // been rejected and updated the cached state while we were waiting.
    if let Some(msg) = state.pre_flight_check().await {
        let resets_at = state.rate_limit.read().await.resets_at;
        return Err(ProxyError::RateLimited {
            message: Some(msg),
            resets_at,
        });
    }

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let model_in_body = body.get("model").and_then(|v| v.as_str());
    // The CLI uses `/v1/messages?beta=true` (the Node SDK's
    // `client.beta.messages.create()` endpoint). Without `?beta=true` the
    // request is routed to the legacy Messages handler that rejects
    // non-Haiku OAuth traffic with 429 `{message:"Error"}`.
    let url = format!("{ANTHROPIC_API_BASE}/v1/messages?beta=true");

    // Retry loop for 529 (overloaded) only
    let mut last_status = 0u16;
    for attempt in 1..=(MAX_OVERLOAD_RETRIES + 1) {
        let headers = build_headers(&token.access_token, extra_betas, model_in_body)
            .map_err(ProxyError::Internal)?;
        let resp = state
            .client
            .post(&url)
            .headers(headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProxyError::Transport(format!("{e}")))?;

        last_status = resp.status().as_u16();
        state.update_rate_limits(resp.headers()).await;

        match last_status {
            200 => {
                let resp_body: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| ProxyError::Transport(format!("Failed to parse response: {e}")))?;

                // Track token usage
                if let Some(usage) = resp_body.get("usage") {
                    if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        state.total_input_tokens.fetch_add(input, Ordering::Relaxed);
                    }
                    if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        state
                            .total_output_tokens
                            .fetch_add(output, Ordering::Relaxed);
                    }
                }

                return Ok((resp_body, 200));
            }

            401 => {
                let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                match auth::recover_from_401(&token.access_token).await {
                    Ok(Some(new_token)) if new_token.access_token != token.access_token => {
                        tracing::info!("Recovered OAuth token after 401; retrying request once");
                        token = new_token;
                        continue;
                    }
                    Ok(_) => return Ok((resp_body, 401)),
                    Err(e) => {
                        tracing::warn!("OAuth recovery after 401 failed: {}", e);
                        return Ok((resp_body, 401));
                    }
                }
            }

            429 => {
                // NO retry for 429 — subscriber rate limits are time-based.
                {
                    let mut rl = state.rate_limit.write().await;
                    rl.status = Some("rejected".to_string());
                }

                let rl = state.rate_limit.read().await;
                let msg = rl.rejection_message();
                drop(rl);

                // Dump headers + raw body for debugging
                let header_dump: Vec<String> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<non-utf8>")))
                    .collect();
                let raw_body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "Rate limited (429): {}\n  headers:\n    {}\n  body: {}",
                    msg,
                    header_dump.join("\n    "),
                    raw_body,
                );

                let resp_body: serde_json::Value =
                    serde_json::from_str(&raw_body).unwrap_or_else(|_| {
                        serde_json::json!({
                            "error": {
                                "type": "rate_limit_error",
                                "message": msg,
                            }
                        })
                    });
                return Ok((resp_body, 429));
            }

            529 => {
                // Overloaded — retry with backoff (up to MAX_OVERLOAD_RETRIES)
                if attempt > MAX_OVERLOAD_RETRIES {
                    tracing::warn!(
                        "Overloaded (529): max retries ({}) exceeded",
                        MAX_OVERLOAD_RETRIES
                    );
                    let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                    return Ok((resp_body, 529));
                }

                let delay =
                    parse_retry_after(resp.headers()).unwrap_or_else(|| retry_delay_ms(attempt));

                tracing::info!(
                    "Overloaded (529): retrying in {}ms (attempt {}/{})",
                    delay,
                    attempt,
                    MAX_OVERLOAD_RETRIES
                );

                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                continue;
            }

            _ => {
                // All other errors: pass through as-is
                let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                return Ok((resp_body, last_status));
            }
        }
    }

    Err(ProxyError::Transport(format!(
        "Request failed after retries (last status: {last_status})"
    )))
}

/// Streaming Messages API call.
/// Pre-flight check + 401 handling. No retry for streaming (too complex).
///
/// See `messages_sync` for the `session_id` contract.
pub async fn messages_stream(
    state: &Arc<ProxyState>,
    mut body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(reqwest::Response, u16), ProxyError> {
    // Inject metadata.user_id for rate-limit attribution.
    crate::client_identity::inject_metadata(&mut body, session_id);

    // Non-Haiku billing-header system block (see messages_sync).
    if body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|m| !is_haiku_model(m))
        .unwrap_or(false)
    {
        crate::client_identity::inject_billing_system_block(&mut body);
    }

    // Pre-flight: check cached rate limit
    if let Some(msg) = state.pre_flight_check().await {
        return Err(ProxyError::RateLimited {
            message: Some(msg),
            resets_at: None,
        });
    }

    let mut token = auth::get_oauth_token_fresh()
        .await
        .map_err(|e| ProxyError::Auth { message: e })?;

    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let _permit = state
        .semaphore
        .acquire()
        .await
        .map_err(|_| ProxyError::Internal("Semaphore closed".to_string()))?;

    // Re-check after acquiring semaphore
    if let Some(msg) = state.pre_flight_check().await {
        return Err(ProxyError::RateLimited {
            message: Some(msg),
            resets_at: None,
        });
    }

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let model_in_body = body.get("model").and_then(|v| v.as_str());
    let url = format!("{ANTHROPIC_API_BASE}/v1/messages?beta=true");
    let mut attempted_recovery = false;
    loop {
        let headers = build_headers(&token.access_token, extra_betas, model_in_body)
            .map_err(ProxyError::Internal)?;

        let resp = state
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProxyError::Transport(format!("{e}")))?;

        let status = resp.status().as_u16();
        state.update_rate_limits(resp.headers()).await;

        if status == 401 && !attempted_recovery {
            attempted_recovery = true;
            match auth::recover_from_401(&token.access_token).await {
                Ok(Some(new_token)) if new_token.access_token != token.access_token => {
                    tracing::info!("Recovered OAuth token after 401; retrying stream once");
                    token = new_token;
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("OAuth recovery after 401 failed: {}", e);
                }
            }
        }

        if status == 429 {
            // Mirror CLI extractQuotaStatusFromError(): force rejected on 429
            let mut rl = state.rate_limit.write().await;
            rl.status = Some("rejected".to_string());
        }

        return Ok((resp, status));
    }
}

// ── Error Types ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("Authentication error: {message}")]
    Auth { message: String },

    #[error("{}", message.as_deref().unwrap_or("Rate limited"))]
    RateLimited {
        message: Option<String>,
        resets_at: Option<f64>,
    },

    /// HTTP transport or upstream error (connection, timeout, parse failure)
    #[error("Request failed: {0}")]
    Transport(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl ProxyError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Auth { .. } => 401,
            Self::RateLimited { .. } => 429,
            Self::Transport(_) => 502,
            Self::Internal(_) => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Auth { .. } => "auth_error",
            Self::RateLimited { .. } => "rate_limited",
            Self::Transport(_) => "upstream_error",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}
