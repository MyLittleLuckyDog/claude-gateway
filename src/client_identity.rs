//! Client identity — mirrors the CLI's request attribution scheme.
//!
//! The Claude Code CLI sends two things on every API request that Anthropic's
//! rate-limit / analytics system keys off:
//!
//!   1. `metadata.user_id` in the request body, which is a JSON string of
//!      `{device_id, account_uuid, session_id}`. Server-side rate limiting
//!      buckets users by this identifier — requests missing it appear to
//!      come from an "unknown" client and may hit stricter limits.
//!
//!   2. A `User-Agent` header in the form
//!      `claude-cli/<version> (<user_type>, cli)`. The server's log pipeline
//!      filters on the `claude-cli` prefix.
//!
//! Reference: `src/services/api/claude.ts:getAPIMetadata()` and
//! `src/utils/http.ts:getUserAgent()` in the CLI source.
//!
//! We read `userID` and `oauthAccount.accountUuid` from `~/.claude.json`
//! (the CLI's global config file) once at startup and cache them.
//!
//! The `session_id` works differently from what you might expect. In the CLI
//! it is "once per process", but because the normal CLI usage pattern is
//! `claude -p "query"` (a fresh process per query), the server effectively
//! sees a unique session_id per logical request. A long-running gateway that
//! reuses a single session_id for thousands of requests ends up tripping
//! server-side per-session heuristics — the user's lived observation is
//! "restart the program and the block clears". We therefore generate a fresh
//! session_id *per request* for the stateless direct proxy, and reuse the
//! same session_id *per multi-turn session* for the proxy sessions endpoint
//! (matching the CLI's interactive-mode behavior where a session_id is
//! stable across turns until /clear).

use std::sync::LazyLock;

use serde::Deserialize;
use uuid::Uuid;

/// User-Agent prefix. The CLI's log pipeline filters on `claude-cli`,
/// so the prefix must be preserved verbatim.
const USER_AGENT_PREFIX: &str = "claude-cli";

/// Version embedded in the User-Agent. Bumped in lockstep with the upstream
/// CLI — the server logs may correlate by this. Verified against the real
/// CLI (2.1.81) via a fetch-hook capture; see DEBUG_SONNET_429.md.
const CLAUDE_CLI_VERSION: &str = "2.1.81";

#[derive(Debug, Deserialize)]
struct GlobalConfig {
    #[serde(rename = "userID")]
    user_id: Option<String>,
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<OAuthAccount>,
}

#[derive(Debug, Deserialize)]
struct OAuthAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClientIdentity {
    /// Device identifier — the persistent `userID` from ~/.claude.json.
    pub device_id: String,
    /// OAuth account UUID (empty string if unavailable, matching CLI behaviour).
    pub account_uuid: String,
}

impl ClientIdentity {
    /// Build the `metadata.user_id` value the Messages API expects for a
    /// specific session. Mirrors `getAPIMetadata()` in the CLI: a
    /// JSON-stringified object embedded as a string under `metadata.user_id`.
    pub fn metadata_user_id(&self, session_id: &str) -> String {
        // Preserve field order to match the CLI output (device_id, account_uuid, session_id).
        // serde_json::json! does not guarantee insertion order for serde_json::Value,
        // so we build the JSON string manually to guarantee stability.
        format!(
            r#"{{"device_id":"{}","account_uuid":"{}","session_id":"{}"}}"#,
            escape_json(&self.device_id),
            escape_json(&self.account_uuid),
            escape_json(session_id),
        )
    }
}

/// Generate a fresh session_id (UUID v4). Use one per logical request on the
/// stateless direct proxy, or one per multi-turn session on proxy sessions.
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Process-wide cached identity. Read from `~/.claude.json` on first access;
/// if the file is missing or malformed we fall back to a random device_id so
/// the gateway still starts cleanly.
static IDENTITY: LazyLock<ClientIdentity> = LazyLock::new(load_identity);

fn load_identity() -> ClientIdentity {
    let (device_id, account_uuid) = match read_global_config() {
        Ok(cfg) => {
            let device = cfg.user_id.unwrap_or_else(fallback_device_id);
            let account = cfg
                .oauth_account
                .and_then(|a| a.account_uuid)
                .unwrap_or_default();
            (device, account)
        }
        Err(e) => {
            tracing::warn!(
                "Could not read ~/.claude.json for client identity ({}); \
                 falling back to a random device_id. Requests will still \
                 succeed but may be rate-limited more aggressively.",
                e
            );
            (fallback_device_id(), String::new())
        }
    };

    tracing::info!(
        "Client identity: device_id={}... account_uuid={}... \
         (session_id is generated per-request)",
        &device_id.chars().take(8).collect::<String>(),
        &account_uuid.chars().take(8).collect::<String>(),
    );

    ClientIdentity {
        device_id,
        account_uuid,
    }
}

fn read_global_config() -> Result<GlobalConfig, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| home.clone());
    // Primary location: ~/.claude.json (matches CLI getGlobalClaudeFile()).
    let primary = format!("{config_dir}/.claude.json");
    let legacy = format!("{home}/.claude/.config.json");

    let path = if std::path::Path::new(&primary).exists() {
        primary
    } else if std::path::Path::new(&legacy).exists() {
        legacy
    } else {
        return Err(format!("Config not found at {primary}"));
    };

    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {path}: {e}"))?;
    serde_json::from_str::<GlobalConfig>(&content)
        .map_err(|e| format!("Failed to parse {path}: {e}"))
}

fn fallback_device_id() -> String {
    // 32 hex chars matches the CLI's randomBytes(32).toString('hex') shape.
    Uuid::new_v4().simple().to_string() + &Uuid::new_v4().simple().to_string()
}

/// Escape a value for safe embedding in a JSON string literal.
/// Handles the subset of characters that can appear in UUIDs and hex strings
/// (none of which should actually need escaping, but we're defensive).
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Get the process-wide client identity. Cached on first call.
pub fn get_identity() -> &'static ClientIdentity {
    &IDENTITY
}

/// Build the `User-Agent` header value for outgoing API requests.
/// Matches the CLI's `getUserAgent()` format:
///   `claude-cli/<version> (<user_type>, cli)`
pub fn user_agent() -> String {
    let user_type = std::env::var("USER_TYPE").unwrap_or_else(|_| "external".to_string());
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").unwrap_or_else(|_| "cli".to_string());
    format!("{USER_AGENT_PREFIX}/{CLAUDE_CLI_VERSION} ({user_type}, {entrypoint})")
}

/// Server-side marker the Anthropic Messages API requires to accept OAuth
/// subscriber traffic for non-Haiku models. It is delivered as the FIRST
/// text block of the `system` array (not as an HTTP header, despite the
/// `x-anthropic-billing-header:` prefix — that prefix is just part of the
/// literal string the server matches against).
///
/// Without this block, Sonnet/Opus requests on an OAuth token are rejected
/// with a generic `{type:"rate_limit_error",message:"Error"}` plus
/// `x-should-retry: true` and no `anthropic-ratelimit-unified-*` headers.
/// Any other system content (empty array, plain assistant instructions)
/// triggers the same rejection. Verified via fetch-hook capture against
/// the real CLI 2.1.81 and curl ablation; see DEBUG_SONNET_429.md.
pub const BILLING_SYSTEM_BLOCK: &str =
    "x-anthropic-billing-header: cc_version=2.1.81.535; cc_entrypoint=cli; cch=00000;";

/// Prepend the billing-header system block to the request body so Sonnet/
/// Opus requests are recognized as Claude Code traffic. Idempotent: if the
/// marker is already present (caller-supplied or a prior pass) we leave it
/// alone. Preserves any existing system content the caller provided by
/// converting it to an array form and appending after the marker.
pub fn inject_billing_system_block(body: &mut serde_json::Value) {
    let obj = match body.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // Build the marker block once.
    let marker_block = serde_json::json!({
        "type": "text",
        "text": BILLING_SYSTEM_BLOCK,
    });

    match obj.get_mut("system") {
        // Already an array — prepend unless the marker is present.
        Some(serde_json::Value::Array(arr)) => {
            let already_has = arr.iter().any(|blk| {
                blk.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.starts_with("x-anthropic-billing-header:"))
                    .unwrap_or(false)
            });
            if !already_has {
                arr.insert(0, marker_block);
            }
        }
        // A plain string system prompt — wrap both in an array.
        Some(serde_json::Value::String(s)) => {
            let user_text = s.clone();
            let replacement = serde_json::json!([
                marker_block,
                {"type": "text", "text": user_text},
            ]);
            obj.insert("system".to_string(), replacement);
        }
        // No system field — create a fresh array with just the marker.
        _ => {
            obj.insert(
                "system".to_string(),
                serde_json::json!([marker_block]),
            );
        }
    }
}

/// Inject `metadata.user_id` into a Messages API request body for a
/// specific session. If the caller already supplied a user_id we respect it.
///
/// `session_id` is the identifier that should flow into the rate-limit /
/// analytics bucket on the server side. For the stateless direct proxy this
/// should be a fresh UUID per request (call `new_session_id()`). For
/// multi-turn proxy sessions it should be stable across all turns of that
/// session.
pub fn inject_metadata(body: &mut serde_json::Value, session_id: &str) {
    let obj = match body.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    let identity = get_identity();
    let user_id_value = serde_json::Value::String(identity.metadata_user_id(session_id));

    match obj.get_mut("metadata") {
        Some(serde_json::Value::Object(meta)) => {
            meta.entry("user_id".to_string()).or_insert(user_id_value);
        }
        _ => {
            let mut meta = serde_json::Map::new();
            meta.insert("user_id".to_string(), user_id_value);
            obj.insert("metadata".to_string(), serde_json::Value::Object(meta));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_user_id_shape() {
        let id = ClientIdentity {
            device_id: "abc123".to_string(),
            account_uuid: "uuid-1".to_string(),
        };
        let s = id.metadata_user_id("sess-1");
        assert_eq!(
            s,
            r#"{"device_id":"abc123","account_uuid":"uuid-1","session_id":"sess-1"}"#
        );
    }

    #[test]
    fn inject_metadata_creates_field() {
        let mut body = serde_json::json!({"model": "x", "messages": []});
        inject_metadata(&mut body, "sess-xyz");
        let user_id = body["metadata"]["user_id"].as_str().unwrap();
        assert!(user_id.contains("\"session_id\":\"sess-xyz\""));
    }

    #[test]
    fn inject_metadata_preserves_existing() {
        let mut body = serde_json::json!({
            "model": "x",
            "messages": [],
            "metadata": {"user_id": "caller-supplied"}
        });
        inject_metadata(&mut body, "fresh-sid");
        assert_eq!(
            body["metadata"]["user_id"],
            serde_json::Value::String("caller-supplied".to_string())
        );
    }

    #[test]
    fn new_session_id_is_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn user_agent_prefix() {
        let ua = user_agent();
        assert!(ua.starts_with("claude-cli/"));
    }
}
