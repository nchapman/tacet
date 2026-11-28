use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::types::{
    RecordingBatchResolveRequest, RecordingBatchResolveResponse, ReleaseResolveRequest,
    ReleaseResolveResponse,
};

pub const DEFAULT_RESOLVER_URL: &str = "https://tacet-resolver.68a117ca.tryvamos.dev";
pub const DEFAULT_RESOLVER_API_KEY: &str = "tacet_629e43a3a878617048b0563ce56b632ff2d873a9eeb15d30";

#[derive(Debug, Clone)]
pub struct ResolverConfig {
    pub base_url: String,
    pub api_key: String,
    pub enabled: bool,
    pub request_timeout: Duration,
    pub min_request_gap: Duration,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_RESOLVER_URL.to_string(),
            api_key: DEFAULT_RESOLVER_API_KEY.to_string(),
            enabled: true,
            request_timeout: Duration::from_secs(15),
            // 250ms spacing = max 240 req/min, safely under the 300/min album limit.
            min_request_gap: Duration::from_millis(250),
        }
    }
}

impl ResolverConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(disabled) = std::env::var("TACET_RESOLVER_DISABLED") {
            let v = disabled.trim();
            if v == "1" || v.eq_ignore_ascii_case("true") {
                cfg.enabled = false;
            }
        }
        if let Ok(url) = std::env::var("TACET_RESOLVER_URL") {
            let trimmed = url.trim().trim_end_matches('/').to_string();
            if trimmed.is_empty() {
                cfg.enabled = false;
            } else {
                cfg.base_url = trimmed;
            }
        }
        if let Ok(key) = std::env::var("TACET_RESOLVER_API_KEY") {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                cfg.api_key = trimmed.to_string();
            }
        }
        cfg
    }
}

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("resolver rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Duration },
    #[error("resolver HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("resolver transport error: {0}")]
    Transport(String),
    #[error("resolver decode error: {0}")]
    Decode(String),
}

impl ResolverError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport(_) => true,
            Self::HttpStatus { status, .. } => *status >= 500 || *status == 408,
            Self::Decode(_) => false,
        }
    }
}

pub struct ResolverClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl ResolverClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, timeout: Duration) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(&format!("tacet/{}", env!("CARGO_PKG_VERSION")))
                .unwrap_or_else(|_| HeaderValue::from_static("tacet")),
        );
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_string(),
            api_key: api_key.into().trim().to_string(),
            http,
        }
    }

    pub fn from_config(cfg: &ResolverConfig) -> Self {
        Self::new(&cfg.base_url, &cfg.api_key, cfg.request_timeout)
    }

    pub async fn health(&self) -> Result<(), ResolverError> {
        let url = format!("{}/v1/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ResolverError::Transport(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(ResolverError::HttpStatus { status, body })
        }
    }

    pub async fn resolve_release(
        &self,
        req: &ReleaseResolveRequest,
    ) -> Result<ReleaseResolveResponse, ResolverError> {
        self.post_json("/v1/resolve/release", req).await
    }

    pub async fn resolve_recordings(
        &self,
        req: &RecordingBatchResolveRequest,
    ) -> Result<RecordingBatchResolveResponse, ResolverError> {
        self.post_json("/v1/resolve/recordings", req).await
    }

    async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Resp, ResolverError> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self
            .http
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| ResolverError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            let retry_after = parse_retry_after(resp.headers());
            return Err(ResolverError::RateLimited { retry_after });
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ResolverError::Transport(e.to_string()))?;

        if !status.is_success() {
            let snippet: String = text.chars().take(256).collect();
            return Err(ResolverError::HttpStatus {
                status: status.as_u16(),
                body: snippet,
            });
        }

        serde_json::from_str(&text).map_err(|e| ResolverError::Decode(e.to_string()))
    }
}

pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Duration {
    let secs = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(5);
    Duration::from_secs(secs.clamp(1, 120))
}
