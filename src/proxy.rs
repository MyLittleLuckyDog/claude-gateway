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
    pub fn is_rejected(&self) -> bool {
        self.status.as_deref() == Some("rejected")
            && self.overage_status.as_deref() != Some("allowed")
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

        let reset_info = self.resets_at.map(|ts| {
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
        }).unwrap_or_default();

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
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("Failed to build HTTP client");

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
        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-overage-disabled-reason") {
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

fn build_headers(token: &str, extra_betas: Option<&[String]>) -> Result<HeaderMap, String> {
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
    if let Some(extras) = extra_betas {
        for b in extras {
            if !betas.contains(b) {
                betas.push(b.clone());
            }
        }
    }
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_str(&betas.join(","))
            .map_err(|e| format!("Invalid beta header: {e}"))?,
    );

    headers.insert("x-app", HeaderValue::from_static("cli"));

    Ok(headers)
}

// ── Retry Logic (for 529 only) ─────────────────────────────────────

/// Calculate backoff delay with jitter for retry attempt
fn retry_delay_ms(attempt: u32) -> u64 {
    let base = (BASE_DELAY_MS as f64) * 2.0_f64.powi(attempt as i32 - 1);
    let capped = base.min(MAX_DELAY_MS as f64);
    let jitter = capped * 0.25 * rand_f64();
    (capped + jitter) as u64
}

/// Simple pseudo-random [0, 1) for jitter — avoids pulling in rand crate
fn rand_f64() -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::Instant::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    (hasher.finish() % 1000) as f64 / 1000.0
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

    let headers = match build_headers(&token.access_token, None) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("Quota pre-check skipped (header error): {}", e);
            return;
        }
    };

    let url = format!("{ANTHROPIC_API_BASE}/v1/messages");
    let body = serde_json::json!({
        "model": models::HAIKU_4_5.id,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "q"}]
    });

    tracing::info!("Running quota pre-check...");

    match state.client.post(&url).headers(headers).json(&body).send().await {
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
pub async fn messages_sync(
    state: &Arc<ProxyState>,
    body: serde_json::Value,
    extra_betas: Option<&[String]>,
) -> Result<(serde_json::Value, u16), ProxyError> {
    // Pre-flight: check cached rate limit
    if let Some(msg) = state.pre_flight_check().await {
        return Err(ProxyError::RateLimited(msg));
    }

    let token = auth::get_oauth_token().map_err(ProxyError::Auth)?;
    if !auth::is_token_valid(&token) {
        return Err(ProxyError::Auth(
            "OAuth token expired. Run `claude /login` to refresh.".to_string(),
        ));
    }

    let _permit = state.semaphore.acquire().await
        .map_err(|_| ProxyError::Internal("Semaphore closed".to_string()))?;

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let headers = build_headers(&token.access_token, extra_betas)
        .map_err(ProxyError::Internal)?;

    let url = format!("{ANTHROPIC_API_BASE}/v1/messages");

    // Retry loop for 529 (overloaded) only
    let mut last_status = 0u16;
    for attempt in 1..=(MAX_OVERLOAD_RETRIES + 1) {
        let resp = state.client
            .post(&url)
            .headers(headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProxyError::Upstream(format!("Request failed: {e}")))?;

        last_status = resp.status().as_u16();
        state.update_rate_limits(resp.headers()).await;

        match last_status {
            200 => {
                let resp_body: serde_json::Value = resp.json().await
                    .map_err(|e| ProxyError::Upstream(format!("Failed to parse response: {e}")))?;

                // Track token usage
                if let Some(usage) = resp_body.get("usage") {
                    if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        state.total_input_tokens.fetch_add(input, Ordering::Relaxed);
                    }
                    if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        state.total_output_tokens.fetch_add(output, Ordering::Relaxed);
                    }
                }

                // Include rate limit warning in response if approaching limit
                // (added as gateway metadata, not modifying API response)
                return Ok((resp_body, 200));
            }

            401 => {
                auth::invalidate_cache();
                let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                return Ok((resp_body, 401));
            }

            429 => {
                // NO retry for 429 — subscriber rate limits are time-based.
                // Parse rich error info from headers and return immediately.
                let rl = state.rate_limit.read().await;
                let msg = rl.rejection_message();
                drop(rl);

                let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                tracing::warn!("Rate limited (429): {}", msg);

                return Ok((resp_body, 429));
            }

            529 => {
                // Overloaded — retry with backoff (up to MAX_OVERLOAD_RETRIES)
                if attempt > MAX_OVERLOAD_RETRIES {
                    tracing::warn!("Overloaded (529): max retries ({}) exceeded", MAX_OVERLOAD_RETRIES);
                    let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
                    return Ok((resp_body, 529));
                }

                let delay = parse_retry_after(resp.headers())
                    .unwrap_or_else(|| retry_delay_ms(attempt));

                tracing::info!(
                    "Overloaded (529): retrying in {}ms (attempt {}/{})",
                    delay, attempt, MAX_OVERLOAD_RETRIES
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

    Err(ProxyError::Upstream(format!("Request failed after retries (last status: {last_status})")))
}

/// Streaming Messages API call.
/// Pre-flight check + 401 handling. No retry for streaming (too complex).
pub async fn messages_stream(
    state: &Arc<ProxyState>,
    mut body: serde_json::Value,
    extra_betas: Option<&[String]>,
) -> Result<(reqwest::Response, u16), ProxyError> {
    // Pre-flight: check cached rate limit
    if let Some(msg) = state.pre_flight_check().await {
        return Err(ProxyError::RateLimited(msg));
    }

    let token = auth::get_oauth_token().map_err(ProxyError::Auth)?;
    if !auth::is_token_valid(&token) {
        return Err(ProxyError::Auth(
            "OAuth token expired. Run `claude /login` to refresh.".to_string(),
        ));
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let _permit = state.semaphore.acquire().await
        .map_err(|_| ProxyError::Internal("Semaphore closed".to_string()))?;

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let headers = build_headers(&token.access_token, extra_betas)
        .map_err(ProxyError::Internal)?;

    let url = format!("{ANTHROPIC_API_BASE}/v1/messages");
    let resp = state.client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| ProxyError::Upstream(format!("Request failed: {e}")))?;

    let status = resp.status().as_u16();
    state.update_rate_limits(resp.headers()).await;

    if status == 401 {
        auth::invalidate_cache();
    }

    Ok((resp, status))
}

// ── Error Types ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ProxyError {
    Auth(String),
    RateLimited(String),
    Upstream(String),
    Internal(String),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(msg) => write!(f, "Authentication error: {msg}"),
            Self::RateLimited(msg) => write!(f, "{msg}"),
            Self::Upstream(msg) => write!(f, "Upstream error: {msg}"),
            Self::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl ProxyError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Auth(_) => 401,
            Self::RateLimited(_) => 429,
            Self::Upstream(_) => 502,
            Self::Internal(_) => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth_error",
            Self::RateLimited(_) => "rate_limited",
            Self::Upstream(_) => "upstream_error",
            Self::Internal(_) => "internal_error",
        }
    }
}
