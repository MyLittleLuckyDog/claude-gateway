//! OAuth token reader and refresh helper for Claude Code credentials.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use futures::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type SharedTokenRead = Shared<BoxFuture<'static, Result<(u64, OAuthToken), String>>>;
type Shared401Recovery = Shared<BoxFuture<'static, Result<Option<OAuthToken>, String>>>;
type AuthResult<T> = Result<T, String>;

#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone)]
enum CredentialSource {
    Keychain { service: String, account: String },
    File { path: String },
}

#[derive(Debug, Deserialize, Serialize)]
struct SecureStorageData {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<KeychainOAuth>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct KeychainOAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
    #[serde(rename = "scopes")]
    scopes: Option<Vec<String>>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

enum PersistOutcome {
    Updated,
    KeptNewerToken(OAuthToken),
}

/// Cached token to avoid repeated keychain reads
static CACHED_TOKEN: std::sync::LazyLock<RwLock<Option<(OAuthToken, std::time::Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

static REFRESH_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

static LAST_CREDENTIALS_MTIME: std::sync::LazyLock<RwLock<Option<SystemTime>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

static CACHE_GENERATION: std::sync::LazyLock<std::sync::atomic::AtomicU64> =
    std::sync::LazyLock::new(|| std::sync::atomic::AtomicU64::new(0));

static PENDING_ASYNC_TOKEN_READ: std::sync::LazyLock<tokio::sync::Mutex<Option<SharedTokenRead>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));
static PENDING_401_RECOVERY: std::sync::LazyLock<
    tokio::sync::Mutex<HashMap<String, Shared401Recovery>>,
> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Cache TTL: re-read keychain at most every 30 seconds
const CACHE_TTL_SECS: u64 = 30;
const DEFAULT_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const MAX_REFRESH_LOCK_RETRIES: usize = 5;
const REFRESH_LOCK_STALE_SECS: u64 = 120;
const SECURITY_STDIN_LINE_LIMIT: usize = 4096 - 64;

/// Read OAuth token from storage.
/// Returns cached value if fresh enough.
pub fn get_oauth_token() -> Result<OAuthToken, String> {
    maybe_invalidate_cache_if_disk_changed();

    let stale_cached = {
        let cache = CACHED_TOKEN
            .read()
            .map_err(|e| format!("Lock error: {e}"))?;
        if let Some((token, cached_at)) = cache.as_ref() {
            if cached_at.elapsed().as_secs() < CACHE_TTL_SECS {
                return Ok(token.clone());
            }
            Some(token.clone())
        } else {
            None
        }
    };

    match read_oauth_token_from_storage_uncached() {
        Ok(token) => {
            cache_token(&token);
            Ok(token)
        }
        Err(err) => {
            if let Some(token) = stale_cached {
                tracing::warn!(
                    "Credential refresh read failed; serving stale cached token: {}",
                    err
                );
                cache_token(&token);
                Ok(token)
            } else {
                Err(err)
            }
        }
    }
}

/// Async read path with in-flight dedup to avoid stacked keychain reads.
async fn read_oauth_token_from_storage_async() -> Result<OAuthToken, String> {
    maybe_invalidate_cache_if_disk_changed();

    loop {
        {
            let cache = CACHED_TOKEN
                .read()
                .map_err(|e| format!("Lock error: {e}"))?;
            if let Some((token, cached_at)) = cache.as_ref() {
                if cached_at.elapsed().as_secs() < CACHE_TTL_SECS {
                    return Ok(token.clone());
                }
            }
        }

        let shared = {
            let mut pending = PENDING_ASYNC_TOKEN_READ.lock().await;
            if let Some(existing) = pending.as_ref() {
                existing.clone()
            } else {
                let generation = CACHE_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
                let future = async move {
                    tokio::task::spawn_blocking(read_oauth_token_from_storage_uncached)
                        .await
                        .map_err(|e| format!("Credential read task failed: {e}"))
                        .and_then(|result| result.map(|token| (generation, token)))
                }
                .boxed()
                .shared();
                *pending = Some(future.clone());
                future
            }
        };

        let result = shared.await;

        let mut pending = PENDING_ASYNC_TOKEN_READ.lock().await;
        *pending = None;

        match result {
            Ok((generation, token)) => {
                if generation == CACHE_GENERATION.load(std::sync::atomic::Ordering::Relaxed) {
                    cache_token(&token);
                    return Ok(token);
                }
                continue;
            }
            Err(err) => {
                let stale_cached = CACHED_TOKEN
                    .read()
                    .ok()
                    .and_then(|cache| cache.as_ref().map(|(token, _)| token.clone()));
                if let Some(token) = stale_cached {
                    tracing::warn!(
                        "Async credential read failed; serving stale cached token: {}",
                        err
                    );
                    cache_token(&token);
                    return Ok(token);
                }
                return Err(err);
            }
        }
    }
}

/// Handle a 401: re-read storage first in case another process already refreshed,
/// otherwise attempt a forced refresh with our refresh token.
pub async fn recover_from_401(failed_access_token: &str) -> Result<Option<OAuthToken>, String> {
    let key = failed_access_token.to_string();
    let shared = {
        let mut pending = PENDING_401_RECOVERY.lock().await;
        if let Some(existing) = pending.get(&key) {
            existing.clone()
        } else {
            let failed_access_token = key.clone();
            let future = async move { recover_from_401_impl(&failed_access_token).await }
                .boxed()
                .shared();
            pending.insert(key.clone(), future.clone());
            future
        }
    };

    let result = shared.await;

    let mut pending = PENDING_401_RECOVERY.lock().await;
    pending.remove(&key);
    result
}

async fn recover_from_401_impl(failed_access_token: &str) -> Result<Option<OAuthToken>, String> {
    invalidate_cache();

    if let Ok(token) = read_oauth_token_from_storage_async().await {
        if token.access_token != failed_access_token && is_token_valid(&token) {
            cache_token(&token);
            return Ok(Some(token));
        }
        if token.refresh_token.is_none() {
            return Ok(None);
        }
        let refreshed = refresh_token_if_needed(&token, true).await?;
        return Ok(Some(refreshed));
    }

    Ok(None)
}

/// Returns a token that is valid enough for an API call, refreshing if needed.
pub async fn get_oauth_token_fresh() -> Result<OAuthToken, String> {
    let token = get_oauth_token()?;
    if is_token_valid(&token) {
        return Ok(token);
    }
    refresh_token_if_needed(&token, false).await
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
            let buffer_ms = 5 * 60 * 1000;
            now_ms + buffer_ms < expires_at
        }
        None => true,
    }
}

/// Invalidate the cached token (e.g., after a 401 response).
pub fn invalidate_cache() {
    if let Ok(mut cache) = CACHED_TOKEN.write() {
        *cache = None;
    }
    CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut last_seen) = LAST_CREDENTIALS_MTIME.write() {
        *last_seen = None;
    }
    if let Ok(mut pending) = PENDING_ASYNC_TOKEN_READ.try_lock() {
        *pending = None;
    }
}

async fn refresh_token_if_needed(token: &OAuthToken, force: bool) -> Result<OAuthToken, String> {
    if !force && is_token_valid(token) {
        return Ok(token.clone());
    }

    let _guard = REFRESH_LOCK.lock().await;
    maybe_invalidate_cache_if_disk_changed();
    let _file_guard = acquire_refresh_file_lock().await?;

    if let Ok(current) = read_oauth_token_from_storage_async().await {
        if current.access_token != token.access_token && is_token_valid(&current) {
            cache_token(&current);
            return Ok(current);
        }
    }

    let refresh_token = token
        .refresh_token
        .clone()
        .ok_or_else(|| "OAuth token expired and no refresh token is available.".to_string())?;

    let refreshed = refresh_via_oauth_api(token, &refresh_token).await?;
    match persist_token_update(token, &refreshed)? {
        PersistOutcome::Updated => {
            cache_token(&refreshed);
            Ok(refreshed)
        }
        PersistOutcome::KeptNewerToken(current) => {
            cache_token(&current);
            Ok(current)
        }
    }
}

async fn refresh_via_oauth_api(
    existing: &OAuthToken,
    refresh_token: &str,
) -> Result<OAuthToken, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let runtime = oauth_runtime_config()?;
    let response = client
        .post(&runtime.token_url)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": runtime.client_id,
            "scope": DEFAULT_SCOPES,
        }))
        .send()
        .await
        .map_err(|e| format!("OAuth refresh request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("OAuth refresh failed ({status}): {body}"));
    }

    let payload: RefreshResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse OAuth refresh response: {e}"))?;

    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
        + payload.expires_in.saturating_mul(1000);

    Ok(OAuthToken {
        access_token: payload.access_token,
        refresh_token: payload
            .refresh_token
            .or_else(|| existing.refresh_token.clone()),
        expires_at: Some(expires_at),
        subscription_type: existing.subscription_type.clone(),
        rate_limit_tier: existing.rate_limit_tier.clone(),
    })
}

fn read_oauth_token_from_storage_uncached() -> Result<OAuthToken, String> {
    let (_, token) = read_storage_entry()?;
    Ok(token)
}

fn read_storage_entry() -> Result<(CredentialSource, OAuthToken), String> {
    if cfg!(target_os = "macos") {
        let account = keychain_account_name();
        for service in keychain_service_candidates() {
            if let Ok(json_str) = read_keychain_entry(&service, &account) {
                let token = parse_storage_json(&json_str)?;
                return Ok((CredentialSource::Keychain { service, account }, token));
            }
        }
    }

    let (path, content) = read_credentials_file_raw()?;
    let token = parse_storage_json(&content)?;
    Ok((CredentialSource::File { path }, token))
}

fn read_keychain_entry(service: &str, account: &str) -> Result<String, String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-a", account, "-s", service, "-w"])
        .output()
        .map_err(|e| format!("Failed to run security command: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_credentials_file_raw() -> Result<(String, String), String> {
    let path = credentials_file_path();
    let content = std::fs::read_to_string(&path)
        .map_err(|_| "No credentials found. Run `claude /login` first.".to_string())?;
    Ok((path, content))
}

fn parse_storage_json(json_str: &str) -> Result<OAuthToken, String> {
    let data: SecureStorageData =
        serde_json::from_str(json_str).map_err(|e| format!("Failed to parse credentials: {e}"))?;

    let oauth = data.claude_ai_oauth.ok_or("No OAuth data in credentials")?;

    let access_token = oauth
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or("No access token found. Run `claude /login` first.")?;

    Ok(OAuthToken {
        access_token,
        refresh_token: oauth.refresh_token.filter(|t| !t.is_empty()),
        expires_at: oauth.expires_at,
        subscription_type: oauth.subscription_type,
        rate_limit_tier: oauth.rate_limit_tier,
    })
}

fn persist_token_update(
    previous: &OAuthToken,
    refreshed: &OAuthToken,
) -> Result<PersistOutcome, String> {
    let (source, raw) = read_storage_entry_raw()?;
    let mut data: SecureStorageData = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse credentials for persistence: {e}"))?;
    let mut oauth = data.claude_ai_oauth.unwrap_or(KeychainOAuth {
        access_token: None,
        refresh_token: None,
        expires_at: None,
        subscription_type: None,
        rate_limit_tier: None,
        scopes: None,
        extra: serde_json::Map::new(),
    });

    if oauth.access_token.as_deref() == Some(previous.access_token.as_str())
        || oauth.access_token.is_none()
    {
        oauth.access_token = Some(refreshed.access_token.clone());
        oauth.refresh_token = refreshed.refresh_token.clone();
        oauth.expires_at = refreshed.expires_at;
        oauth.subscription_type = refreshed
            .subscription_type
            .clone()
            .or(oauth.subscription_type);
        oauth.rate_limit_tier = refreshed.rate_limit_tier.clone().or(oauth.rate_limit_tier);
        if oauth.scopes.is_none() {
            oauth.scopes = Some(DEFAULT_SCOPES.split(' ').map(|s| s.to_string()).collect());
        }
        data.claude_ai_oauth = Some(oauth);
        let serialized = serde_json::to_string(&data)
            .map_err(|e| format!("Failed to serialize refreshed credentials: {e}"))?;
        write_storage_entry(&source, &serialized)?;
        return Ok(PersistOutcome::Updated);
    }

    let current = parse_storage_json(&raw)?;
    Ok(PersistOutcome::KeptNewerToken(current))
}

fn read_storage_entry_raw() -> Result<(CredentialSource, String), String> {
    // Test-only escape hatch: when set, skip the system keychain so unit tests
    // never read (or, worse, write to) the user's real credentials. Production
    // callers don't set this variable.
    let skip_keychain = std::env::var("CLAUDE_GATEWAY_SKIP_KEYCHAIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if cfg!(target_os = "macos") && !skip_keychain {
        let account = keychain_account_name();
        for service in keychain_service_candidates() {
            if let Ok(json_str) = read_keychain_entry(&service, &account) {
                return Ok((CredentialSource::Keychain { service, account }, json_str));
            }
        }
    }

    let (path, content) = read_credentials_file_raw()?;
    Ok((CredentialSource::File { path }, content))
}

fn write_storage_entry(source: &CredentialSource, value: &str) -> Result<(), String> {
    match source {
        CredentialSource::File { path } => {
            std::fs::write(path, value)
                .map_err(|e| format!("Failed to update credentials file: {e}"))?;
            refresh_last_credentials_mtime(path);
            Ok(())
        }
        CredentialSource::Keychain { service, account } => {
            let hex_value = hex_encode(value.as_bytes());
            let interactive_command = format!(
                "add-generic-password -U -a \"{}\" -s \"{}\" -X \"{}\"\n",
                shell_escape_double_quotes(account),
                shell_escape_double_quotes(service),
                hex_value
            );

            let output = if interactive_command.len() <= SECURITY_STDIN_LINE_LIMIT {
                Command::new("security")
                    .args(["-i"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        if let Some(mut stdin) = child.stdin.take() {
                            stdin.write_all(interactive_command.as_bytes())?;
                        }
                        child.wait_with_output()
                    })
                    .map_err(|e| format!("Failed to update macOS keychain: {e}"))?
            } else {
                Command::new("security")
                    .args([
                        "add-generic-password",
                        "-U",
                        "-a",
                        account,
                        "-s",
                        service,
                        "-X",
                        &hex_value,
                    ])
                    .output()
                    .map_err(|e| format!("Failed to update macOS keychain: {e}"))?
            };

            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Ok(())
        }
    }
}

fn cache_token(token: &OAuthToken) {
    if let Ok(mut cache) = CACHED_TOKEN.write() {
        *cache = Some((token.clone(), std::time::Instant::now()));
    }
}

fn maybe_invalidate_cache_if_disk_changed() {
    let path = credentials_file_path();
    let metadata = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    let modified = match metadata.modified() {
        Ok(mtime) => mtime,
        Err(_) => return,
    };

    let mut changed = false;
    if let Ok(mut last_seen) = LAST_CREDENTIALS_MTIME.write() {
        changed = last_seen.map(|prev| prev != modified).unwrap_or(false);
        *last_seen = Some(modified);
    }
    if changed {
        invalidate_cache();
    }
}

fn refresh_last_credentials_mtime(path: &str) {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok();
    if let (Some(modified), Ok(mut last_seen)) = (modified, LAST_CREDENTIALS_MTIME.write()) {
        *last_seen = Some(modified);
    }
}

fn credentials_file_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_dir =
        std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| format!("{home}/.claude"));
    format!("{config_dir}/.credentials.json")
}

fn keychain_account_name() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude-code-user".to_string())
}

fn keychain_service_candidates() -> Vec<String> {
    let mut names = vec![primary_keychain_service_name()];
    let legacy = "Claude Code-credentials".to_string();
    if !names.contains(&legacy) {
        names.push(legacy);
    }
    names
}

fn primary_keychain_service_name() -> String {
    let config_dir = config_home_dir();
    let service_suffix = oauth_runtime_file_suffix();
    let is_default_dir = std::env::var("CLAUDE_CONFIG_DIR").is_err();

    let dir_hash = if is_default_dir {
        String::new()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(config_dir.as_bytes());
        let digest = hasher.finalize();
        format!("-{}", &hex_encode(&digest)[..8])
    };

    format!("Claude Code{service_suffix}-credentials{dir_hash}")
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

fn config_home_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| format!("{home}/.claude"))
}

struct OAuthRuntimeConfig {
    token_url: String,
    client_id: String,
}

fn oauth_runtime_config() -> AuthResult<OAuthRuntimeConfig> {
    if let Some(base) = validated_custom_oauth_base()? {
        return Ok(OAuthRuntimeConfig {
            token_url: format!("{base}/v1/oauth/token"),
            client_id: std::env::var("CLAUDE_CODE_OAUTH_CLIENT_ID")
                .unwrap_or_else(|_| "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string()),
        });
    }

    let client_id_override = std::env::var("CLAUDE_CODE_OAUTH_CLIENT_ID").ok();
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") && env_truthy("USE_LOCAL_OAUTH") {
        let api = std::env::var("CLAUDE_LOCAL_OAUTH_API_BASE")
            .unwrap_or_else(|_| "http://localhost:8000".to_string());
        return Ok(OAuthRuntimeConfig {
            token_url: format!("{}/v1/oauth/token", api.trim_end_matches('/')),
            client_id: client_id_override
                .unwrap_or_else(|| "22422756-60c9-4084-8eb7-27705fd5cf9a".to_string()),
        });
    }
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") && env_truthy("USE_STAGING_OAUTH") {
        return Ok(OAuthRuntimeConfig {
            token_url: "https://platform.staging.ant.dev/v1/oauth/token".to_string(),
            client_id: client_id_override
                .unwrap_or_else(|| "22422756-60c9-4084-8eb7-27705fd5cf9a".to_string()),
        });
    }

    Ok(OAuthRuntimeConfig {
        token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
        client_id: client_id_override
            .unwrap_or_else(|| "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string()),
    })
}

fn oauth_runtime_file_suffix() -> &'static str {
    if std::env::var("CLAUDE_CODE_CUSTOM_OAUTH_URL").is_ok() {
        return "-custom-oauth";
    }
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") && env_truthy("USE_LOCAL_OAUTH") {
        return "-local-oauth";
    }
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") && env_truthy("USE_STAGING_OAUTH") {
        return "-staging-oauth";
    }
    ""
}

fn validated_custom_oauth_base() -> AuthResult<Option<String>> {
    const ALLOWED_CUSTOM_OAUTH_BASE_URLS: &[&str] = &[
        "https://beacon.claude-ai.staging.ant.dev",
        "https://claude.fedstart.com",
        "https://claude-staging.fedstart.com",
    ];

    let Ok(base) = std::env::var("CLAUDE_CODE_CUSTOM_OAUTH_URL") else {
        return Ok(None);
    };
    let base = base.trim_end_matches('/').to_string();

    if !ALLOWED_CUSTOM_OAUTH_BASE_URLS
        .iter()
        .any(|allowed| *allowed == base)
    {
        return Err("CLAUDE_CODE_CUSTOM_OAUTH_URL is not an approved endpoint.".to_string());
    }

    Ok(Some(base))
}

async fn acquire_refresh_file_lock() -> Result<RefreshFileGuard, String> {
    let path = refresh_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create refresh lock dir: {e}"))?;
    }

    for attempt in 0..=MAX_REFRESH_LOCK_RETRIES {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(RefreshFileGuard { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                reap_stale_refresh_lock(&path);
                if attempt == MAX_REFRESH_LOCK_RETRIES {
                    return Err("Timed out waiting for OAuth refresh lock".to_string());
                }
                tokio::time::sleep(Duration::from_millis(1000 + (attempt as u64 * 250))).await;
            }
            Err(e) => return Err(format!("Failed to acquire OAuth refresh lock: {e}")),
        }
    }

    Err("Timed out waiting for OAuth refresh lock".to_string())
}

fn refresh_lock_path() -> PathBuf {
    PathBuf::from(config_home_dir()).join(".oauth-refresh.lock")
}

struct RefreshFileGuard {
    path: PathBuf,
}

impl Drop for RefreshFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn reap_stale_refresh_lock(path: &PathBuf) {
    let metadata = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    let modified = match metadata.modified() {
        Ok(mtime) => mtime,
        Err(_) => return,
    };
    let age = match SystemTime::now().duration_since(modified) {
        Ok(age) => age,
        Err(_) => return,
    };
    if age.as_secs() > REFRESH_LOCK_STALE_SECS {
        let _ = std::fs::remove_file(path);
    }
}

fn shell_escape_double_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the global env lock; if a prior test panicked while holding it
    /// we recover the guard from the poison error rather than cascading the
    /// failure across every remaining test.
    fn env_lock() -> MutexGuard<'static, ()> {
        match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }

    /// Combined guard for an auth test: holds ENV_LOCK *and* forces the file
    /// credential path (via CLAUDE_GATEWAY_SKIP_KEYCHAIN=1) so the test is
    /// fully isolated from the developer's real Keychain entry.
    struct AuthTestGuard {
        _lock: MutexGuard<'static, ()>,
        _skip: EnvGuard,
    }

    impl AuthTestGuard {
        fn new() -> Self {
            Self {
                _lock: env_lock(),
                _skip: EnvGuard::set("CLAUDE_GATEWAY_SKIP_KEYCHAIN", "1"),
            }
        }
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn unset(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(ref old) = self.old {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn parse_storage_json_reads_refresh_token() {
        let token = parse_storage_json(
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":123}}"#,
        )
        .unwrap();
        assert_eq!(token.access_token, "a");
        assert_eq!(token.refresh_token.as_deref(), Some("r"));
        assert_eq!(token.expires_at, Some(123));
    }

    #[test]
    fn token_validity_uses_expiry_buffer() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap();
        let valid = OAuthToken {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: Some(now_ms + 10 * 60 * 1000),
            subscription_type: None,
            rate_limit_tier: None,
        };
        let expired = OAuthToken {
            expires_at: Some(now_ms + 60_000),
            ..valid.clone()
        };

        assert!(is_token_valid(&valid));
        assert!(!is_token_valid(&expired));
    }

    #[test]
    fn default_keychain_service_name_matches_prod() {
        let _guard = AuthTestGuard::new();
        let _config = EnvGuard::unset("CLAUDE_CONFIG_DIR");
        let _custom = EnvGuard::unset("CLAUDE_CODE_CUSTOM_OAUTH_URL");
        let _local = EnvGuard::unset("USE_LOCAL_OAUTH");
        let _staging = EnvGuard::unset("USE_STAGING_OAUTH");
        let _user_type = EnvGuard::unset("USER_TYPE");
        assert_eq!(primary_keychain_service_name(), "Claude Code-credentials");
    }

    #[test]
    fn custom_oauth_url_must_be_allowlisted() {
        let _guard = AuthTestGuard::new();
        let _custom = EnvGuard::set("CLAUDE_CODE_CUSTOM_OAUTH_URL", "https://example.com");
        let err = validated_custom_oauth_base().unwrap_err();
        assert!(err.contains("approved endpoint"));
    }

    #[test]
    fn local_oauth_runtime_uses_local_endpoint() {
        let _guard = AuthTestGuard::new();
        let _user_type = EnvGuard::set("USER_TYPE", "ant");
        let _local = EnvGuard::set("USE_LOCAL_OAUTH", "1");
        let _api = EnvGuard::set("CLAUDE_LOCAL_OAUTH_API_BASE", "http://127.0.0.1:9999/");
        let cfg = oauth_runtime_config().unwrap();
        assert_eq!(cfg.token_url, "http://127.0.0.1:9999/v1/oauth/token");
    }

    #[test]
    fn persist_token_update_keeps_newer_stored_token() {
        let _guard = AuthTestGuard::new();
        let temp_dir =
            std::env::temp_dir().join(format!("claude-gateway-auth-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).unwrap();
        let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", temp_dir.to_str().unwrap());
        let path = temp_dir.join(".credentials.json");
        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"newer","refreshToken":"r2","expiresAt":999}}"#,
        )
        .unwrap();

        let previous = OAuthToken {
            access_token: "older".to_string(),
            refresh_token: Some("r1".to_string()),
            expires_at: Some(111),
            subscription_type: None,
            rate_limit_tier: None,
        };
        let refreshed = OAuthToken {
            access_token: "refreshed".to_string(),
            refresh_token: Some("r3".to_string()),
            expires_at: Some(222),
            subscription_type: None,
            rate_limit_tier: None,
        };

        match persist_token_update(&previous, &refreshed).unwrap() {
            PersistOutcome::KeptNewerToken(current) => {
                assert_eq!(current.access_token, "newer");
                assert_eq!(current.refresh_token.as_deref(), Some("r2"));
            }
            PersistOutcome::Updated => panic!("expected newer stored token to win"),
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn refresh_last_credentials_mtime_tracks_file_write() {
        let _guard = AuthTestGuard::new();
        let temp_dir =
            std::env::temp_dir().join(format!("claude-gateway-auth-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join(".credentials.json");
        fs::write(&path, "{}").unwrap();

        if let Ok(mut last_seen) = LAST_CREDENTIALS_MTIME.write() {
            *last_seen = None;
        }

        refresh_last_credentials_mtime(path.to_str().unwrap());

        let expected = fs::metadata(&path).unwrap().modified().unwrap();
        let actual = LAST_CREDENTIALS_MTIME
            .read()
            .unwrap()
            .expect("mtime should be cached");
        assert_eq!(actual, expected);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
