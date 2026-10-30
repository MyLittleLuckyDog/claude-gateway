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

/// How long a rejection that carried no reset time (no `unified-reset` and no
/// `retry-after`) is trusted before the next request is let through to probe
/// upstream again. Without an expiry such a rejection could never be cleared:
/// the pre-flight check refuses locally, so no response ever arrives to
/// refresh the cached state.
pub const REJECTION_FALLBACK_TTL_SECS: f64 = 60.0;

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

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
    /// Unix timestamp (seconds) at which this family was last seen rejected.
    /// Lets `pre_flight_check` expire a rejection that arrived without any
    /// reset information instead of holding it forever.
    pub rejected_at: Option<f64>,
    /// Whether a fallback model is available
    pub fallback_available: Option<bool>,
    /// Which limit is the bottleneck: "five_hour" | "seven_day" | "seven_day_opus" | "seven_day_sonnet"
    pub rate_limit_type: Option<String>,
    /// Overage (extra usage) status
    pub overage_status: Option<String>,
    /// Reason overage is disabled, if applicable
    pub overage_disabled_reason: Option<String>,
}

/// When a rejected family may be tried again, and where that time comes from:
/// the reset time upstream reported, or `REJECTION_FALLBACK_TTL_SECS` after the
/// rejection was recorded when upstream gave none. `None` when there is no
/// timestamp at all.
fn rejection_expiry(rl: &RateLimitStatus) -> Option<(f64, &'static str)> {
    match (rl.resets_at, rl.rejected_at) {
        (Some(reset), _) => Some((reset, "reset time")),
        (None, Some(at)) => Some((at + REJECTION_FALLBACK_TTL_SECS, "fallback ttl")),
        (None, None) => None,
    }
}

/// A request the gateway refused itself, without contacting upstream, because
/// the family is cached as rejected.
#[derive(Debug, Clone)]
pub struct PreflightBlock {
    pub message: String,
    /// Unix seconds at which requests are let through again.
    pub until: f64,
}

impl PreflightBlock {
    pub fn into_error(self) -> ProxyError {
        ProxyError::RateLimited {
            message: Some(self.message),
            resets_at: Some(self.until),
        }
    }
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
    /// Upstream API origin. `ANTHROPIC_API_BASE` in production; tests point it
    /// at a local stub so the outgoing request can be inspected without
    /// calling the real API.
    pub base_url: String,
    /// Serialize concurrent requests (conservative: 1 at a time)
    pub semaphore: Semaphore,
    pub total_requests: AtomicU64,
    pub total_input_tokens: AtomicU64,
    pub total_output_tokens: AtomicU64,
    /// Keyed by model family (Haiku/Sonnet/Opus/Fable/Unknown) — each has its
    /// own quota pool on the real API (confirmed live 2026-10-24: Fable 5.1
    /// at 0% utilization was rejected by Sonnet's ~90%-utilized cached
    /// status back when this was one shared RateLimitStatus for every
    /// model). Never let one family's cached state gate a different family's
    /// request.
    pub rate_limit:
        tokio::sync::RwLock<std::collections::HashMap<models::ModelFamily, RateLimitStatus>>,
}

impl ProxyState {
    pub fn new(max_concurrent: usize) -> Self {
        Self::with_base_url(max_concurrent, ANTHROPIC_API_BASE)
    }

    pub fn with_base_url(max_concurrent: usize, base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            semaphore: Semaphore::new(max_concurrent),
            total_requests: AtomicU64::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            rate_limit: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Extract all rate limit info from response headers and update the
    /// given family's cached status. `family` must be the family of the
    /// model that produced these headers — see `models::family_of`.
    pub async fn update_rate_limits(
        &self,
        headers: &reqwest::header::HeaderMap,
        family: models::ModelFamily,
    ) {
        let mut map = self.rate_limit.write().await;
        let rl = map.entry(family).or_default();

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

        // Fable/Mythos carry a SEPARATE 7-day sub-allocation alongside the
        // generic one — confirmed live 2026-10-24: a Fable response has both
        // `anthropic-ratelimit-unified-7d-*` (the generic, blended figure —
        // 92% utilized, shared with Sonnet/Opus/Haiku) AND
        // `anthropic-ratelimit-unified-7d_oi-*` (0% utilized, present ONLY
        // on Fable responses — Sonnet/Opus/Haiku never send it). The generic
        // field doesn't reflect Fable's own real headroom at all; prefer the
        // `7d_oi` variant when present so Fable's cached status (and thus
        // pre_flight_check for the Fable family) is based on Fable's actual
        // constraint, not the unrelated general pool's.
        if let Some(v) = header_f64(headers, "anthropic-ratelimit-unified-7d_oi-utilization") {
            rl.utilization_7d = Some(v);
        }
        if let Some(v) = header_f64(headers, "anthropic-ratelimit-unified-7d_oi-reset") {
            rl.resets_at = Some(v);
        }
        if let Some(v) = header_str(headers, "anthropic-ratelimit-unified-7d_oi-status") {
            rl.status = Some(v);
        }

        // Keep `rejected_at` in step with the final status: stamp it when a
        // rejection first appears, drop it once the family is no longer rejected.
        if rl.status.as_deref() == Some("rejected") {
            rl.rejected_at.get_or_insert_with(now_secs);
        } else {
            rl.rejected_at = None;
        }
    }

    /// Record an upstream 429 for `family`.
    ///
    /// The status is forced to `rejected` regardless of the headers (as the CLI
    /// does), but the expiry is derived from *this* response: its reset header,
    /// else its `retry-after`, else none (→ `REJECTION_FALLBACK_TTL_SECS`). A
    /// reset left over from an earlier response describes a different window
    /// and must not stretch a bare 429 into hours of local rejection.
    pub async fn mark_rejected(
        &self,
        family: models::ModelFamily,
        headers: &reqwest::header::HeaderMap,
    ) {
        let now = now_secs();
        let mut map = self.rate_limit.write().await;
        let rl = map.entry(family).or_default();
        rl.status = Some("rejected".to_string());
        rl.rejected_at = Some(now);

        let has_reset = [
            "anthropic-ratelimit-unified-reset",
            "anthropic-ratelimit-unified-7d_oi-reset",
        ]
        .iter()
        .any(|k| header_f64(headers, k).is_some());
        if !has_reset {
            rl.resets_at = parse_retry_after(headers).map(|ms| now + ms as f64 / 1000.0);
        }
    }

    /// Unix seconds at which `family` stops being refused, if it currently is.
    /// Lets callers tell a client when to come back.
    pub async fn rejection_expires_at(&self, family: models::ModelFamily) -> Option<f64> {
        let map = self.rate_limit.read().await;
        let rl = map.get(&family)?;
        if !rl.is_rejected() {
            return None;
        }
        rejection_expiry(rl).map(|(until, _)| until)
    }

    /// Check if we should block a request for `family` before sending
    /// (pre-flight). Returns an error message if that family is rate-limited.
    /// A rejection expires at its reset time, or — when upstream gave none —
    /// `REJECTION_FALLBACK_TTL_SECS` after it was recorded; once expired the
    /// next request is let through to refresh the real status. A family with
    /// no cached status yet (nothing seen for it) is never rejected.
    pub async fn pre_flight_check(&self, family: models::ModelFamily) -> Option<PreflightBlock> {
        let now = now_secs();
        {
            let map = self.rate_limit.read().await;
            let rl = map.get(&family)?;
            if !rl.is_rejected() {
                return None;
            }

            let Some((until, source)) = rejection_expiry(rl) else {
                // Rejected with no timestamp at all: nothing to expire it by.
                // Fail open — upstream is the authority, not this cache.
                tracing::warn!(
                    "[pre-flight] {:?} cached as rejected with no expiry; letting the request through",
                    family
                );
                return None;
            };

            if now < until {
                let remaining = (until - now).ceil() as u64;
                tracing::warn!(
                    "[pre-flight] refusing locally, upstream not contacted: family={:?} expiry={} retry_in={}s",
                    family,
                    source,
                    remaining
                );
                return Some(PreflightBlock {
                    message: format!(
                        "{} (cached rejection, request not sent upstream; gateway retries in {}s)",
                        rl.rejection_message(),
                        remaining
                    ),
                    until,
                });
            }
        }

        // Expired: clear it so the next request refreshes the real status.
        let mut map = self.rate_limit.write().await;
        if let Some(rl) = map.get_mut(&family) {
            if rl.is_rejected() {
                rl.status = Some("allowed".to_string());
                rl.rejected_at = None;
                tracing::info!(
                    "Cached rate-limit rejection for {:?} expired, allowing requests",
                    family
                );
            }
        }
        None
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
/// True for a Haiku model confirmed to need the REDUCED treatment (no
/// billing system block, no NON_HAIKU_BETAS).
///
/// Confirmed via live testing 2026-10-24: Haiku 5.5 needs the SAME treatment
/// as Sonnet/Opus — omitting the billing block produces the exact generic
/// `{type:"rate_limit_error",message:"Error"}` a non-Haiku model gets
/// without it (see client_identity::billing_system_block's doc comment);
/// adding just the block (no extra betas needed) fixed it. Only Haiku 4.5 is
/// confirmed exempt, so exclude 5.5 explicitly rather than matching any
/// model whose name merely contains "haiku" — a future Haiku should default
/// to the safe (full) treatment until proven otherwise, the same way 5.5
/// turned out to need it despite the name match.
fn is_haiku_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("haiku") && !lower.contains("5-5")
}

fn build_headers(
    token: &str,
    extra_betas: Option<&[String]>,
    model: Option<&str>,
    session_id: Option<&str>,
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

    // The CLI stamps every request with its session id (2.1.314 bundle: the
    // default headers are x-app, User-Agent and this one). It is the same id
    // that goes into metadata.user_id, so the two always agree. A caller-chosen
    // id that is not a valid header value is left off rather than failing the
    // request.
    if let Some(value) = session_id.and_then(|sid| HeaderValue::from_str(sid).ok()) {
        headers.insert("x-claude-code-session-id", value);
    }

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
        HeaderValue::from_static("0.128.0"),
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
    parse_retry_after_value(val, chrono::Utc::now())
}

/// Milliseconds a `Retry-After` value asks us to wait. Like Claude Code, accepts
/// both forms HTTP allows: seconds (fractions included) and an HTTP date, which
/// counts down from `now` and is never negative. Anything else is `None`.
fn parse_retry_after_value(val: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    let val = val.trim();
    if let Ok(secs) = val.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| (secs * 1000.0).round() as u64);
    }
    let when = chrono::DateTime::parse_from_rfc2822(val).ok()?;
    let ms = when
        .with_timezone(&chrono::Utc)
        .signed_duration_since(now)
        .num_milliseconds();
    Some(ms.max(0) as u64)
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

    let headers = match build_headers(&token.access_token, None, Some(models::HAIKU_4_5.id), None) {
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
    let url = format!("{}/v1/messages?beta=true", state.base_url);
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
            state
                .update_rate_limits(resp.headers(), models::ModelFamily::Haiku)
                .await;

            let map = state.rate_limit.read().await;
            let rl = map
                .get(&models::ModelFamily::Haiku)
                .cloned()
                .unwrap_or_default();
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

// ── Upstream error bodies ──────────────────────────────────────────

/// The Anthropic error envelope, `{"type":"error","error":{"type","message"}}`.
fn error_envelope(error_type: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": {"type": error_type, "message": message.into()},
    })
}

/// An upstream error body as JSON, losing nothing it said.
///
/// A JSON object is returned as it is. Anything else -- an HTML page from an
/// edge proxy in front of the API, plain text, an empty body -- becomes the
/// whole text as the `message` of an error envelope. (These used to be read
/// with `unwrap_or_default()`, which turned them into `null`.)
fn error_json(status: u16, text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) if value.is_object() => value,
        _ if text.trim().is_empty() => error_envelope(
            "api_error",
            format!("Upstream answered HTTP {status} with an empty body"),
        ),
        _ => error_envelope("api_error", text),
    }
}

/// Read an upstream error response into JSON; see [`error_json`].
pub async fn error_body(resp: reqwest::Response) -> serde_json::Value {
    let status = resp.status().as_u16();
    match resp.text().await {
        Ok(text) => error_json(status, &text),
        Err(e) => error_envelope(
            "api_error",
            format!("Upstream answered HTTP {status}, but its body could not be read: {e}"),
        ),
    }
}

// ── Token counting and model lookup ────────────────────────────────

/// Request fields the upstream `count_tokens` endpoint accepts. Everything else
/// a Messages request carries (`max_tokens`, `stream`, `temperature`,
/// `metadata`, ...) is rejected there as an unknown input, so it is dropped.
const COUNT_TOKENS_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "mcp_servers",
    "context_management",
];

/// The body `count_tokens` is sent: the counting-relevant part of a Messages
/// request body.
pub fn count_tokens_body(body: &serde_json::Value) -> serde_json::Value {
    let kept: serde_json::Map<String, serde_json::Value> = body
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(k, _)| COUNT_TOKENS_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    serde_json::Value::Object(kept)
}

/// Count the input tokens of a Messages request upstream (`POST
/// /v1/messages/count_tokens`). Free of charge and outside the Messages rate
/// limits, so it neither consults nor updates the cached rate-limit state.
pub async fn count_tokens(
    state: &Arc<ProxyState>,
    body: &serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(serde_json::Value, u16), ProxyError> {
    let counted = count_tokens_body(body);
    let model = counted.get("model").and_then(|v| v.as_str());
    let token = auth::get_oauth_token_fresh()
        .await
        .map_err(|e| ProxyError::Auth { message: e })?;
    let headers = build_headers(&token.access_token, extra_betas, model, Some(session_id))
        .map_err(ProxyError::Internal)?;
    let url = format!("{}/v1/messages/count_tokens?beta=true", state.base_url);
    send_json(state.client.post(&url).headers(headers).json(&counted)).await
}

/// `GET` a JSON resource from the upstream API, e.g. `/v1/models/<id>`, with the
/// gateway's client identity.
pub async fn upstream_get(
    state: &Arc<ProxyState>,
    path_and_query: &str,
) -> Result<(serde_json::Value, u16), ProxyError> {
    let token = auth::get_oauth_token_fresh()
        .await
        .map_err(|e| ProxyError::Auth { message: e })?;
    let headers =
        build_headers(&token.access_token, None, None, None).map_err(ProxyError::Internal)?;
    let url = format!("{}{}", state.base_url, path_and_query);
    send_json(state.client.get(&url).headers(headers)).await
}

async fn send_json(
    request: reqwest::RequestBuilder,
) -> Result<(serde_json::Value, u16), ProxyError> {
    let resp = request
        .send()
        .await
        .map_err(|e| ProxyError::Transport(format!("{e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| ProxyError::Transport(format!("Failed to parse response: {e}")))?;
    Ok((body, status))
}

// ── API Calls ──────────────────────────────────────────────────────

/// Synchronous (non-streaming) Messages API call.
/// Handles: pre-flight rate limit check, 529 retry, 401 token invalidation.
///
/// `session_id` must be provided by the caller: use a fresh UUID per request
/// for the stateless direct proxy, or a stable per-session UUID for
/// multi-turn proxy sessions. See `client_identity` module docs for why.
async fn messages_sync_once(
    state: &Arc<ProxyState>,
    mut body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(serde_json::Value, u16), ProxyError> {
    // Inject metadata.user_id for rate-limit attribution.
    crate::client_identity::inject_metadata(&mut body, session_id);

    // Owned (not &str into `body`) so it outlives the mutable borrows below
    // (billing-block injection) — still used for the family/haiku checks
    // here and for headers/logging further down.
    let model_in_body = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // Resolved once, up front: both pre-flight checks and the eventual
    // update_rate_limits() call must all key off the SAME family, so a
    // Fable call is never gated by Sonnet's cached status or vice versa.
    let family = models::family_of(model_in_body.as_deref().unwrap_or(""));

    // Non-Haiku models require the billing-header system block or the server
    // rejects with a generic 429 "Error". Haiku accepts requests without it.
    if model_in_body
        .as_deref()
        .map(|m| !is_haiku_model(m))
        .unwrap_or(false)
    {
        crate::client_identity::inject_billing_system_block(&mut body);
    }

    // REVERTED 2026-10-24: `fallbacks` (both "default" and a named
    // claude-opus-4-8 list) is a dead end on this account for claude-sonnet-5
    // — "default" returns recommended_model:null (nothing to route to, just
    // points at the Cyber Verification Program) and the named-model form
    // 400s outright ("'claude-sonnet-5' does not support the `fallbacks`
    // parameter"), which broke every single call — not just refused ones —
    // for ~10 hours before this was caught. Do not re-add without first
    // confirming with one manual curl call that the exact value sent doesn't
    // 400. Refusals fall through to ARTEX's own idle-round retry instead, as
    // before this experiment.

    // Pre-flight: check cached rate limit (this family only)
    if let Some(block) = state.pre_flight_check(family).await {
        return Err(block.into_error());
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
    if let Some(block) = state.pre_flight_check(family).await {
        return Err(block.into_error());
    }

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    // The CLI uses `/v1/messages?beta=true` (the Node SDK's
    // `client.beta.messages.create()` endpoint). Without `?beta=true` the
    // request is routed to the legacy Messages handler that rejects
    // non-Haiku OAuth traffic with 429 `{message:"Error"}`.
    let url = format!("{}/v1/messages?beta=true", state.base_url);

    // Measurement, not inference: log the actual outgoing request shape so a
    // long-context rejection can be matched to the request that caused it.
    let req_bytes = serde_json::to_vec(&body).map(|v| v.len()).unwrap_or(0);
    let msg_count = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let system_bytes = body
        .get("system")
        .map(|s| serde_json::to_string(s).map(|s| s.len()).unwrap_or(0))
        .unwrap_or(0);
    tracing::info!(
        "[measure] outgoing request: model={:?} session={} req_bytes={} messages={} system_bytes={}",
        model_in_body,
        session_id,
        req_bytes,
        msg_count,
        system_bytes,
    );

    // Retry loop for 529 (overloaded) only
    let mut last_status = 0u16;
    for attempt in 1..=(MAX_OVERLOAD_RETRIES + 1) {
        let headers = build_headers(
            &token.access_token,
            extra_betas,
            model_in_body.as_deref(),
            Some(session_id),
        )
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
        state.update_rate_limits(resp.headers(), family).await;

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
                    tracing::info!(
                        "[measure] response usage: {}",
                        serde_json::to_string(usage).unwrap_or_default(),
                    );
                    // output_tokens:0 with stop_reason:"refusal" is Anthropic's
                    // safety classifier declining the request. Log it for
                    // visibility, but hand the response back unchanged: the
                    // gateway never swaps in a different model.
                    if resp_body.get("stop_reason").and_then(|v| v.as_str()) == Some("refusal") {
                        tracing::warn!(
                            "[refusal] session={} category={:?} model={:?}",
                            session_id,
                            resp_body
                                .get("stop_details")
                                .and_then(|d| d.get("category")),
                            resp_body.get("model"),
                        );
                    }
                }

                return Ok((resp_body, 200));
            }

            401 => {
                let resp_body = error_body(resp).await;
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
                state.mark_rejected(family, resp.headers()).await;

                let map = state.rate_limit.read().await;
                let msg = map
                    .get(&family)
                    .cloned()
                    .unwrap_or_default()
                    .rejection_message();
                drop(map);

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

                // A JSON body is passed on as it is. Anything else keeps its
                // text next to the gateway's own rate-limit summary.
                let resp_body = match serde_json::from_str::<serde_json::Value>(&raw_body) {
                    Ok(value) if value.is_object() => value,
                    _ if raw_body.trim().is_empty() => error_envelope("rate_limit_error", msg),
                    _ => error_envelope("rate_limit_error", format!("{msg}: {raw_body}")),
                };
                return Ok((resp_body, 429));
            }

            529 => {
                // Overloaded — retry with backoff (up to MAX_OVERLOAD_RETRIES)
                if attempt > MAX_OVERLOAD_RETRIES {
                    tracing::warn!(
                        "Overloaded (529): max retries ({}) exceeded",
                        MAX_OVERLOAD_RETRIES
                    );
                    let resp_body = error_body(resp).await;
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
                let resp_body = error_body(resp).await;
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
async fn messages_stream_once(
    state: &Arc<ProxyState>,
    mut body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(reqwest::Response, u16), ProxyError> {
    // Inject metadata.user_id for rate-limit attribution.
    crate::client_identity::inject_metadata(&mut body, session_id);

    // Owned — see messages_sync for why (outlives the mutable borrow below).
    let model_in_body = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // See messages_sync: resolved once so every rate-limit check/update below
    // keys off the same family.
    let family = models::family_of(model_in_body.as_deref().unwrap_or(""));

    // Non-Haiku billing-header system block (see messages_sync).
    if model_in_body
        .as_deref()
        .map(|m| !is_haiku_model(m))
        .unwrap_or(false)
    {
        crate::client_identity::inject_billing_system_block(&mut body);
    }

    // See messages_sync: `fallbacks` reverted — 400s outright on this model/
    // account combo and broke every call, not just refused ones.

    // Pre-flight: check cached rate limit (this family only)
    if let Some(block) = state.pre_flight_check(family).await {
        return Err(block.into_error());
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
    if let Some(block) = state.pre_flight_check(family).await {
        return Err(block.into_error());
    }

    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let url = format!("{}/v1/messages?beta=true", state.base_url);
    let mut attempted_recovery = false;
    loop {
        let headers = build_headers(
            &token.access_token,
            extra_betas,
            model_in_body.as_deref(),
            Some(session_id),
        )
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
        state.update_rate_limits(resp.headers(), family).await;

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
            state.mark_rejected(family, resp.headers()).await;
        }

        return Ok((resp, status));
    }
}

// ── Context overflow: let the server say what fits ────────────────

/// The least a reply may be given. Below this, lowering `max_tokens` cannot help
/// (Claude Code's `FLOOR_OUTPUT_TOKENS`).
const FLOOR_OUTPUT_TOKENS: u64 = 3_000;

/// Tokens kept free beyond what the server counted, to absorb the slack between
/// its count and the limit it enforces (the margin Claude Code leaves).
const OVERFLOW_HEADROOM_TOKENS: u64 = 1_000;

/// Most adjustments of one request. Each one has to lower `max_tokens`, so the
/// loop ends anyway; this is a backstop.
const MAX_OVERFLOW_ADJUSTMENTS: u32 = 3;

/// The numbers in upstream's `400 input length and `max_tokens` exceed context
/// limit: <input> + <max_tokens> > <limit>` error.
#[derive(Debug, PartialEq, Eq)]
struct ContextOverflow {
    input_tokens: u64,
    limit: u64,
}

impl ContextOverflow {
    /// The `max_tokens` that fits: what is left of the window after the input
    /// the server counted and the headroom, if that leaves a usable reply.
    fn adjusted_max_tokens(&self) -> Option<u64> {
        let available = self
            .limit
            .saturating_sub(self.input_tokens)
            .saturating_sub(OVERFLOW_HEADROOM_TOKENS);
        (available >= FLOOR_OUTPUT_TOKENS).then_some(available)
    }
}

/// Read a run of ASCII digits off the front of `text`.
fn take_number(text: &str) -> Option<(u64, &str)> {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, rest) = text.split_at(end);
    digits.parse().ok().map(|n| (n, rest))
}

/// Parse the overflow error message. Claude Code matches
/// `/input length and `max_tokens` exceed context limit: (\d+) \+ (\d+) > (\d+)/`.
fn parse_context_overflow(message: &str) -> Option<ContextOverflow> {
    const MARKER: &str = "input length and `max_tokens` exceed context limit: ";
    let (_, rest) = message.split_once(MARKER)?;
    let (input_tokens, rest) = take_number(rest)?;
    let rest = rest.trim_start().strip_prefix('+')?.trim_start();
    let (_max_tokens, rest) = take_number(rest)?;
    let rest = rest.trim_start().strip_prefix('>')?.trim_start();
    let (limit, _) = take_number(rest)?;
    Some(ContextOverflow {
        input_tokens,
        limit,
    })
}

/// The `max_tokens` to retry `request` with after upstream answered `error`,
/// when the error is a context overflow that lowering `max_tokens` can fix. A
/// retry that would not lower it is refused, so the caller gets upstream's 400.
fn overflow_retry_max_tokens(
    error: &serde_json::Value,
    request: &serde_json::Value,
) -> Option<u64> {
    let message = error.get("error")?.get("message")?.as_str()?;
    let adjusted = parse_context_overflow(message)?.adjusted_max_tokens()?;
    match request
        .get("max_tokens")
        .and_then(serde_json::Value::as_u64)
    {
        Some(current) if adjusted >= current => None,
        _ => Some(adjusted),
    }
}

/// Synchronous Messages API call.
///
/// Whether a request fits cannot be known without upstream's tokenizer, so
/// this does what Claude Code does: send it, and when upstream answers that
/// `input + max_tokens` exceeds the window, take the input count from the
/// error and retry with `max_tokens = limit - input - 1000`.
/// See `messages_sync_once` for the rest of the behavior and the `session_id`
/// contract.
pub async fn messages_sync(
    state: &Arc<ProxyState>,
    body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(serde_json::Value, u16), ProxyError> {
    let mut body = body;
    let mut adjustments = 0;
    loop {
        let (resp, status) =
            messages_sync_once(state, body.clone(), extra_betas, session_id).await?;
        if status == 400 && adjustments < MAX_OVERFLOW_ADJUSTMENTS {
            if let Some(max_tokens) = overflow_retry_max_tokens(&resp, &body) {
                adjustments += 1;
                tracing::warn!(
                    "[max_tokens] session={session_id} does not fit the context window; \
                     retrying with max_tokens={max_tokens} (from upstream's own counts)"
                );
                body["max_tokens"] = serde_json::json!(max_tokens);
                continue;
            }
        }
        return Ok((resp, status));
    }
}

/// Streaming Messages API call; lowers `max_tokens` on a context overflow like
/// [`messages_sync`]. The overflow is reported before any streaming starts, as
/// an ordinary 400, so the retry happens before the first byte is relayed.
pub async fn messages_stream(
    state: &Arc<ProxyState>,
    body: serde_json::Value,
    extra_betas: Option<&[String]>,
    session_id: &str,
) -> Result<(reqwest::Response, u16), ProxyError> {
    let mut body = body;
    let mut adjustments = 0;
    loop {
        let (resp, status) =
            messages_stream_once(state, body.clone(), extra_betas, session_id).await?;
        if status != 400 {
            return Ok((resp, status));
        }

        // A 400 is a small JSON error. Read it to see whether lowering
        // max_tokens fixes it; if not, hand back an equivalent response.
        let headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ProxyError::Transport(format!("Failed to read error response: {e}")))?;
        if adjustments < MAX_OVERFLOW_ADJUSTMENTS {
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if let Some(max_tokens) = overflow_retry_max_tokens(&error, &body) {
                adjustments += 1;
                tracing::warn!(
                    "[max_tokens] session={session_id} does not fit the context window; \
                     retrying with max_tokens={max_tokens} (from upstream's own counts)"
                );
                body["max_tokens"] = serde_json::json!(max_tokens);
                continue;
            }
        }
        let mut rebuilt = axum::http::Response::builder().status(status);
        if let Some(h) = rebuilt.headers_mut() {
            *h = headers;
        }
        let rebuilt = rebuilt
            .body(bytes)
            .map_err(|e| ProxyError::Internal(format!("Failed to rebuild error response: {e}")))?;
        return Ok((reqwest::Response::from(rebuilt), status));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fable_7d_oi_headers_override_the_generic_7d_figure() {
        // Regression: 2026-10-24 — confirmed live that a Fable response
        // carries both the generic (blended, shared-with-everyone) 7d figure
        // AND a `7d_oi` figure that's Fable's own real sub-allocation
        // (Sonnet/Opus/Haiku responses never send `7d_oi`). The `7d_oi`
        // status/utilization/reset must win so Fable's cached state reflects
        // Fable's own constraint, not the unrelated general pool's.
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            HeaderValue::from_static("allowed_warning"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("0.92"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-reset",
            HeaderValue::from_static("1000"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d_oi-status",
            HeaderValue::from_static("allowed"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d_oi-utilization",
            HeaderValue::from_static("0.0"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d_oi-reset",
            HeaderValue::from_static("2000"),
        );

        let state = ProxyState::new(1);
        state
            .update_rate_limits(&headers, models::ModelFamily::Fable)
            .await;

        let map = state.rate_limit.read().await;
        let rl = map.get(&models::ModelFamily::Fable).unwrap();
        assert_eq!(rl.status, Some("allowed".to_string()));
        assert_eq!(rl.utilization_7d, Some(0.0));
        assert_eq!(rl.resets_at, Some(2000.0));
    }

    #[test]
    fn haiku_5_5_is_not_treated_as_reduced_haiku() {
        // Regression: 2026-10-24 — Haiku 5.5 needs the billing block and
        // NON_HAIKU_BETAS just like Sonnet/Opus; only Haiku 4.5 is exempt.
        assert!(!is_haiku_model("claude-haiku-5-5"));
        assert!(!is_haiku_model("CLAUDE-HAIKU-5-5"));
        assert!(is_haiku_model("claude-haiku-4-5"));
        assert!(is_haiku_model("claude-haiku-4-5-20251001"));
    }

    // ── Retry-After parsing ────────────────────────────────────────

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("test timestamp")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn retry_after_accepts_whole_and_fractional_seconds() {
        let now = at("2026-10-21T07:27:00Z");
        assert_eq!(parse_retry_after_value("120", now), Some(120_000));
        assert_eq!(parse_retry_after_value(" 1.5 ", now), Some(1_500));
        assert_eq!(parse_retry_after_value("0", now), Some(0));
    }

    #[test]
    fn retry_after_accepts_an_http_date() {
        let now = at("2026-10-21T07:27:00Z");
        // 2026-10-21 is a Wednesday.
        assert_eq!(
            parse_retry_after_value("Wed, 21 Oct 2026 07:28:00 GMT", now),
            Some(60_000)
        );
        // A date already past means "now", not a negative wait.
        assert_eq!(
            parse_retry_after_value("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(0)
        );
    }

    #[test]
    fn retry_after_rejects_what_is_not_a_wait() {
        let now = at("2026-10-21T07:27:00Z");
        for bad in ["", "soon", "-5", "inf", "NaN", "12 minutes"] {
            assert_eq!(parse_retry_after_value(bad, now), None, "{bad:?}");
        }
    }

    // ── context overflow ───────────────────────────────────────────

    #[test]
    fn overflow_message_yields_the_servers_input_count() {
        let msg = "input length and `max_tokens` exceed context limit: 990000 + 64000 > 1000000, \
                   decrease input length or `max_tokens` and try again";
        let overflow = parse_context_overflow(msg).expect("parses");
        assert_eq!(
            overflow,
            ContextOverflow {
                input_tokens: 990_000,
                limit: 1_000_000
            }
        );
        assert_eq!(overflow.adjusted_max_tokens(), Some(9_000));
    }

    #[test]
    fn overflow_parser_ignores_other_errors() {
        for msg in [
            "",
            "prompt is too long: 215000 tokens > 200000 maximum",
            "input length and `max_tokens` exceed context limit: abc + 1 > 2",
            "input length and `max_tokens` exceed context limit: 1 + 2",
            "max_tokens: 200000 > 64000, which is the maximum allowed",
        ] {
            assert_eq!(parse_context_overflow(msg), None, "{msg:?}");
        }
    }

    #[test]
    fn adjusted_max_tokens_needs_room_for_a_real_reply() {
        let at = |input, limit| ContextOverflow {
            input_tokens: input,
            limit,
        };
        // Exactly the 3,000-token floor is enough; one token less is not.
        assert_eq!(at(196_000, 200_000).adjusted_max_tokens(), Some(3_000));
        assert_eq!(at(196_001, 200_000).adjusted_max_tokens(), None);
        // Input beyond the limit leaves nothing (and must not underflow).
        assert_eq!(at(300_000, 200_000).adjusted_max_tokens(), None);
    }

    #[test]
    fn a_retry_must_lower_max_tokens() {
        let error = |input: u64, max: u64, limit: u64| {
            serde_json::json!({"error": {"message": format!(
                "input length and `max_tokens` exceed context limit: {input} + {max} > {limit}"
            )}})
        };
        let request = |max: u64| serde_json::json!({"max_tokens": max});
        // 200_000 - 150_000 - 1_000 = 49_000
        assert_eq!(
            overflow_retry_max_tokens(&error(150_000, 64_000, 200_000), &request(64_000)),
            Some(49_000)
        );
        // Already at or below what would fit: nothing to gain.
        assert_eq!(
            overflow_retry_max_tokens(&error(150_000, 49_000, 200_000), &request(49_000)),
            None
        );
        // A request that names no max_tokens can still be given one.
        assert_eq!(
            overflow_retry_max_tokens(&error(150_000, 64_000, 200_000), &serde_json::json!({})),
            Some(49_000)
        );
    }

    // ── upstream error bodies ──────────────────────────────────────

    #[test]
    fn a_json_error_is_passed_on_untouched() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1"}"#;
        assert_eq!(
            error_json(529, body),
            serde_json::from_str::<serde_json::Value>(body).expect("json")
        );
    }

    #[test]
    fn other_bodies_keep_their_whole_text() {
        let html = "<html>\n<h1>502 Bad Gateway</h1>\n</html>\n";
        let v = error_json(502, html);
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["message"], html);
        // JSON that is not an object is text too, not something to drop.
        assert_eq!(error_json(500, "null")["error"]["message"], "null");
        assert_eq!(error_json(500, "\"oops\"")["error"]["message"], "\"oops\"");
    }

    #[test]
    fn an_empty_body_says_so() {
        for empty in ["", "  \n"] {
            let v = error_json(503, empty);
            assert_eq!(
                v["error"]["message"],
                "Upstream answered HTTP 503 with an empty body"
            );
        }
    }
}
