//! Stdio <-> Streamable HTTP (`/mcp`) JSON-RPC proxy with SSE streaming and session recovery.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Map, Value};
use tokio::io::{self, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Sink};

use crate::client;
use crate::config::ResolvedConfig;

const DEFAULT_MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const DELETE_SESSION_TIMEOUT: Duration = Duration::from_secs(1);

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

struct ProxyState {
    cfg: ResolvedConfig,
    http: Client,
    session_id: Option<String>,
    protocol_version: String,
    cached_initialize: Option<Value>,
    cached_initialized_notification: Option<Value>,
    reinit_seq: AtomicU64,
}

enum UpstreamOutcome {
    Success,
    NotFoundExpiredSession,
    HttpError { status: u16, body: String },
    TransportError(String),
}

/// Runs the newline-delimited stdio JSON-RPC bridge until `stdin` reaches EOF.
pub async fn run() -> Result<()> {
    let cfg = ResolvedConfig::load(None);
    let http = Client::builder()
        .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(5_000)))
        .timeout(Duration::from_millis(cfg.timeout_ms.max(60_000)))
        .build()?;

    let mut state = ProxyState {
        cfg,
        http,
        session_id: None,
        protocol_version: DEFAULT_MCP_PROTOCOL_VERSION.to_string(),
        cached_initialize: None,
        cached_initialized_notification: None,
        reinit_seq: AtomicU64::new(1),
    };

    state.cfg.log_debug(
        "mcp_proxy",
        "start",
        json!({ "mcp_url": state.cfg.mcp_url }),
    );

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            let err_frame = jsonrpc_error(&Value::Null, -32700, "Parse error", None);
            write_json_line(&mut stdout, &err_frame).await?;
            continue;
        };

        handle_client_message(&mut state, msg, &mut stdout).await?;
    }

    delete_mcp_session(&mut state).await;
    let _ = stdout.flush().await;
    Ok(())
}

#[derive(Debug, Clone)]
struct ClientMessageMeta {
    reply_mode: ReplyMode,
    req_id: Value,
    is_initialize: bool,
    is_initialized_notification: bool,
}

fn inspect_client_message(msg: &Value) -> Option<ClientMessageMeta> {
    match msg {
        Value::Object(obj) => {
            let reply_mode = if obj.contains_key("id") {
                ReplyMode::ExpectReply
            } else {
                ReplyMode::Notification
            };
            let req_id = obj.get("id").cloned().unwrap_or(Value::Null);
            let method = obj.get("method").and_then(Value::as_str).unwrap_or("");
            Some(ClientMessageMeta {
                reply_mode,
                req_id,
                is_initialize: method == "initialize",
                is_initialized_notification: method == "notifications/initialized",
            })
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let mut first_req_id = None;
            for item in items {
                if let Some(obj) = item.as_object() {
                    if let Some(id_val) = obj.get("id") {
                        first_req_id = Some(id_val.clone());
                        break;
                    }
                }
            }
            let (reply_mode, req_id) = match first_req_id {
                Some(id_val) => (ReplyMode::ExpectReply, id_val),
                None => (ReplyMode::Notification, Value::Null),
            };
            Some(ClientMessageMeta {
                reply_mode,
                req_id,
                is_initialize: false,
                is_initialized_notification: false,
            })
        }
        _ => None,
    }
}

async fn handle_client_message<W: AsyncWrite + Unpin>(
    state: &mut ProxyState,
    msg: Value,
    stdout: &mut W,
) -> Result<()> {
    let Some(meta) = inspect_client_message(&msg) else {
        let err_frame = jsonrpc_error(&Value::Null, -32600, "Invalid Request", None);
        write_json_line(stdout, &err_frame).await?;
        return Ok(());
    };

    let reply_mode = meta.reply_mode;
    let req_id = meta.req_id;

    let (session_header, req_kind) = if meta.is_initialize {
        state.cached_initialize = Some(msg.clone());
        state.session_id = None;
        state.protocol_version = DEFAULT_MCP_PROTOCOL_VERSION.to_string();
        (SessionHeader::Omit, RequestKind::Initialize)
    } else {
        if meta.is_initialized_notification {
            state.cached_initialized_notification = Some(msg.clone());
        }
        (SessionHeader::Attach, RequestKind::Regular)
    };

    let first_outcome = forward_once(
        state,
        &msg,
        session_header,
        reply_mode,
        req_kind,
        Some(stdout),
    )
    .await?;

    match first_outcome {
        UpstreamOutcome::Success => Ok(()),
        UpstreamOutcome::NotFoundExpiredSession => {
            if req_kind == RequestKind::Regular && state.cached_initialize.is_some() {
                state.cfg.log_debug(
                    "mcp_proxy",
                    "session_404_reinitializing",
                    json!({ "is_batch": msg.is_array() }),
                );
                if reinitialize_session(state).await.is_ok() {
                    let retry_outcome = forward_once(
                        state,
                        &msg,
                        SessionHeader::Attach,
                        reply_mode,
                        RequestKind::Regular,
                        Some(stdout),
                    )
                    .await?;
                    return finalize_non_ok_outcome(
                        state,
                        retry_outcome,
                        reply_mode,
                        req_id,
                        stdout,
                    )
                    .await;
                }
            }
            finalize_non_ok_outcome(
                state,
                UpstreamOutcome::HttpError {
                    status: 404,
                    body: "MCP session expired and re-initialization failed".to_string(),
                },
                reply_mode,
                req_id,
                stdout,
            )
            .await
        }
        other => finalize_non_ok_outcome(state, other, reply_mode, req_id, stdout).await,
    }
}

async fn finalize_non_ok_outcome<W: AsyncWrite + Unpin>(
    state: &ProxyState,
    outcome: UpstreamOutcome,
    reply_mode: ReplyMode,
    req_id: Value,
    stdout: &mut W,
) -> Result<()> {
    if reply_mode == ReplyMode::Notification {
        return Ok(());
    }

    match outcome {
        UpstreamOutcome::Success => Ok(()),
        UpstreamOutcome::NotFoundExpiredSession => {
            let frame = jsonrpc_error(&req_id, -32002, "OpenViking MCP returned HTTP 404", None);
            write_json_line(stdout, &frame).await
        }
        UpstreamOutcome::HttpError { status, body } => {
            let (code, msg) = if status == 403 {
                (
                    -32001,
                    "OpenViking MCP returned 403 Forbidden. Note: `server.root_api_key` is rejected on `/mcp`; configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string(),
                )
            } else if status == 401 {
                (
                    -32001,
                    "OpenViking MCP authentication failed (401 Unauthorized). Check `api_key` in `~/.openviking/ovcli.conf` or `OPENVIKING_API_KEY`.".to_string(),
                )
            } else {
                (-32002, format!("OpenViking MCP returned HTTP {status}"))
            };
            let detail = if body.trim().is_empty() {
                None
            } else {
                let preview: String = body.chars().take(400).collect();
                Some(json!({ "status": status, "detail": preview }))
            };
            let frame = jsonrpc_error(&req_id, code, &msg, detail);
            write_json_line(stdout, &frame).await
        }
        UpstreamOutcome::TransportError(cause) => {
            let frame = jsonrpc_error(
                &req_id,
                -32001,
                &format!(
                    "Failed to reach OpenViking MCP endpoint ({})",
                    state.cfg.mcp_url
                ),
                Some(json!({ "cause": cause })),
            );
            write_json_line(stdout, &frame).await
        }
    }
}

async fn reinitialize_session(state: &mut ProxyState) -> Result<()> {
    let Some(mut init_msg) = state.cached_initialize.clone() else {
        anyhow::bail!("no cached initialize payload");
    };

    let seq = state.reinit_seq.fetch_add(1, Ordering::Relaxed);
    if let Some(obj) = init_msg.as_object_mut() {
        obj.insert("id".to_string(), json!(format!("ov-agy-reinit-{seq}")));
    }
    state.session_id = None;

    let mut sink = io::sink();
    let init_outcome = forward_once(
        state,
        &init_msg,
        SessionHeader::Omit,
        ReplyMode::ExpectReply,
        RequestKind::Initialize,
        Some(&mut sink),
    )
    .await?;
    if !matches!(init_outcome, UpstreamOutcome::Success) {
        anyhow::bail!("re-initialize request failed");
    }

    let notif = state
        .cached_initialized_notification
        .clone()
        .unwrap_or_else(|| {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
        });
    let _ = forward_once(
        state,
        &notif,
        SessionHeader::Attach,
        ReplyMode::Notification,
        RequestKind::Regular,
        None::<&mut Sink>,
    )
    .await?;

    state.cfg.log_debug(
        "mcp_proxy",
        "reinitialized_session",
        json!({ "has_session_id": state.session_id.is_some() }),
    );
    Ok(())
}

async fn forward_once<W: AsyncWrite + Unpin>(
    state: &mut ProxyState,
    payload: &Value,
    session_header: SessionHeader,
    reply_mode: ReplyMode,
    req_kind: RequestKind,
    mut stdout_opt: Option<&mut W>,
) -> Result<UpstreamOutcome> {
    use eventsource_stream::Eventsource;

    let had_active_session = session_header == SessionHeader::Attach && state.session_id.is_some();
    let headers = build_mcp_headers(state, session_header);
    let resp = match state
        .http
        .post(&state.cfg.mcp_url)
        .headers(headers)
        .json(payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => return Ok(UpstreamOutcome::TransportError(err.to_string())),
    };

    let status = resp.status();
    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        state.session_id = Some(sid.to_string());
    }

    if status.as_u16() == 404 && had_active_session {
        return Ok(UpstreamOutcome::NotFoundExpiredSession);
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Ok(UpstreamOutcome::HttpError {
            status: status.as_u16(),
            body,
        });
    }

    if matches!(status.as_u16(), 202 | 204) {
        if reply_mode == ReplyMode::Notification {
            return Ok(UpstreamOutcome::Success);
        }
        return Ok(UpstreamOutcome::HttpError {
            status: status.as_u16(),
            body: format!(
                "Upstream returned empty HTTP {} for JSON-RPC request expecting a response",
                status.as_u16()
            ),
        });
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.contains("text/event-stream") {
        let mut event_stream = resp.bytes_stream().eventsource();
        let mut wrote_reply = false;
        let mut stream_err: Option<String> = None;

        while let Some(event_res) = event_stream.next().await {
            let ev = match event_res {
                Ok(event) => event,
                Err(err) => {
                    stream_err = Some(err.to_string());
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
                        update_negotiated_protocol(state, &parsed_json);
                    }
                    if reply_mode == ReplyMode::ExpectReply {
                        if let Some(out) = stdout_opt.as_deref_mut() {
                            write_json_line(out, &parsed_json).await?;
                        }
                        wrote_reply = true;
                    }
                }
            }
        }

        if reply_mode == ReplyMode::ExpectReply && !wrote_reply {
            let reason = stream_err.unwrap_or_else(|| {
                "MCP SSE stream closed without emitting a valid JSON-RPC response".to_string()
            });
            return Ok(UpstreamOutcome::TransportError(reason));
        }
        return Ok(UpstreamOutcome::Success);
    }

    let text = match resp.text().await {
        Ok(body) => body,
        Err(err) => {
            return Ok(UpstreamOutcome::TransportError(format!(
                "Failed reading MCP response body: {err}"
            )));
        }
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        if reply_mode == ReplyMode::Notification {
            return Ok(UpstreamOutcome::Success);
        }
        return Ok(UpstreamOutcome::TransportError(
            "Upstream returned empty HTTP 200 body for JSON-RPC request".to_string(),
        ));
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed_json) if parsed_json.is_object() || parsed_json.is_array() => {
            if req_kind == RequestKind::Initialize {
                update_negotiated_protocol(state, &parsed_json);
            }
            if reply_mode == ReplyMode::ExpectReply {
                if let Some(out) = stdout_opt {
                    write_json_line(out, &parsed_json).await?;
                }
            }
            Ok(UpstreamOutcome::Success)
        }
        _ => {
            if reply_mode == ReplyMode::Notification {
                return Ok(UpstreamOutcome::Success);
            }
            let preview: String = trimmed.chars().take(240).collect();
            Ok(UpstreamOutcome::HttpError {
                status: status.as_u16(),
                body: format!("Non-JSON-RPC payload returned on HTTP 200: {preview}"),
            })
        }
    }
}

fn update_negotiated_protocol(state: &mut ProxyState, msg: &Value) {
    if let Some(ver) = msg
        .get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        state.protocol_version = ver.to_string();
    }
}

fn build_mcp_headers(state: &ProxyState, session_header: SessionHeader) -> HeaderMap {
    let mut headers = client::build_ov_headers(&state.cfg);
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    if let Ok(pv) = HeaderValue::from_str(&state.protocol_version) {
        headers.insert(HeaderName::from_static("mcp-protocol-version"), pv);
    }
    if session_header == SessionHeader::Attach {
        if let Some(ref sid) = state.session_id {
            if let Ok(hv) = HeaderValue::from_str(sid) {
                headers.insert(HeaderName::from_static("mcp-session-id"), hv);
            }
        }
    }
    headers
}

async fn delete_mcp_session(state: &mut ProxyState) {
    let Some(sid) = state.session_id.take() else {
        return;
    };
    let mut headers = client::build_ov_headers(&state.cfg);
    if let Ok(hv) = HeaderValue::from_str(&sid) {
        headers.insert(HeaderName::from_static("mcp-session-id"), hv);
    }
    let _ = state
        .http
        .delete(&state.cfg.mcp_url)
        .headers(headers)
        .timeout(DELETE_SESSION_TIMEOUT)
        .send()
        .await;
}

async fn write_json_line<W: AsyncWrite + Unpin>(out: &mut W, val: &Value) -> Result<()> {
    let compact = serde_json::to_string(val)?;
    out.write_all(compact.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
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
