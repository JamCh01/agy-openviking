//! Stdio <-> Streamable HTTP (`/mcp`) JSON-RPC proxy with SSE streaming, concurrent requests,
//! session recovery, and credential reload.
//!
//! `initialize` and notifications are forwarded in order on the reader task. Every request
//! with an id runs on its own task (at most `MAX_IN_FLIGHT`), so one slow `search` does not
//! hold up a parallel `read` or a cancellation; replies carry their ids and may complete out of
//! order. Whatever replaces the upstream session (404 recovery, a roots change, new
//! credentials) takes the connection's write lock, which waits for the requests still in
//! flight on the old session.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use eventsource_stream::EventStreamError;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Map, Value};
use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;

use crate::client::{self, ActorPeer};
use crate::config::ResolvedConfig;

const DEFAULT_MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const DELETE_SESSION_TIMEOUT: Duration = Duration::from_secs(1);
/// Requests forwarded at once, as in the Codex and Claude Code proxies.
const MAX_IN_FLIGHT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionHeader {
    Attach,
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyMode {
    ExpectReply,
    Notification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Initialize,
    Regular,
}

/// Where an upstream reply goes: to the host, or nowhere (replayed `initialize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Client,
    Discard,
}

/// Modification time and size of a credential file, `None` while it does not exist.
type FileStamp = Option<(SystemTime, u64)>;

/// The upstream session and the configuration it was opened with.
struct Conn {
    cfg: ResolvedConfig,
    session_id: Option<String>,
    protocol_version: String,
    /// Bumped by every (re)initialize, so concurrent failures replace a session only once.
    generation: u64,
}

struct Shared {
    http: Client,
    conn: RwLock<Conn>,
    cached_initialize: Mutex<Option<Value>>,
    cached_initialized: Mutex<Option<Value>>,
    reinit_seq: AtomicU64,
    out: tokio::sync::Mutex<io::Stdout>,
    workspace: Mutex<Option<PathBuf>>,
    /// The credential files as they were when the configuration was last read.
    credentials: Mutex<Vec<FileStamp>>,
}

enum UpstreamOutcome {
    Success,
    NotFoundExpiredSession,
    HttpError { status: u16, body: String },
    TransportError { message: String, timeout: bool },
}

struct Forwarded {
    outcome: UpstreamOutcome,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl Forwarded {
    fn failed(err: &anyhow::Error) -> Self {
        Self {
            outcome: UpstreamOutcome::TransportError {
                message: err.to_string(),
                timeout: false,
            },
            session_id: None,
            protocol_version: None,
        }
    }
}

/// Runs the newline-delimited stdio JSON-RPC bridge until `stdin` reaches EOF.
pub async fn run() -> Result<()> {
    let cfg = workspace_config(None);
    let http = client::http_client_builder(&cfg)
        .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(5_000)))
        .timeout(Duration::from_millis(cfg.timeout_ms.max(60_000)))
        .build()?;
    cfg.log_debug("mcp_proxy", "start", json!({ "mcp_url": cfg.mcp_url }));
    let shared = Arc::new(Shared {
        http,
        credentials: Mutex::new(credential_stamp(&cfg)),
        conn: RwLock::new(Conn {
            cfg,
            session_id: None,
            protocol_version: DEFAULT_MCP_PROTOCOL_VERSION.to_string(),
            generation: 0,
        }),
        cached_initialize: Mutex::new(None),
        cached_initialized: Mutex::new(None),
        reinit_seq: AtomicU64::new(1),
        out: tokio::sync::Mutex::new(io::stdout()),
        workspace: Mutex::new(None),
    });

    let mut lines = BufReader::new(io::stdin()).lines();
    let mut pending = VecDeque::new();
    let mut roots_supported = false;
    let mut refresh_roots = false;
    let mut roots_seq = 0_u64;
    let slots = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut tasks = JoinSet::new();
    loop {
        while tasks.try_join_next().is_some() {}
        let msg = if let Some(msg) = pending.pop_front() {
            msg
        } else {
            let Some(line) = lines.next_line().await? else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                emit(
                    &shared,
                    &jsonrpc_error(&Value::Null, -32700, "Parse error", None),
                )
                .await?;
                continue;
            };
            msg
        };
        let method = msg.get("method").and_then(Value::as_str);
        if method.is_none()
            && msg["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("ov-agy-roots-"))
        {
            continue; // A response that arrived after our roots deadline.
        }
        match method {
            Some("server/discover") => {
                roots_supported = msg["params"]["_meta"]
                    ["io.modelcontextprotocol/clientCapabilities"]["roots"]
                    .is_object();
                refresh_roots = roots_supported;
            }
            Some("initialize") => {
                roots_supported = msg["params"]["capabilities"]["roots"].is_object();
                refresh_roots = roots_supported;
            }
            Some("notifications/roots/list_changed") => {
                refresh_roots = roots_supported;
                continue;
            }
            _ => {}
        }
        // Older clients can serve roots only after initialize has returned.
        if refresh_roots && !matches!(method, Some("initialize" | "notifications/initialized")) {
            roots_seq += 1;
            let workspace = request_workspace(&mut lines, &shared, &mut pending, roots_seq).await?;
            switch_workspace(&shared, workspace).await?;
            refresh_roots = false;
        }

        let Some(meta) = inspect_client_message(&msg) else {
            emit(
                &shared,
                &jsonrpc_error(&Value::Null, -32600, "Invalid Request", None),
            )
            .await?;
            continue;
        };
        if meta.is_initialize {
            handle_initialize(&shared, &msg, meta.req_id).await?;
        } else if meta.reply_mode == ReplyMode::Notification {
            if meta.is_initialized_notification {
                if let Ok(mut cached) = shared.cached_initialized.lock() {
                    *cached = Some(msg.clone());
                }
            }
            let _ = forward_with_recovery(&shared, &msg, ReplyMode::Notification).await;
        } else {
            let permit = Arc::clone(&slots).acquire_owned().await?;
            let task_shared = Arc::clone(&shared);
            tasks.spawn(async move {
                let _permit = permit;
                let outcome =
                    forward_with_recovery(&task_shared, &msg, ReplyMode::ExpectReply).await;
                let _ = finalize_non_ok_outcome(&task_shared, outcome, &meta.req_id).await;
            });
        }
    }

    while tasks.join_next().await.is_some() {}
    let mut conn = shared.conn.write().await;
    delete_mcp_session(&shared, &mut conn).await;
    drop(conn);
    let _ = shared.out.lock().await.flush().await;
    Ok(())
}

/// Installed plugins start in `PLUGIN_ROOT`. Without a host root, use personal
/// configuration only. A standalone invocation still inherits its caller's cwd.
fn workspace_config(workspace: Option<&Path>) -> ResolvedConfig {
    let neutral = std::env::var_os("PLUGIN_ROOT").and_then(|_| dirs::home_dir());
    ResolvedConfig::load(workspace.or(neutral.as_deref()))
}

/// The files `ov config switch` or an edit rewrites when the credentials change.
fn credential_stamp(cfg: &ResolvedConfig) -> Vec<FileStamp> {
    let candidate = |loaded: &Option<PathBuf>, env: &str, default: &str| {
        loaded
            .clone()
            .or_else(|| {
                std::env::var_os(env)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| cfg.ov_home_dir.join(default))
    };
    [
        candidate(
            &cfg.cli_config_path,
            "OPENVIKING_CLI_CONFIG_FILE",
            "ovcli.conf",
        ),
        candidate(&cfg.ov_config_path, "OPENVIKING_CONFIG_FILE", "ov.conf"),
    ]
    .iter()
    .map(|path| {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    })
    .collect()
}

/// Everything the upstream session is bound to.
fn same_identity(a: &ResolvedConfig, b: &ResolvedConfig) -> bool {
    a.peer_id == b.peer_id
        && a.mcp_url == b.mcp_url
        && a.account == b.account
        && a.user == b.user
        && a.api_key == b.api_key
        && a.auth_mode == b.auth_mode
        && a.recall_peer_scope == b.recall_peer_scope
        && a.extra_headers == b.extra_headers
}

fn has_cached_initialize(shared: &Shared) -> bool {
    shared
        .cached_initialize
        .lock()
        .is_ok_and(|cached| cached.is_some())
}

/// Writes one JSON-RPC frame per line; concurrent replies never interleave.
async fn emit(shared: &Shared, val: &Value) -> Result<()> {
    let mut line = serde_json::to_string(val)?;
    line.push('\n');
    let mut out = shared.out.lock().await;
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

async fn request_workspace<R: AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    shared: &Shared,
    pending: &mut VecDeque<Value>,
    seq: u64,
) -> Result<Option<PathBuf>> {
    let id = format!("ov-agy-roots-{seq}");
    emit(
        shared,
        &json!({"jsonrpc":"2.0","id":id,"method":"roots/list"}),
    )
    .await?;
    let reply = async {
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                emit(
                    shared,
                    &jsonrpc_error(&Value::Null, -32700, "Parse error", None),
                )
                .await?;
                continue;
            };
            if msg.get("method").is_none() && msg["id"] == id {
                let path = msg["result"]["roots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find_map(|root| {
                        url::Url::parse(root["uri"].as_str()?)
                            .ok()?
                            .to_file_path()
                            .ok()
                    });
                return Ok(path);
            }
            pending.push_back(msg);
        }
        Ok(None)
    };
    tokio::time::timeout(Duration::from_secs(2), reply)
        .await
        .unwrap_or(Ok(None))
}

/// Applies the host's workspace. A different identity replaces the stateful upstream session.
async fn switch_workspace(shared: &Shared, workspace: Option<PathBuf>) -> Result<()> {
    let cfg = workspace_config(workspace.as_deref());
    if let Ok(mut current) = shared.workspace.lock() {
        current.clone_from(&workspace);
    }
    if let Ok(mut stamp) = shared.credentials.lock() {
        *stamp = credential_stamp(&cfg);
    }
    let mut conn = shared.conn.write().await;
    let reconnect = conn.session_id.is_some() && !same_identity(&conn.cfg, &cfg);
    if reconnect {
        delete_mcp_session(shared, &mut conn).await;
    }
    conn.cfg = cfg;
    if reconnect {
        reinitialize_session(shared, &mut conn).await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ClientMessageMeta {
    reply_mode: ReplyMode,
    req_id: Value,
    is_initialize: bool,
    is_initialized_notification: bool,
}

/// A request is a message with both a method and an id; the host's replies to server
/// requests carry an id but no method, and the server answers them with an empty 202.
fn inspect_client_message(msg: &Value) -> Option<ClientMessageMeta> {
    let request_id = |obj: &Map<String, Value>| {
        obj.contains_key("method")
            .then(|| obj.get("id").cloned())
            .flatten()
    };
    match msg {
        Value::Object(obj) => {
            let req_id = request_id(obj);
            let method = obj.get("method").and_then(Value::as_str).unwrap_or("");
            Some(ClientMessageMeta {
                reply_mode: if req_id.is_some() {
                    ReplyMode::ExpectReply
                } else {
                    ReplyMode::Notification
                },
                req_id: req_id.unwrap_or(Value::Null),
                is_initialize: method == "initialize",
                is_initialized_notification: method == "notifications/initialized",
            })
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let first_req_id = items
                .iter()
                .filter_map(Value::as_object)
                .find_map(request_id);
            Some(ClientMessageMeta {
                reply_mode: if first_req_id.is_some() {
                    ReplyMode::ExpectReply
                } else {
                    ReplyMode::Notification
                },
                req_id: first_req_id.unwrap_or(Value::Null),
                is_initialize: false,
                is_initialized_notification: false,
            })
        }
        _ => None,
    }
}

/// `initialize` opens a new upstream session. A 401/403 right after `ov config switch`
/// retries once with the credentials now on disk.
async fn handle_initialize(shared: &Shared, msg: &Value, req_id: Value) -> Result<()> {
    if let Ok(mut cached) = shared.cached_initialize.lock() {
        *cached = Some(msg.clone());
    }
    let mut conn = shared.conn.write().await;
    conn.session_id = None;
    conn.protocol_version = DEFAULT_MCP_PROTOCOL_VERSION.to_string();
    let mut forwarded = initialize_once(shared, &conn, msg, Delivery::Client).await;
    if matches!(
        forwarded.outcome,
        UpstreamOutcome::HttpError {
            status: 401 | 403,
            ..
        }
    ) {
        if let Some(cfg) = changed_credentials(shared, &conn.cfg) {
            cfg.log_debug("mcp_proxy", "credentials_reloaded", json!({}));
            conn.cfg = cfg;
            forwarded = initialize_once(shared, &conn, msg, Delivery::Client).await;
        }
    }
    if matches!(forwarded.outcome, UpstreamOutcome::Success) {
        apply_initialize(&mut conn, &forwarded);
    }
    drop(conn);
    finalize_non_ok_outcome(shared, forwarded.outcome, &req_id).await
}

async fn initialize_once(
    shared: &Shared,
    conn: &Conn,
    msg: &Value,
    delivery: Delivery,
) -> Forwarded {
    forward_once(
        shared,
        conn,
        msg,
        SessionHeader::Omit,
        ReplyMode::ExpectReply,
        RequestKind::Initialize,
        delivery,
    )
    .await
    .unwrap_or_else(|err| Forwarded::failed(&err))
}

fn apply_initialize(conn: &mut Conn, forwarded: &Forwarded) {
    conn.session_id.clone_from(&forwarded.session_id);
    if let Some(ref version) = forwarded.protocol_version {
        conn.protocol_version.clone_from(version);
    }
    conn.generation += 1;
}

/// One attempt on the current session; also returns the session generation it ran under.
async fn attempt(shared: &Shared, msg: &Value, reply_mode: ReplyMode) -> (UpstreamOutcome, u64) {
    let conn = shared.conn.read().await;
    let forwarded = forward_once(
        shared,
        &conn,
        msg,
        SessionHeader::Attach,
        reply_mode,
        RequestKind::Regular,
        Delivery::Client,
    )
    .await
    .unwrap_or_else(|err| Forwarded::failed(&err));
    (forwarded.outcome, conn.generation)
}

/// Forwards one client message, recovering once from an expired session (404) or from
/// credentials that changed on disk since they were read (401/403).
async fn forward_with_recovery(
    shared: &Shared,
    msg: &Value,
    reply_mode: ReplyMode,
) -> UpstreamOutcome {
    let (outcome, generation) = attempt(shared, msg, reply_mode).await;
    match outcome {
        UpstreamOutcome::NotFoundExpiredSession => {
            if !has_cached_initialize(shared) {
                return UpstreamOutcome::HttpError {
                    status: 404,
                    body: "MCP session expired".to_string(),
                };
            }
            if reinitialize_if_stale(shared, generation).await.is_err() {
                return UpstreamOutcome::HttpError {
                    status: 404,
                    body: "MCP session expired and re-initialization failed".to_string(),
                };
            }
        }
        UpstreamOutcome::HttpError {
            status: 401 | 403, ..
        } => {
            if !reload_credentials(shared, generation).await {
                return outcome;
            }
        }
        other => return other,
    }
    attempt(shared, msg, reply_mode).await.0
}

/// Several requests can hit the same expired session; only the first replaces it.
async fn reinitialize_if_stale(shared: &Shared, generation: u64) -> Result<()> {
    let mut conn = shared.conn.write().await;
    if conn.generation != generation {
        return Ok(());
    }
    conn.cfg
        .log_debug("mcp_proxy", "session_404_reinitializing", json!({}));
    reinitialize_session(shared, &mut conn).await
}

/// The configuration to switch to when the credential files changed and now name a
/// different identity; `None` keeps the current one.
fn changed_credentials(shared: &Shared, current: &ResolvedConfig) -> Option<ResolvedConfig> {
    let now = credential_stamp(current);
    {
        let mut stamp = shared.credentials.lock().ok()?;
        if *stamp == now {
            return None;
        }
        *stamp = now;
    }
    let workspace = shared.workspace.lock().ok().and_then(|w| w.clone());
    let cfg = workspace_config(workspace.as_deref());
    (!same_identity(current, &cfg)).then_some(cfg)
}

/// After a 401/403: reconnect with the credentials on disk if they changed. `true` when the
/// request is worth one more try.
async fn reload_credentials(shared: &Shared, generation: u64) -> bool {
    let mut conn = shared.conn.write().await;
    if conn.generation != generation {
        return true; // Another request already replaced the session.
    }
    let Some(cfg) = changed_credentials(shared, &conn.cfg) else {
        return false;
    };
    cfg.log_debug("mcp_proxy", "credentials_reloaded", json!({}));
    delete_mcp_session(shared, &mut conn).await;
    conn.cfg = cfg;
    if has_cached_initialize(shared) {
        reinitialize_session(shared, &mut conn).await.is_ok()
    } else {
        conn.generation += 1;
        true
    }
}

async fn finalize_non_ok_outcome(
    shared: &Shared,
    outcome: UpstreamOutcome,
    req_id: &Value,
) -> Result<()> {
    if req_id.is_null() || matches!(outcome, UpstreamOutcome::Success) {
        return Ok(());
    }
    let (mcp_url, root_key, timeout_ms) = {
        let conn = shared.conn.read().await;
        (
            conn.cfg.mcp_url.clone(),
            conn.cfg.root_key_warning.is_some(),
            conn.cfg.timeout_ms.max(60_000),
        )
    };
    emit(
        shared,
        &error_frame(req_id, outcome, &mcp_url, root_key, timeout_ms),
    )
    .await
}

fn error_frame(
    req_id: &Value,
    outcome: UpstreamOutcome,
    mcp_url: &str,
    root_key: bool,
    timeout_ms: u64,
) -> Value {
    match outcome {
        UpstreamOutcome::Success | UpstreamOutcome::NotFoundExpiredSession => {
            jsonrpc_error(req_id, -32002, "OpenViking MCP returned HTTP 404", None)
        }
        UpstreamOutcome::HttpError { status, body } => {
            let (code, msg) = match status {
                403 if root_key => (
                    -32001,
                    "OpenViking MCP returned 403 Forbidden: `server.root_api_key` is rejected on `/mcp`. Configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string(),
                ),
                403 => (
                    -32001,
                    "OpenViking MCP returned 403 Forbidden: this key may not use the tool (a `server.root_api_key` is always rejected on `/mcp`), or a gateway in front of the server refused the request. Check the key with `ov config`.".to_string(),
                ),
                401 => (
                    -32001,
                    "OpenViking MCP authentication failed (401 Unauthorized). Check `api_key` in `~/.openviking/ovcli.conf` or `OPENVIKING_API_KEY`.".to_string(),
                ),
                _ => (-32002, format!("OpenViking MCP returned HTTP {status}")),
            };
            let detail = (!body.trim().is_empty()).then(|| {
                let preview: String = body.chars().take(400).collect();
                json!({ "status": status, "detail": preview })
            });
            jsonrpc_error(req_id, code, &msg, detail)
        }
        UpstreamOutcome::TransportError {
            message,
            timeout: true,
        } => jsonrpc_error(
            req_id,
            -32004,
            &format!(
                "OpenViking MCP request timed out after {timeout_ms} ms ({mcp_url}); the server may still finish it"
            ),
            Some(json!({ "cause": message })),
        ),
        UpstreamOutcome::TransportError { message, .. } => jsonrpc_error(
            req_id,
            -32001,
            &format!("Failed to reach OpenViking MCP endpoint ({mcp_url})"),
            Some(json!({ "cause": message })),
        ),
    }
}

async fn reinitialize_session(shared: &Shared, conn: &mut Conn) -> Result<()> {
    let Some(mut init_msg) = shared
        .cached_initialize
        .lock()
        .ok()
        .and_then(|cached| cached.clone())
    else {
        anyhow::bail!("no cached initialize payload");
    };
    let seq = shared.reinit_seq.fetch_add(1, Ordering::Relaxed);
    if let Some(obj) = init_msg.as_object_mut() {
        obj.insert("id".to_string(), json!(format!("ov-agy-reinit-{seq}")));
    }
    conn.session_id = None;
    let forwarded = initialize_once(shared, conn, &init_msg, Delivery::Discard).await;
    if !matches!(forwarded.outcome, UpstreamOutcome::Success) {
        anyhow::bail!("re-initialize request failed");
    }
    apply_initialize(conn, &forwarded);
    let notif = shared
        .cached_initialized
        .lock()
        .ok()
        .and_then(|cached| cached.clone())
        .unwrap_or_else(|| json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    forward_once(
        shared,
        conn,
        &notif,
        SessionHeader::Attach,
        ReplyMode::Notification,
        RequestKind::Regular,
        Delivery::Discard,
    )
    .await?;
    conn.cfg.log_debug(
        "mcp_proxy",
        "reinitialized_session",
        json!({ "has_session_id": conn.session_id.is_some() }),
    );
    Ok(())
}

async fn forward_once(
    shared: &Shared,
    conn: &Conn,
    payload: &Value,
    session_header: SessionHeader,
    reply_mode: ReplyMode,
    req_kind: RequestKind,
    delivery: Delivery,
) -> Result<Forwarded> {
    use eventsource_stream::Eventsource;

    let mut forwarded = Forwarded {
        outcome: UpstreamOutcome::Success,
        session_id: None,
        protocol_version: None,
    };
    let had_active_session = session_header == SessionHeader::Attach && conn.session_id.is_some();
    let resp = match shared
        .http
        .post(&conn.cfg.mcp_url)
        .headers(build_mcp_headers(conn, session_header))
        .json(payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            forwarded.outcome = UpstreamOutcome::TransportError {
                timeout: err.is_timeout(),
                message: err.to_string(),
            };
            return Ok(forwarded);
        }
    };

    let status = resp.status();
    forwarded.session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if status.as_u16() == 404 && had_active_session {
        forwarded.outcome = UpstreamOutcome::NotFoundExpiredSession;
        return Ok(forwarded);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        forwarded.outcome = UpstreamOutcome::HttpError {
            status: status.as_u16(),
            body,
        };
        return Ok(forwarded);
    }
    if matches!(status.as_u16(), 202 | 204) {
        if reply_mode == ReplyMode::ExpectReply {
            forwarded.outcome = UpstreamOutcome::HttpError {
                status: status.as_u16(),
                body: format!(
                    "Upstream returned empty HTTP {} for JSON-RPC request expecting a response",
                    status.as_u16()
                ),
            };
        }
        return Ok(forwarded);
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let deliver = reply_mode == ReplyMode::ExpectReply && delivery == Delivery::Client;

    if content_type.contains("text/event-stream") {
        let mut event_stream = resp.bytes_stream().eventsource();
        let mut wrote_reply = false;
        let mut stream_err: Option<(String, bool)> = None;
        while let Some(event_res) = event_stream.next().await {
            let ev = match event_res {
                Ok(event) => event,
                Err(err) => {
                    let timeout =
                        matches!(&err, EventStreamError::Transport(inner) if inner.is_timeout());
                    stream_err = Some((err.to_string(), timeout));
                    break;
                }
            };
            if ev.event.eq_ignore_ascii_case("ping") {
                continue;
            }
            let data = ev.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(parsed_json) = serde_json::from_str::<Value>(data) {
                if parsed_json.is_object() || parsed_json.is_array() {
                    if req_kind == RequestKind::Initialize {
                        forwarded.protocol_version = negotiated_protocol(&parsed_json);
                    }
                    if reply_mode == ReplyMode::ExpectReply {
                        if deliver {
                            emit(shared, &parsed_json).await?;
                        }
                        wrote_reply = true;
                    }
                }
            }
        }
        if reply_mode == ReplyMode::ExpectReply && !wrote_reply {
            let (message, timeout) = stream_err.unwrap_or_else(|| {
                (
                    "MCP SSE stream closed without emitting a valid JSON-RPC response".to_string(),
                    false,
                )
            });
            forwarded.outcome = UpstreamOutcome::TransportError { message, timeout };
        }
        return Ok(forwarded);
    }

    let text = match resp.text().await {
        Ok(body) => body,
        Err(err) => {
            forwarded.outcome = UpstreamOutcome::TransportError {
                timeout: err.is_timeout(),
                message: format!("Failed reading MCP response body: {err}"),
            };
            return Ok(forwarded);
        }
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        if reply_mode == ReplyMode::ExpectReply {
            forwarded.outcome = UpstreamOutcome::TransportError {
                message: "Upstream returned empty HTTP 200 body for JSON-RPC request".to_string(),
                timeout: false,
            };
        }
        return Ok(forwarded);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed_json) if parsed_json.is_object() || parsed_json.is_array() => {
            if req_kind == RequestKind::Initialize {
                forwarded.protocol_version = negotiated_protocol(&parsed_json);
            }
            if deliver {
                emit(shared, &parsed_json).await?;
            }
        }
        _ if reply_mode == ReplyMode::Notification => {}
        _ => {
            let preview: String = trimmed.chars().take(240).collect();
            forwarded.outcome = UpstreamOutcome::HttpError {
                status: status.as_u16(),
                body: format!("Non-JSON-RPC payload returned on HTTP 200: {preview}"),
            };
        }
    }
    Ok(forwarded)
}

fn negotiated_protocol(msg: &Value) -> Option<String> {
    msg.get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn build_mcp_headers(conn: &Conn, session_header: SessionHeader) -> HeaderMap {
    let mut headers = client::build_ov_headers(&conn.cfg, ActorPeer::ActorScopeOnly);
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    if let Ok(pv) = HeaderValue::from_str(&conn.protocol_version) {
        headers.insert(HeaderName::from_static("mcp-protocol-version"), pv);
    }
    if session_header == SessionHeader::Attach {
        if let Some(hv) = conn
            .session_id
            .as_deref()
            .and_then(|sid| HeaderValue::from_str(sid).ok())
        {
            headers.insert(HeaderName::from_static("mcp-session-id"), hv);
        }
    }
    headers
}

async fn delete_mcp_session(shared: &Shared, conn: &mut Conn) {
    let Some(sid) = conn.session_id.take() else {
        return;
    };
    let mut headers = client::build_ov_headers(&conn.cfg, ActorPeer::ActorScopeOnly);
    if let Ok(hv) = HeaderValue::from_str(&sid) {
        headers.insert(HeaderName::from_static("mcp-session-id"), hv);
    }
    let _ = shared
        .http
        .delete(&conn.cfg.mcp_url)
        .headers(headers)
        .timeout(DELETE_SESSION_TIMEOUT)
        .send()
        .await;
}

fn jsonrpc_error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut err_obj = Map::new();
    err_obj.insert("code".to_string(), json!(code));
    err_obj.insert("message".to_string(), json!(message));
    if let Some(d) = data {
        err_obj.insert("data".to_string(), d);
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": Value::Object(err_obj),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_messages_with_a_method_and_an_id_expect_a_reply() {
        let request = inspect_client_message(&json!({"jsonrpc":"2.0","id":7,"method":"ping"}));
        assert_eq!(request.unwrap().reply_mode, ReplyMode::ExpectReply);
        let reply = inspect_client_message(&json!({"jsonrpc":"2.0","id":7,"result":{}}));
        assert_eq!(reply.unwrap().reply_mode, ReplyMode::Notification);
        let batch = inspect_client_message(&json!([
            {"jsonrpc":"2.0","id":"r","result":{}},
            {"jsonrpc":"2.0","id":"q","method":"ping"}
        ]))
        .unwrap();
        assert_eq!(batch.reply_mode, ReplyMode::ExpectReply);
        assert_eq!(batch.req_id, "q");
        assert!(inspect_client_message(&json!("ping")).is_none());
    }

    #[test]
    fn timeouts_and_forbidden_keys_get_distinct_errors() {
        let timeout = error_frame(
            &json!(1),
            UpstreamOutcome::TransportError {
                message: "operation timed out".into(),
                timeout: true,
            },
            "http://ov/mcp",
            false,
            60_000,
        );
        assert_eq!(timeout["error"]["code"], -32004);
        let forbidden = |root_key| {
            error_frame(
                &json!(2),
                UpstreamOutcome::HttpError {
                    status: 403,
                    body: String::new(),
                },
                "http://ov/mcp",
                root_key,
                60_000,
            )["error"]["message"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(forbidden(true).starts_with(
            "OpenViking MCP returned 403 Forbidden: `server.root_api_key` is rejected"
        ));
        assert!(forbidden(false).contains("gateway"));
    }
}
