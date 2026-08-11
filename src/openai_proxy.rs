use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};

use crate::error::GatewayError;

pub struct OpenAiProxyState {
    pub client: reqwest::Client,
    pub api_key: String,
    pub base_url: String,
    pub total_requests: AtomicU64,
}

impl OpenAiProxyState {
    pub fn from_env() -> Option<Arc<Self>> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }

        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com".to_string());

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Some(Arc::new(Self {
            client,
            api_key,
            base_url,
            total_requests: AtomicU64::new(0),
        }))
    }
}

fn auth_headers(api_key: &str) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key))
            .map_err(|e| GatewayError::Internal(format!("invalid OPENAI_API_KEY header: {}", e)))?,
    );
    Ok(headers)
}

pub async fn responses_sync(
    state: &OpenAiProxyState,
    body: serde_json::Value,
) -> Result<(serde_json::Value, u16), GatewayError> {
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let headers = auth_headers(&state.api_key)?;
    let url = format!("{}/v1/responses", state.base_url.trim_end_matches('/'));

    let resp = state
        .client
        .post(url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            GatewayError::CliConnection(format!("OpenAI responses request failed: {}", e))
        })?;

    let status = resp.status().as_u16();
    let json = resp.json::<serde_json::Value>().await.map_err(|e| {
        GatewayError::Internal(format!("OpenAI responses JSON decode failed: {}", e))
    })?;
    Ok((json, status))
}

pub async fn responses_stream(
    state: &OpenAiProxyState,
    body: serde_json::Value,
) -> Result<(reqwest::Response, u16), GatewayError> {
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let headers = auth_headers(&state.api_key)?;
    let url = format!("{}/v1/responses", state.base_url.trim_end_matches('/'));

    let resp = state
        .client
        .post(url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            GatewayError::CliConnection(format!("OpenAI streaming request failed: {}", e))
        })?;
    let status = resp.status().as_u16();
    Ok((resp, status))
}
