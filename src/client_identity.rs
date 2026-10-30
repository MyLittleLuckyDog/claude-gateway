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
/// CLI — the server logs may correlate by this. Originally verified against
/// the real CLI (2.1.81) via a fetch-hook capture; see DEBUG_SONNET_429.md
/// (file not present in this checkout). Bumped 2026-10-24 to the version
/// this account's installed CLI actually reports (`claude-code@2.1.302` via
/// `npm list -g`, matching `~/.claude.json`'s `lastClawdEntranceVersion`) —
/// Opus 5.5 / Fable 5.1 requests were otherwise rejected with
/// `claude_code_version_too_old` (server requires >=2.1.280). The exact
/// cc_version format (`${VERSION}.${counter}`) and the gating logic were
/// read directly out of the installed CLI's bundled JS
/// (`~/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`,
/// function `PPo`) rather than guessed.
///
/// Synced to 2.1.314 on 2026-10-30, the CLI installed on this machine, whose
/// bundle was unpacked to confirm the User-Agent, billing-header and
/// `X-Claude-Code-Session-Id` shapes used here. Note that the `.N` after the
/// version in `cc_version` is not a counter: see [`cc_version_fingerprint`].
const CLAUDE_CLI_VERSION: &str = "2.1.314";

/// Salt of the `cc_version` fingerprint (constant `x0` in the 2.1.314 bundle;
/// the same value appears in 2.1.241).
const FINGERPRINT_SALT: &str = "59cf53e54c78";

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
    format!(
        "{USER_AGENT_PREFIX}/{CLAUDE_CLI_VERSION} ({user_type}, {})",
        entrypoint()
    )
}

/// The client entrypoint reported upstream (`cli` unless the environment says
/// otherwise), shared by the User-Agent and the billing header.
fn entrypoint() -> String {
    std::env::var("CLAUDE_CODE_ENTRYPOINT").unwrap_or_else(|_| "cli".to_string())
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
/// triggers the same rejection. Originally verified via fetch-hook capture
/// against the real CLI 2.1.81 and curl ablation; see DEBUG_SONNET_429.md
/// (file not present in this checkout). `cc_version`'s number bumped
/// 2026-10-24 in lockstep with CLAUDE_CLI_VERSION above — see that constant's
/// comment. Only the version changed; `cc_entrypoint`/`cch` are untouched
/// since they weren't implicated in the Opus 5.5 rejection this was fixing.
///
/// `cc_version` is `<cli version>.<fingerprint>` where the fingerprint depends
/// on the conversation's first user message; the block is therefore built per
/// request by [`billing_system_block`].
pub fn billing_system_block(first_user_text: &str) -> String {
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CLI_VERSION}.{}; cc_entrypoint={}; cch=00000;",
        cc_version_fingerprint(first_user_text),
        entrypoint()
    )
}

/// The three-hex-digit suffix of `cc_version`, as Claude Code computes it
/// (2.1.314 bundle, `Cf`): SHA-256 of
/// `salt + chars [4, 7, 20] of the first user message + cli version`, first
/// three hex digits. A missing character counts as `"0"`.
///
/// JavaScript indexes strings by UTF-16 code unit and hashes the joined string
/// as UTF-8, so the units are collected first and decoded together: a valid
/// surrogate pair formed by neighbouring picks becomes one character, a lone
/// surrogate becomes U+FFFD, exactly as in Node.
pub fn cc_version_fingerprint(first_user_text: &str) -> String {
    use sha2::{Digest, Sha256};

    let units: Vec<u16> = first_user_text.encode_utf16().collect();
    let picked: Vec<u16> = [4usize, 7, 20]
        .iter()
        .map(|&i| units.get(i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let picked = String::from_utf16_lossy(&picked);
    let digest = Sha256::digest(format!("{FINGERPRINT_SALT}{picked}{CLAUDE_CLI_VERSION}"));
    format!("{:02x}{:02x}", digest[0], digest[1])[..3].to_string()
}

/// Text of the first user message, the way the CLI reads it: a string body as
/// is, otherwise the first `text` block; empty when there is none.
fn first_user_text(body: &serde_json::Value) -> String {
    let first = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        });
    match first.and_then(|m| m.get("content")) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .find(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Prepend the billing-header system block to the request body so Sonnet/
/// Opus requests are recognized as Claude Code traffic. Idempotent: if the
/// marker is already present (caller-supplied or a prior pass) we leave it
/// alone. Preserves any existing system content the caller provided by
/// converting it to an array form and appending after the marker.
pub fn inject_billing_system_block(body: &mut serde_json::Value) {
    let first_user = first_user_text(body);
    let obj = match body.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // Build the marker block once (the fingerprint reads the first user message).
    let marker_block = serde_json::json!({
        "type": "text",
        "text": billing_system_block(&first_user),
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
            obj.insert("system".to_string(), serde_json::json!([marker_block]));
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

    // ── billing header / cc_version fingerprint ────────────────────

    /// Golden values produced by running the CLI's own algorithm (2.1.314
    /// bundle, `Cf`) in Node: sha256(salt + s[4]s[7]s[20] + version)[..3].
    const FINGERPRINT_VECTORS: &[(&str, &str)] = &[
        ("", "cfe"),
        ("hi", "cfe"),
        ("abcde", "0f2"),
        ("Reply with the single word: ok", "d8d"),
        (
            "The quick brown fox jumps over the lazy dog, twice over.",
            "582",
        ),
        (
            "안녕하세요, 오늘 날씨가 어떤가요? 알려주세요 부탁드립니다",
            "969",
        ),
        // Characters outside the BMP are two UTF-16 units, as in JavaScript.
        ("abcd😀fgh😀jklmnopqrstuvwxyz", "593"),
        ("abcdefg😀ijklmnopqrstuvwxyz0123", "8c1"),
        ("0000000000000000000000000", "cfe"),
    ];

    #[test]
    fn fingerprint_matches_the_clis_own_algorithm() {
        for (text, expected) in FINGERPRINT_VECTORS {
            assert_eq!(&cc_version_fingerprint(text), expected, "input {text:?}");
        }
    }

    #[test]
    fn billing_block_has_the_clis_shape() {
        let block = billing_system_block("Reply with the single word: ok");
        assert!(
            block.starts_with("x-anthropic-billing-header: cc_version=2.1.314.d8d; cc_entrypoint="),
            "{block}"
        );
        assert!(block.ends_with("; cch=00000;"), "{block}");
    }

    #[test]
    fn injected_block_fingerprints_the_first_user_message() {
        let block_for = |messages: serde_json::Value| {
            let mut body = serde_json::json!({"model": "x", "messages": messages});
            inject_billing_system_block(&mut body);
            body["system"][0]["text"].as_str().unwrap_or("").to_string()
        };

        // String content, and the first text block of block content, agree.
        let a = block_for(
            serde_json::json!([{"role": "user", "content": "The quick brown fox jumps"}]),
        );
        let b = block_for(serde_json::json!([{"role": "user", "content": [
            {"type": "image", "source": {}},
            {"type": "text", "text": "The quick brown fox jumps"},
        ]}]));
        assert_eq!(a, b);

        // Only the first *user* message counts, not an earlier assistant turn
        // and not later turns.
        let c = block_for(serde_json::json!([
            {"role": "assistant", "content": "ignored"},
            {"role": "user", "content": "The quick brown fox jumps"},
            {"role": "user", "content": "something else entirely"},
        ]));
        assert_eq!(a, c);

        // A different opening message gives a different fingerprint.
        let d = block_for(
            serde_json::json!([{"role": "user", "content": "Reply with the single word: ok"}]),
        );
        assert_ne!(a, d);
    }

    #[test]
    fn user_agent_reports_the_synced_cli_version() {
        assert!(
            user_agent().starts_with("claude-cli/2.1.314 ("),
            "{}",
            user_agent()
        );
    }
}
