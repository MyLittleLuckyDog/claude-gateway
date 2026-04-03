//! OAuth token reader — reads Claude Code's stored credentials from macOS Keychain.
//! Read-only: never writes, refreshes, or modifies tokens.

use std::process::Command;
use std::sync::RwLock;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: String,
    pub expires_at: Option<i64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KeychainData {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<KeychainOAuth>,
}

#[derive(Debug, Deserialize)]
struct KeychainOAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// Cached token to avoid repeated keychain reads
static CACHED_TOKEN: std::sync::LazyLock<RwLock<Option<(OAuthToken, std::time::Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

/// Cache TTL: re-read keychain at most every 60 seconds
const CACHE_TTL_SECS: u64 = 60;

/// Read OAuth token from macOS Keychain.
/// Returns cached value if fresh enough.
pub fn get_oauth_token() -> Result<OAuthToken, String> {
    // Check cache first
    {
        let cache = CACHED_TOKEN.read().map_err(|e| format!("Lock error: {e}"))?;
        if let Some((token, cached_at)) = cache.as_ref() {
            if cached_at.elapsed().as_secs() < CACHE_TTL_SECS {
                return Ok(token.clone());
            }
        }
    }

    // Cache miss or stale — read from keychain
    let token = read_from_keychain()?;

    // Update cache
    if let Ok(mut cache) = CACHED_TOKEN.write() {
        *cache = Some((token.clone(), std::time::Instant::now()));
    }

    Ok(token)
}

/// Check if the token is likely still valid (not expired).
/// Conservative: treats unknown expiry as valid.
pub fn is_token_valid(token: &OAuthToken) -> bool {
    match token.expires_at {
        Some(expires_at) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            // 5 minute buffer
            let buffer_ms = 5 * 60 * 1000;
            now_ms + buffer_ms < expires_at
        }
        None => true, // Unknown expiry — assume valid
    }
}

/// Invalidate the cached token (e.g., after a 401 response).
pub fn invalidate_cache() {
    if let Ok(mut cache) = CACHED_TOKEN.write() {
        *cache = None;
    }
}

fn read_from_keychain() -> Result<OAuthToken, String> {
    if cfg!(not(target_os = "macos")) {
        return read_from_credentials_file();
    }

    let output = Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .map_err(|e| format!("Failed to run security command: {e}"))?;

    if !output.status.success() {
        // Fallback to plaintext credentials file
        return read_from_credentials_file();
    }

    let json_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_keychain_json(&json_str)
}

fn read_from_credentials_file() -> Result<OAuthToken, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|_| format!("{home}/.claude"));
    let path = format!("{config_dir}/.credentials.json");

    let content = std::fs::read_to_string(&path)
        .map_err(|_| "No credentials found. Run `claude /login` first.".to_string())?;

    parse_keychain_json(&content)
}

fn parse_keychain_json(json_str: &str) -> Result<OAuthToken, String> {
    let data: KeychainData = serde_json::from_str(json_str)
        .map_err(|e| format!("Failed to parse credentials: {e}"))?;

    let oauth = data.claude_ai_oauth
        .ok_or("No OAuth data in credentials")?;

    let access_token = oauth.access_token
        .filter(|t| !t.is_empty())
        .ok_or("No access token found. Run `claude /login` first.")?;

    Ok(OAuthToken {
        access_token,
        expires_at: oauth.expires_at,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
    })
}
