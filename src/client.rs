//! Clean-room HTTP REST client for `OpenViking` (`/health`, `/api/v1/*`, and `/mcp` probes).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
};
use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use url::Url;

use crate::config::ResolvedConfig;
use crate::wire::{
    OvAddMessageRequest, OvCommitRequest, OvContextSearchRequest, OvCreateSessionRequest,
    OvEnvelope, OvFindRequest, OvFsEntry, OvHealthResponse, OvMatchedItem, OvSearchResult,
    OvSystemStatusResult,
};

/// Errors returned by `OpenViking` HTTP operations.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("network or timeout error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("HTTP {status}: {message}")]
    HttpStatus {
        status: u16,
        code: Option<String>,
        message: String,
    },

    #[error("invalid JSON response: {0}")]
    InvalidJson(String),
}

impl ClientError {
    /// Returns `true` for transient failures (network errors, HTTP 429, or HTTP 5xx)
    /// that should be queued in the offline WAL for later replay.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Transport(_) => true,
            ClientError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            ClientError::InvalidJson(_) => false,
        }
    }

    /// Returns the upstream HTTP status code when available.
    #[must_use]
    pub fn status_code(&self) -> Option<u16> {
        match self {
            ClientError::HttpStatus { status, .. } => Some(*status),
            ClientError::Transport(_) | ClientError::InvalidJson(_) => None,
        }
    }
}

/// Measured outcome of a `GET /health` check.
#[derive(Debug, Clone)]
pub struct HealthProbe {
    pub response: OvHealthResponse,
    pub latency_ms: u128,
}

/// Outcome of a diagnostic `POST /mcp` handshake check.
#[derive(Debug, Clone)]
pub struct McpProbeResult {
    pub ok: bool,
    pub status_code: Option<u16>,
    pub has_session_id: bool,
    pub is_forbidden_root_key: bool,
    pub error: Option<String>,
}

/// `OpenViking` HTTP REST client.
#[derive(Clone)]
pub struct OvClient {
    http: Client,
    cfg: ResolvedConfig,
}

impl OvClient {
    /// Constructs a new `OvClient` backed by `rustls-tls`.
    pub fn try_new(cfg: ResolvedConfig) -> Result<Self, ClientError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(5_000)))
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()?;
        Ok(Self { http, cfg })
    }

    /// Builds the standard `OpenViking` authentication and actor headers for this configuration.
    #[must_use]
    pub fn default_headers(&self) -> HeaderMap {
        build_ov_headers(&self.cfg)
    }

    /// Probes `GET /health` and measures round-trip latency.
    pub async fn health(&self, timeout: Duration) -> Result<HealthProbe, ClientError> {
        let url = format!("{}/health", self.cfg.base_url);
        let start = Instant::now();
        let resp = self
            .http
            .get(&url)
            .headers(self.default_headers())
            .timeout(timeout)
            .send()
            .await?;
        let latency_ms = start.elapsed().as_millis();
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_http_error(status.as_u16(), &text));
        }
        let response: OvHealthResponse = serde_json::from_str(&text)
            .map_err(|err| ClientError::InvalidJson(format!("{err}: {text}")))?;
        Ok(HealthProbe {
            response,
            latency_ms,
        })
    }

    /// Queries `GET /api/v1/system/status` to verify authenticated tenant identity.
    pub async fn system_status(
        &self,
        timeout: Duration,
    ) -> Result<OvSystemStatusResult, ClientError> {
        let url = format!("{}/api/v1/system/status", self.cfg.base_url);
        let resp = self
            .http
            .get(&url)
            .headers(self.default_headers())
            .timeout(timeout)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_http_error(status.as_u16(), &text));
        }
        let env: OvEnvelope<OvSystemStatusResult> = serde_json::from_str(&text)
            .map_err(|err| ClientError::InvalidJson(format!("{err}: {text}")))?;
        if env.status.as_deref() == Some("error") {
            let msg = env
                .error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| "API status=error".to_string());
            return Err(ClientError::HttpStatus {
                status: status.as_u16(),
                code: env.error.and_then(|e| e.code),
                message: msg,
            });
        }
        Ok(env.result.unwrap_or_default())
    }

    /// Lists skill packages under `uri` (e.g. `viking://~/skills` or `viking://agent/skills`).
    pub async fn list_skills(
        &self,
        uri: &str,
        timeout: Duration,
    ) -> Result<Vec<OvFsEntry>, ClientError> {
        let mut endpoint = Url::parse(&format!("{}/api/v1/fs/ls", self.cfg.base_url))
            .map_err(|err| ClientError::InvalidJson(err.to_string()))?;
        endpoint
            .query_pairs_mut()
            .append_pair("uri", uri)
            .append_pair("include_abstract", "true")
            .append_pair("limit", "30");

        let resp = self
            .http
            .get(endpoint)
            .headers(self.default_headers())
            .timeout(timeout)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_http_error(status.as_u16(), &text));
        }
        let env: OvEnvelope<Vec<OvFsEntry>> = serde_json::from_str(&text)
            .map_err(|err| ClientError::InvalidJson(format!("{err}: {text}")))?;
        Ok(env.result.unwrap_or_default())
    }

    /// Performs context-assembled search via `POST /api/v1/search/search` (`mode: "context"`).
    pub async fn search_context(
        &self,
        query: &str,
        session_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, ClientError> {
        let exclude_uris: Vec<String> = self
            .cfg
            .recall_exclude_uris
            .iter()
            .take(200)
            .cloned()
            .collect();

        let req_body = OvContextSearchRequest {
            query: query.to_string(),
            mode: "context".to_string(),
            session_id: if session_id.is_empty() {
                None
            } else {
                Some(session_id.to_string())
            },
            dedup_turns: Some(self.cfg.recall_dedup_turns),
            limit: Some(self.cfg.recall_limit),
            score_threshold: Some(self.cfg.score_threshold),
            peer_scope: if self.cfg.recall_peer_scope == "actor" {
                Some("actor".to_string())
            } else {
                None
            },
            exclude_uris,
        };

        let url = format!("{}/api/v1/search/search", self.cfg.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .timeout(timeout)
            .json(&req_body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            let err = parse_http_error(status.as_u16(), &text);
            let lower = text.to_ascii_lowercase();
            if matches!(status.as_u16(), 400 | 422)
                && (lower.contains("mode") || lower.contains("extra"))
            {
                return self.fallback_find(query, timeout).await;
            }
            return Err(err);
        }

        let env: OvEnvelope<OvSearchResult> = serde_json::from_str(&text)
            .map_err(|err| ClientError::InvalidJson(format!("{err}: {text}")))?;
        if env.status.as_deref() == Some("error") {
            let msg = env
                .error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| "search returned error".to_string());
            return Err(ClientError::HttpStatus {
                status: status.as_u16(),
                code: env.error.and_then(|e| e.code),
                message: msg,
            });
        }

        let Some(result) = env.result else {
            return Ok(None);
        };

        Ok(render_search_result(&result))
    }

    async fn fallback_find(
        &self,
        query: &str,
        timeout: Duration,
    ) -> Result<Option<String>, ClientError> {
        let targets = ["viking://~/memories", "viking://~/skills"];
        let url = format!("{}/api/v1/search/find", self.cfg.base_url);
        let mut combined = Vec::new();

        for target in targets {
            let body = OvFindRequest {
                query: query.to_string(),
                target_uri: target.to_string(),
                limit: self.cfg.recall_limit,
                score_threshold: self.cfg.score_threshold,
            };
            let resp = self
                .http
                .post(&url)
                .headers(self.default_headers())
                .timeout(timeout)
                .json(&body)
                .send()
                .await?;
            if !resp.status().is_success() {
                continue;
            }
            if let Ok(env) = resp.json::<OvEnvelope<OvSearchResult>>().await {
                if let Some(res) = env.result {
                    combined.extend(res.memories);
                    combined.extend(res.resources);
                    combined.extend(res.skills);
                }
            }
        }

        if combined.is_empty() {
            return Ok(None);
        }
        Ok(format_matched_items(&combined))
    }

    /// Appends a message to `POST /api/v1/sessions/{session_id}/messages`,
    /// auto-creating the session if the server returns `404 Not Found`.
    pub async fn add_session_message(
        &self,
        session_id: &str,
        msg: &OvAddMessageRequest,
    ) -> Result<(), ClientError> {
        let encoded_id = urlencoding_segment(session_id);
        let url = format!(
            "{}/api/v1/sessions/{encoded_id}/messages",
            self.cfg.base_url
        );

        let first = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .json(msg)
            .send()
            .await?;
        let first_status = first.status();
        let first_text = first.text().await?;

        if first_status.is_success() && !envelope_is_error(&first_text) {
            return Ok(());
        }

        if first_status.as_u16() == 404 {
            let create_url = format!("{}/api/v1/sessions", self.cfg.base_url);
            let _ = self
                .http
                .post(&create_url)
                .headers(self.default_headers())
                .json(&OvCreateSessionRequest {
                    session_id: session_id.to_string(),
                })
                .send()
                .await;

            let retry = self
                .http
                .post(&url)
                .headers(self.default_headers())
                .json(msg)
                .send()
                .await?;
            let retry_status = retry.status();
            let retry_text = retry.text().await?;
            if retry_status.is_success() && !envelope_is_error(&retry_text) {
                return Ok(());
            }
            return Err(parse_http_error(retry_status.as_u16(), &retry_text));
        }

        Err(parse_http_error(first_status.as_u16(), &first_text))
    }

    /// Commits a session via `POST /api/v1/sessions/{session_id}/commit`.
    pub async fn commit_session(&self, session_id: &str) -> Result<(), ClientError> {
        let encoded_id = urlencoding_segment(session_id);
        let url = format!("{}/api/v1/sessions/{encoded_id}/commit", self.cfg.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .json(&OvCommitRequest {
                keep_recent_count: 0,
            })
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.is_success() && !envelope_is_error(&text) {
            return Ok(());
        }
        Err(parse_http_error(status.as_u16(), &text))
    }

    /// Performs a one-shot MCP `initialize` probe for `ov-agy doctor`.
    pub async fn probe_mcp_handshake(&self, timeout: Duration) -> McpProbeResult {
        let mut headers = self.default_headers();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Ok(v) = HeaderValue::from_str("2025-03-26") {
            headers.insert(HeaderName::from_static("mcp-protocol-version"), v);
        }

        let init_payload = json!({
            "jsonrpc": "2.0",
            "id": "ov-agy-doctor-probe",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "ov-agy-doctor",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });

        let resp = match self
            .http
            .post(&self.cfg.mcp_url)
            .headers(headers)
            .timeout(timeout)
            .json(&init_payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                return McpProbeResult {
                    ok: false,
                    status_code: None,
                    has_session_id: false,
                    is_forbidden_root_key: false,
                    error: Some(err.to_string()),
                };
            }
        };

        let code = resp.status().as_u16();
        let session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let body_text = resp.text().await.unwrap_or_default();

        if let Some(ref sid) = session_id {
            let mut del_headers = self.default_headers();
            if let Ok(hv) = HeaderValue::from_str(sid) {
                del_headers.insert(HeaderName::from_static("mcp-session-id"), hv);
            }
            let _ = self
                .http
                .delete(&self.cfg.mcp_url)
                .headers(del_headers)
                .timeout(Duration::from_millis(500))
                .send()
                .await;
        }

        if (200..300).contains(&code) {
            McpProbeResult {
                ok: true,
                status_code: Some(code),
                has_session_id: session_id.is_some(),
                is_forbidden_root_key: false,
                error: None,
            }
        } else {
            McpProbeResult {
                ok: false,
                status_code: Some(code),
                has_session_id: session_id.is_some(),
                is_forbidden_root_key: code == 403,
                error: Some(if body_text.trim().is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code}: {}", body_text.trim())
                }),
            }
        }
    }
}

/// Builds `OpenViking` HTTP headers (`Authorization`, `X-API-Key`, `X-OpenViking-*`).
#[must_use]
pub fn build_ov_headers(cfg: &ResolvedConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let ua = format!(
        "openviking-memory-antigravity/{}",
        env!("CARGO_PKG_VERSION")
    );
    if let Ok(v) = HeaderValue::from_str(&ua) {
        headers.insert(USER_AGENT, v);
    }

    if !cfg.api_key.trim().is_empty() {
        let key_trimmed = cfg.api_key.trim();
        if let Ok(bearer) = HeaderValue::from_str(&format!("Bearer {key_trimmed}")) {
            headers.insert(AUTHORIZATION, bearer);
        }
        if let Ok(raw_key) = HeaderValue::from_str(key_trimmed) {
            headers.insert(HeaderName::from_static("x-api-key"), raw_key);
        }
    }

    if !cfg.account.trim().is_empty() {
        if let Ok(v) = HeaderValue::from_str(cfg.account.trim()) {
            headers.insert(HeaderName::from_static("x-openviking-account"), v);
        }
    }

    if !cfg.user.trim().is_empty() {
        if let Ok(v) = HeaderValue::from_str(cfg.user.trim()) {
            headers.insert(HeaderName::from_static("x-openviking-user"), v);
        }
    }

    if !cfg.peer_id.trim().is_empty() {
        if let Ok(v) = HeaderValue::from_str(cfg.peer_id.trim()) {
            headers.insert(HeaderName::from_static("x-openviking-actor-peer"), v);
        }
    }

    headers
}

fn render_search_result(res: &OvSearchResult) -> Option<String> {
    if let Some(ref stats) = res.stats {
        if stats
            .rewrite
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case("no_relevant"))
        {
            return None;
        }
    }

    if let Some(ref digest) = res.digest {
        let trimmed = digest.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Some(ref rendered) = res.rendered {
        let trimmed = rendered.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if !res.entries.is_empty() {
        let mut lines = Vec::with_capacity(res.entries.len());
        for entry in &res.entries {
            let cat = entry.category.as_deref().unwrap_or("memory");
            let body = entry
                .text
                .as_deref()
                .or(entry.content.as_deref())
                .or(entry.abstract_text.as_deref())
                .map_or("", str::trim);
            if body.is_empty() {
                lines.push(format!("- [{cat}] {}", entry.uri));
            } else {
                lines.push(format!("- [{cat}] {}: {body}", entry.uri));
            }
        }
        let joined = lines.join("\n");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }

    let mut list_items =
        Vec::with_capacity(res.memories.len() + res.resources.len() + res.skills.len());
    list_items.extend(res.memories.iter().cloned());
    list_items.extend(res.resources.iter().cloned());
    list_items.extend(res.skills.iter().cloned());
    format_matched_items(&list_items)
}

fn format_matched_items(items: &[OvMatchedItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for item in items {
        if item.uri.trim().is_empty() {
            continue;
        }
        let kind = item
            .context_type
            .as_deref()
            .or(item.category.as_deref())
            .unwrap_or("memory");
        let summary = item
            .content
            .as_deref()
            .or(item.abstract_text.as_deref())
            .or(item.overview.as_deref())
            .map_or("", str::trim);
        if summary.is_empty() {
            lines.push(format!("- [{kind}] {}", item.uri));
        } else {
            lines.push(format!("- [{kind}] {}: {summary}", item.uri));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn envelope_is_error(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("status")?.as_str().map(|s| s == "error"))
        .unwrap_or(false)
}

fn parse_http_error(status: u16, text: &str) -> ClientError {
    if let Ok(val) = serde_json::from_str::<Value>(text) {
        if let Some(err_obj) = val.get("error").and_then(Value::as_object) {
            let code = err_obj
                .get("code")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let msg = err_obj
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| format!("HTTP {status}"), ToString::to_string);
            return ClientError::HttpStatus {
                status,
                code,
                message: msg,
            };
        }
    }
    let fallback = text.trim();
    ClientError::HttpStatus {
        status,
        code: None,
        message: if fallback.is_empty() {
            format!("HTTP {status}")
        } else {
            fallback.chars().take(300).collect()
        },
    }
}

fn urlencoding_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
