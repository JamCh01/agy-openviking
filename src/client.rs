//! Clean-room HTTP REST client for `OpenViking` (`/health`, `/api/v1/*`, and `/mcp` probes).

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
};
use reqwest::{Certificate, Client, ClientBuilder, RequestBuilder, Response};
use serde_json::{json, Value};
use thiserror::Error;
use url::Url;

use crate::config::ResolvedConfig;
use crate::wire::{
    OvAddMessageRequest, OvBatchAddMessagesRequest, OvCommitRequest, OvContextSearchRequest,
    OvCreateSessionRequest, OvEnvelope, OvFindRequest, OvFsEntry, OvHealthResponse, OvMatchedItem,
    OvSearchResult, OvSessionMeta, OvSkillItem, OvSkillList, OvSystemStatusResult,
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

    /// A 2xx reply that is not an `OpenViking` JSON envelope, such as a gateway or SSO login
    /// page. The write did not reach `OpenViking`, so it must not count as delivered.
    #[error("HTTP {status} without an OpenViking JSON envelope: {preview}")]
    UnexpectedBody { status: u16, preview: String },
}

impl ClientError {
    /// Returns `true` when a failed session write should be preserved in the offline WAL
    /// (network/timeout errors, HTTP 401/403 credential errors awaiting remediation, HTTP 429,
    /// 5xx, or a 2xx page from something other than `OpenViking`).
    #[must_use]
    pub fn should_queue_to_wal(&self) -> bool {
        match self {
            ClientError::Transport(_) | ClientError::UnexpectedBody { .. } => true,
            ClientError::HttpStatus { status, .. } => {
                matches!(*status, 401 | 403 | 429) || *status >= 500
            }
            ClientError::InvalidJson(_) => false,
        }
    }

    /// Returns the upstream HTTP status code when available.
    #[must_use]
    pub fn status_code(&self) -> Option<u16> {
        match self {
            ClientError::HttpStatus { status, .. } => Some(*status),
            ClientError::Transport(_)
            | ClientError::InvalidJson(_)
            | ClientError::UnexpectedBody { .. } => None,
        }
    }
}

/// Measured outcome of a `GET /health` check.
#[derive(Debug, Clone)]
pub struct HealthProbe {
    pub response: OvHealthResponse,
    pub latency_ms: u128,
}

/// `OpenViking` HTTP REST client.
///
/// Hooks run once per prompt, and reading the macOS keychain costs about 120 ms, so requests
/// start on the bundled Mozilla roots plus the extra CA bundle. A certificate those cannot
/// verify retries once on a client that adds the operating system's trust store; later
/// requests of this process stay on it.
#[derive(Clone)]
pub struct OvClient {
    http: Client,
    with_system_store: Arc<OnceLock<Option<Client>>>,
    cfg: ResolvedConfig,
}

impl OvClient {
    /// Constructs a new `OvClient` on the shared rustls configuration.
    pub fn try_new(cfg: ResolvedConfig) -> Result<Self, ClientError> {
        let http = Self::configure(tls_client_builder(&cfg, false), &cfg).build()?;
        Ok(Self {
            http,
            with_system_store: Arc::new(OnceLock::new()),
            cfg,
        })
    }

    fn configure(builder: ClientBuilder, cfg: &ResolvedConfig) -> ClientBuilder {
        builder
            .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(5_000)))
            .timeout(Duration::from_millis(cfg.timeout_ms))
    }

    /// Sends `request`, retrying once with the system trust store after a certificate failure.
    async fn execute(&self, request: RequestBuilder) -> reqwest::Result<Response> {
        let request = request.build()?;
        if let Some(Some(upgraded)) = self.with_system_store.get() {
            return upgraded.execute(request).await;
        }
        let retry = request.try_clone();
        let result = self.http.execute(request).await;
        let Err(ref err) = result else {
            return result;
        };
        if !self.cfg.uses_https() || !is_certificate_error(err) {
            return result;
        }
        let upgraded = self.with_system_store.get_or_init(|| {
            let client = Self::configure(tls_client_builder(&self.cfg, true), &self.cfg).build();
            self.cfg.log_debug(
                "client",
                "system_trust_store",
                json!({ "error": client.as_ref().err().map(ToString::to_string) }),
            );
            client.ok()
        });
        match (upgraded, retry) {
            (Some(client), Some(request)) => client.execute(request).await,
            _ => result,
        }
    }

    /// Builds the standard `OpenViking` authentication and actor headers for this configuration.
    #[must_use]
    pub fn default_headers(&self) -> HeaderMap {
        build_ov_headers(&self.cfg, ActorPeer::Always)
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
            .send_on(self)
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
            .send_on(self)
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
    pub async fn list_entries(
        &self,
        uri: &str,
        recursive: bool,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<OvFsEntry>, ClientError> {
        let mut endpoint = Url::parse(&format!("{}/api/v1/fs/ls", self.cfg.base_url))
            .map_err(|err| ClientError::InvalidJson(err.to_string()))?;
        endpoint
            .query_pairs_mut()
            .append_pair("uri", uri)
            .append_pair("recursive", if recursive { "true" } else { "false" })
            .append_pair("include_abstract", "true")
            .append_pair("abs_limit", "512")
            .append_pair("node_limit", &limit.to_string());

        let resp = self
            .http
            .get(endpoint)
            .headers(self.default_headers())
            .timeout(timeout)
            .send_on(self)
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

    /// Reads the text of a startup profile when available.
    pub async fn read_profile(&self, uri: &str, timeout: Duration) -> Option<String> {
        let env = self
            .http
            .get(format!("{}/api/v1/content/read", self.cfg.base_url))
            .headers(self.default_headers())
            .query(&[("uri", uri)])
            .timeout(timeout)
            .send_on(self)
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<OvEnvelope<String>>()
            .await
            .ok()?;
        env.result
    }

    pub async fn latest_archive(
        &self,
        session: &str,
        token_budget: usize,
        timeout: Duration,
    ) -> Option<String> {
        let path = urlencoding_segment(session);
        let env = self
            .http
            .get(format!(
                "{}/api/v1/sessions/{path}/context",
                self.cfg.base_url
            ))
            .headers(self.default_headers())
            .query(&[("token_budget", token_budget.to_string())])
            .timeout(timeout)
            .send_on(self)
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<OvEnvelope<Value>>()
            .await
            .ok()?;
        env.result?
            .get("latest_archive_overview")?
            .as_str()
            .map(str::to_owned)
    }

    /// Lists both skill roots through `GET /api/v1/skills`; older servers without the endpoint
    /// return an error and the caller falls back to listing the roots.
    pub async fn list_skills(
        &self,
        node_limit: usize,
        timeout: Duration,
    ) -> Result<Vec<OvSkillItem>, ClientError> {
        let resp = self
            .http
            .get(format!("{}/api/v1/skills", self.cfg.base_url))
            .headers(self.default_headers())
            .query(&[("node_limit", node_limit.to_string())])
            .timeout(timeout)
            .send_on(self)
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_http_error(status.as_u16(), &text));
        }
        let env: OvEnvelope<OvSkillList> =
            serde_json::from_str(&text).map_err(|err| ClientError::InvalidJson(err.to_string()))?;
        Ok(env.result.unwrap_or_default().skills)
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
            purpose: "coding",
            quotas: self.cfg.explicit_recall_limit.map(coding_quotas),
            max_tokens: self.cfg.recall_max_tokens,
            rewrite: self.cfg.recall_rewrite.clone(),
            score_threshold: Some(self.cfg.score_threshold),
            peer_scope: if self.cfg.recall_peer_scope == "actor" {
                Some("actor".to_string())
            } else {
                None
            },
            exclude_uris,
            query_expansion: self.cfg.effective_query_expansion().map(str::to_string),
        };

        let url = format!("{}/api/v1/search/search", self.cfg.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .timeout(timeout)
            .json(&req_body)
            .send_on(self)
            .await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            let err = parse_http_error(status.as_u16(), &text);
            let lower = text.to_ascii_lowercase();
            if self.cfg.recall_peer_scope != "actor"
                && self.cfg.recall_exclude_uris.is_empty()
                && matches!(status.as_u16(), 400 | 422)
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
        let url = format!("{}/api/v1/search/find", self.cfg.base_url);
        let find = async |target: &str| -> Result<Vec<OvMatchedItem>, ClientError> {
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
                .send_on(self)
                .await?
                .error_for_status()?;
            let env = resp.json::<OvEnvelope<OvSearchResult>>().await?;
            let result = env.result.unwrap_or_default();
            let mut items = result.memories;
            items.extend(result.resources);
            items.extend(result.skills);
            Ok(items)
        };
        let (memories, skills) =
            tokio::join!(find("viking://~/memories"), find("viking://~/skills"));
        let mut items = match (memories, skills) {
            (Err(err), Err(_)) => return Err(err),
            (Ok(items), Err(_)) | (Err(_), Ok(items)) => items,
            (Ok(mut items), Ok(skills)) => {
                items.extend(skills);
                items
            }
        };
        items.truncate(self.cfg.recall_limit);
        Ok(format_matched_items(&items))
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
            .send_on(self)
            .await?;
        let first_status = first.status();
        let first_text = first.text().await?;

        if first_status.as_u16() != 404 {
            return write_outcome(first_status.as_u16(), &first_text);
        }

        let create_url = format!("{}/api/v1/sessions", self.cfg.base_url);
        let _ = self
            .http
            .post(&create_url)
            .headers(self.default_headers())
            .json(&OvCreateSessionRequest {
                session_id: session_id.to_string(),
            })
            .send_on(self)
            .await;

        let retry = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .json(msg)
            .send_on(self)
            .await?;
        let retry_status = retry.status();
        let retry_text = retry.text().await?;
        write_outcome(retry_status.as_u16(), &retry_text)
    }

    /// Sends up to 100 messages through `POST /api/v1/sessions/{session_id}/messages/batch`.
    /// `Ok(false)` means the server has no batch endpoint (404/405) and the caller should
    /// send one message at a time; that path also creates a missing session.
    pub async fn add_session_messages_batch(
        &self,
        session_id: &str,
        messages: &[OvAddMessageRequest],
    ) -> Result<bool, ClientError> {
        let encoded_id = urlencoding_segment(session_id);
        let url = format!(
            "{}/api/v1/sessions/{encoded_id}/messages/batch",
            self.cfg.base_url
        );
        let resp = self
            .http
            .post(&url)
            .headers(self.default_headers())
            .json(&OvBatchAddMessagesRequest { messages })
            .send_on(self)
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if matches!(status, 404 | 405) {
            return Ok(false);
        }
        write_outcome(status, &text).map(|()| true)
    }

    /// Reads `pending_tokens` and counters from `GET /api/v1/sessions/{session_id}`.
    pub async fn session_meta(&self, session_id: &str) -> Result<OvSessionMeta, ClientError> {
        let encoded_id = urlencoding_segment(session_id);
        let url = format!("{}/api/v1/sessions/{encoded_id}", self.cfg.base_url);
        let resp = self
            .http
            .get(&url)
            .headers(self.default_headers())
            .send_on(self)
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_http_error(status.as_u16(), &text));
        }
        let env: OvEnvelope<OvSessionMeta> =
            serde_json::from_str(&text).map_err(|err| ClientError::InvalidJson(err.to_string()))?;
        Ok(env.result.unwrap_or_default())
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
            .send_on(self)
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        write_outcome(status.as_u16(), &text)
    }
}

/// The HTTP client every component builds on: rustls with the bundled Mozilla roots, the
/// operating system's trust store, and the CA bundle from `OPENVIKING_CA_CERTS`. The store is
/// read only for `https` servers; reading the macOS keychain costs time a plain-HTTP local
/// server does not need.
pub fn http_client_builder(cfg: &ResolvedConfig) -> ClientBuilder {
    tls_client_builder(cfg, cfg.uses_https())
}

/// [`http_client_builder`] for a server other than the configured one (`setup` checks a new
/// url before saving it).
pub fn tls_client_builder(cfg: &ResolvedConfig, https: bool) -> ClientBuilder {
    // `SSL_CERT_FILE` / `SSL_CERT_DIR` replace the system store when set (rustls-native-certs).
    let mut builder = Client::builder().tls_built_in_native_certs(https);
    match extra_root_certificates(cfg) {
        Ok(certs) => {
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        Err(err) => cfg.log_debug("client", "ca_certs_error", json!({ "error": err })),
    }
    builder
}

/// The certificates of the extra CA bundle; empty when none is configured.
///
/// # Errors
///
/// The configured file cannot be read or holds no PEM certificate.
pub fn extra_root_certificates(cfg: &ResolvedConfig) -> Result<Vec<Certificate>, String> {
    let Some(path) = cfg.ca_certs.as_ref() else {
        return Ok(Vec::new());
    };
    let pem =
        std::fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let certs = Certificate::from_pem_bundle(&pem)
        .map_err(|err| format!("{} is not a PEM bundle: {err}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no PEM certificate", path.display()));
    }
    Ok(certs)
}

/// `RequestBuilder::send` routed through [`OvClient::execute`].
trait SendOn {
    async fn send_on(self, client: &OvClient) -> reqwest::Result<Response>;
}

impl SendOn for RequestBuilder {
    async fn send_on(self, client: &OvClient) -> reqwest::Result<Response> {
        client.execute(self).await
    }
}

/// A TLS failure that more trusted roots might fix (rustls reports it as an invalid peer
/// certificate, usually `UnknownIssuer`).
fn is_certificate_error(err: &reqwest::Error) -> bool {
    let mut source: Option<&dyn std::error::Error> = Some(err);
    while let Some(cause) = source {
        if cause
            .to_string()
            .to_ascii_lowercase()
            .contains("invalid peer certificate")
        {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Whether the actor-peer header goes on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorPeer {
    /// Hooks act for the workspace they run in, so they always name its peer.
    Always,
    /// A long-lived MCP proxy names the peer only for actor-scoped recall; broad recall spans
    /// every peer under the user, as the Codex and Claude Code proxies do.
    ActorScopeOnly,
}

/// Builds the `OpenViking` request headers. Only `Authorization: Bearer` carries the key
/// (`X-API-Key` would duplicate it into a header that logs rarely redact), and identity
/// headers go out only when `authMode` is `trusted`.
#[must_use]
pub fn build_ov_headers(cfg: &ResolvedConfig, actor_peer: ActorPeer) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let ua = format!(
        "openviking-memory-antigravity/{}",
        env!("CARGO_PKG_VERSION")
    );
    if let Ok(v) = HeaderValue::from_str(&ua) {
        headers.insert(USER_AGENT, v);
    }

    let key = cfg.api_key.trim();
    if !key.is_empty() {
        if let Ok(bearer) = HeaderValue::from_str(&format!("Bearer {key}")) {
            headers.insert(AUTHORIZATION, bearer);
        }
    }

    if cfg.auth_mode == "trusted" {
        for (name, value) in [
            ("x-openviking-account", &cfg.account),
            ("x-openviking-user", &cfg.user),
        ] {
            if let Ok(v) = HeaderValue::from_str(value.trim()) {
                if !value.trim().is_empty() {
                    headers.insert(HeaderName::from_static(name), v);
                }
            }
        }
    }

    let send_peer = match actor_peer {
        ActorPeer::Always => true,
        ActorPeer::ActorScopeOnly => cfg.recall_peer_scope == "actor",
    };
    if send_peer && !cfg.peer_id.trim().is_empty() {
        if let Ok(v) = HeaderValue::from_str(cfg.peer_id.trim()) {
            headers.insert(HeaderName::from_static("x-openviking-actor-peer"), v);
        }
    }

    for (name, value) in &cfg.extra_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    headers
}

fn coding_quotas(limit: usize) -> Value {
    // Match the coding preset and its one-slot floor for each source.
    let weights = [
        ("events", 1),
        ("entities", 2),
        ("preferences", 1),
        ("experiences", 1),
        ("resources", 3),
        ("skills", 2),
    ];
    let mut counts = [1_i64; 6];
    let slots = i64::try_from(limit).unwrap_or(50);
    for _ in weights.len()..limit {
        let mut best = 0;
        for i in 1..weights.len() {
            if slots * weights[i].1 - counts[i] * 10 > slots * weights[best].1 - counts[best] * 10 {
                best = i;
            }
        }
        counts[best] += 1;
    }
    Value::Object(
        weights
            .into_iter()
            .zip(counts)
            .map(|((key, _), n)| (key.to_owned(), Value::from(n)))
            .collect(),
    )
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

/// A write counts as delivered only when `OpenViking` itself answered 2xx with a JSON
/// envelope that is not `status: "error"`. Gateways and SSO proxies can answer 200 with an
/// HTML page, which must not advance the capture cursor.
fn write_outcome(status: u16, text: &str) -> Result<(), ClientError> {
    if !(200..300).contains(&status) {
        return Err(parse_http_error(status, text));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(envelope)) => {
            if envelope.get("status").and_then(Value::as_str) == Some("error") {
                Err(parse_http_error(status, text))
            } else {
                Ok(())
            }
        }
        _ => Err(ClientError::UnexpectedBody {
            status,
            preview: text.trim().chars().take(120).collect(),
        }),
    }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn cfg(extra: &[(&str, &str)]) -> ResolvedConfig {
        let temp = tempfile::TempDir::new().unwrap();
        let mut env: HashMap<String, String> = [
            ("HOME", temp.path().to_str().unwrap()),
            ("OPENVIKING_CREDENTIAL_SOURCE", "env"),
            ("OPENVIKING_URL", "http://127.0.0.1:1933"),
            ("OPENVIKING_API_KEY", "user-key"),
            ("OPENVIKING_ACCOUNT", "acct"),
            ("OPENVIKING_USER", "alice"),
            ("OPENVIKING_PEER_ID", "github.com-org-repo"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            env.insert((*k).to_string(), (*v).to_string());
        }
        ResolvedConfig::load_with_env(Some(temp.path()), &env)
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn headers_follow_auth_mode_peer_scope_and_extra_headers() {
        let trusted = build_ov_headers(&cfg(&[]), ActorPeer::Always);
        assert_eq!(header(&trusted, "authorization"), Some("Bearer user-key"));
        assert_eq!(
            header(&trusted, "x-api-key"),
            None,
            "the key travels only as Bearer"
        );
        assert_eq!(header(&trusted, "x-openviking-account"), Some("acct"));
        assert_eq!(
            header(&trusted, "x-openviking-actor-peer"),
            Some("github.com-org-repo")
        );

        let api_key = build_ov_headers(
            &cfg(&[("OPENVIKING_AUTH_MODE", "api_key")]),
            ActorPeer::Always,
        );
        assert_eq!(header(&api_key, "x-openviking-account"), None);
        assert_eq!(header(&api_key, "x-openviking-user"), None);

        let broad_mcp = build_ov_headers(&cfg(&[]), ActorPeer::ActorScopeOnly);
        assert_eq!(header(&broad_mcp, "x-openviking-actor-peer"), None);
        let actor_mcp = build_ov_headers(
            &cfg(&[("OPENVIKING_RECALL_PEER_SCOPE", "actor")]),
            ActorPeer::ActorScopeOnly,
        );
        assert_eq!(
            header(&actor_mcp, "x-openviking-actor-peer"),
            Some("github.com-org-repo")
        );

        let extra = build_ov_headers(
            &cfg(&[(
                "OPENVIKING_EXTRA_HEADERS",
                r#"{"CF-Access-Client-Id":"svc"}"#,
            )]),
            ActorPeer::Always,
        );
        assert_eq!(header(&extra, "cf-access-client-id"), Some("svc"));
    }
}
