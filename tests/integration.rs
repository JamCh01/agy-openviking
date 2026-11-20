//! End-to-end integration tests for `ov-agy` against a local mock `axum` OpenViking server.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::{self, JoinHandle};
use tokio::time::Instant;

fn ov_agy_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ov-agy"))
}

#[derive(Debug, Clone)]
struct ObservedAuth {
    authorization: String,
    api_key: String,
}

#[derive(Debug, Clone)]
struct CapturedSessionMessage {
    session_id: String,
    body: Value,
}

#[derive(Debug)]
struct CommandOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
struct TestConfigPaths {
    ov_home: PathBuf,
    plugin_data: PathBuf,
    cli_conf: PathBuf,
}

#[derive(Default)]
struct MockServerState {
    search_hits: AtomicUsize,
    last_search_body: Mutex<Option<Value>>,
    observed_peers: Mutex<Vec<String>>,
    observed_auth_headers: Mutex<Vec<ObservedAuth>>,
    fail_messages_503: AtomicBool,
    captured_messages: Mutex<Vec<CapturedSessionMessage>>,
    committed_sessions: Mutex<Vec<String>>,
    mcp_initialize_calls: AtomicUsize,
    mcp_initialized_notif_calls: AtomicUsize,
    mcp_active_session: Mutex<String>,
    mcp_deleted_sessions: Mutex<Vec<String>>,
    mcp_return_403: AtomicBool,
}

struct SpawnedServer {
    addr: SocketAddr,
    _handle: JoinHandle<()>,
}

async fn spawn_mock_ov_server(state: Arc<MockServerState>) -> SpawnedServer {
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/api/v1/system/status", get(handle_system_status))
        .route("/api/v1/fs/ls", get(handle_fs_ls))
        .route("/api/v1/search/search", post(handle_search))
        .route("/api/v1/sessions", post(handle_create_session))
        .route(
            "/api/v1/sessions/{session_id}/messages",
            post(handle_add_message),
        )
        .route(
            "/api/v1/sessions/{session_id}/commit",
            post(handle_commit_session),
        )
        .route("/mcp", post(handle_mcp_post).delete(handle_mcp_delete))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral loopback port must bind");
    let addr = listener
        .local_addr()
        .expect("listener must report local socket address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    SpawnedServer {
        addr,
        _handle: handle,
    }
}

fn record_common_headers(state: &MockServerState, headers: &HeaderMap) {
    if let Some(peer) = headers
        .get("x-openviking-actor-peer")
        .and_then(|v| v.to_str().ok())
    {
        state
            .observed_peers
            .lock()
            .expect("observed_peers mutex should not be poisoned")
            .push(peer.to_string());
    }
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !authorization.is_empty() || !api_key.is_empty() {
        state
            .observed_auth_headers
            .lock()
            .expect("observed_auth_headers mutex should not be poisoned")
            .push(ObservedAuth {
                authorization,
                api_key,
            });
    }
}

async fn handle_health(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    record_common_headers(&state, &headers);
    Json(json!({
        "status": "ok",
        "healthy": true,
        "version": "0.5.0-mock",
        "auth_mode": "api_key",
        "user_id": "alice"
    }))
}

async fn handle_system_status(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    record_common_headers(&state, &headers);
    Json(json!({
        "status": "ok",
        "result": {
            "initialized": true,
            "user": "alice"
        }
    }))
}

async fn handle_fs_ls(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    record_common_headers(&state, &headers);
    let uri = params.get("uri").map(String::as_str).unwrap_or("");
    let entries = match uri {
        "viking://~/skills" => json!([
            {
                "name": "rust-audit",
                "uri": "viking://user/alice/skills/rust-audit",
                "isDir": true,
                "abstract": "Audit Rust crates for unsafe blocks and musl compatibility."
            }
        ]),
        "viking://agent/skills" => json!([
            {
                "name": "deploy-runbook",
                "uri": "viking://agent/skills/deploy-runbook",
                "isDir": true,
                "abstract": "Shared production rollout checklist."
            }
        ]),
        _ => json!([]),
    };
    Json(json!({
        "status": "ok",
        "result": entries
    }))
}

async fn handle_search(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    record_common_headers(&state, &headers);
    state.search_hits.fetch_add(1, Ordering::SeqCst);
    *state
        .last_search_body
        .lock()
        .expect("last_search_body mutex should not be poisoned") = Some(body.clone());

    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    Json(json!({
        "status": "ok",
        "result": {
            "rendered": format!("<memory uri=\"viking://user/alice/memories/preferences/arch.md\" type=\"preferences\" score=\"0.91\">Recalled note for: {query}</memory>"),
            "digest": "",
            "entries": [
                {
                    "uri": "viking://user/alice/memories/preferences/arch.md",
                    "category": "preferences",
                    "detail": "abstract",
                    "score": 0.91,
                    "text": format!("Recalled note for: {query}")
                }
            ],
            "stats": {
                "candidates": 1,
                "returned": 1,
                "used_tokens": 32,
                "rewrite": "off"
            }
        }
    }))
}

async fn handle_create_session(Json(body): Json<Value>) -> impl IntoResponse {
    let sid = body
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("agy-unknown");
    Json(json!({
        "status": "ok",
        "result": {
            "session_id": sid
        }
    }))
}

async fn handle_add_message(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    record_common_headers(&state, &headers);
    if state.fail_messages_503.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "error",
                "error": {
                    "code": "UNAVAILABLE",
                    "message": "Simulated 503 outage"
                }
            })),
        )
            .into_response();
    }

    state
        .captured_messages
        .lock()
        .expect("captured_messages mutex should not be poisoned")
        .push(CapturedSessionMessage {
            session_id: session_id.clone(),
            body,
        });

    Json(json!({
        "status": "ok",
        "result": {
            "session_id": session_id,
            "message_count": 1
        }
    }))
    .into_response()
}

async fn handle_commit_session(
    State(state): State<Arc<MockServerState>>,
    AxumPath(session_id): AxumPath<String>,
    Json(_body): Json<Value>,
) -> impl IntoResponse {
    state
        .committed_sessions
        .lock()
        .expect("committed_sessions mutex should not be poisoned")
        .push(session_id.clone());
    Json(json!({
        "status": "ok",
        "result": {
            "session_id": session_id,
            "status": "accepted",
            "task_id": "task-mock-1",
            "archived": true
        }
    }))
}

async fn handle_mcp_post(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record_common_headers(&state, &headers);

    if state.mcp_return_403.load(Ordering::SeqCst) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error",
                "error": {
                    "code": "PERMISSION_DENIED",
                    "message": "ROOT API keys cannot access /mcp"
                }
            })),
        )
            .into_response();
    }

    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let req_id = body.get("id").cloned().unwrap_or(Value::Null);

    if method == "initialize" {
        let n = state.mcp_initialize_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let new_sid = format!("mcp-sess-{n}");
        *state
            .mcp_active_session
            .lock()
            .expect("mcp_active_session mutex should not be poisoned") = new_sid.clone();

        let mut resp = Json(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "openviking-mock", "version": "0.5.0" }
            }
        }))
        .into_response();
        resp.headers_mut().insert(
            "mcp-session-id",
            HeaderValue::from_str(&new_sid).expect("session ID must be valid ASCII header"),
        );
        return resp;
    }

    let incoming_sid = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected_sid = state
        .mcp_active_session
        .lock()
        .expect("mcp_active_session mutex should not be poisoned")
        .clone();
    if incoming_sid.is_empty() || incoming_sid != expected_sid {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "status": "error",
                "error": { "code": "NOT_FOUND", "message": "Session expired or unknown" }
            })),
        )
            .into_response();
    }

    if method == "notifications/initialized" {
        state
            .mcp_initialized_notif_calls
            .fetch_add(1, Ordering::SeqCst);
        return StatusCode::ACCEPTED.into_response();
    }

    if method == "tools/list" {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    { "name": "search", "description": "Context search" },
                    { "name": "read", "description": "Read viking:// URI" }
                ]
            }
        });
        let serialized = serde_json::to_string(&payload).expect("tools/list JSON should serialize");
        let sse_body = format!(": keep-alive\n\nevent: message\ndata: {serialized}\n\n");
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("mcp-session-id", incoming_sid)
            .body(Body::from(sse_body))
            .expect("SSE response builder must succeed");
    }

    if method == "tools/call" {
        let tool_name = body
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Json(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{ "type": "text", "text": format!("Executed {tool_name} under {incoming_sid}") }],
                "isError": false
            }
        }))
        .into_response();
    }

    Json(json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {}
    }))
    .into_response()
}

async fn handle_mcp_delete(
    State(state): State<Arc<MockServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        state
            .mcp_deleted_sessions
            .lock()
            .expect("mcp_deleted_sessions mutex should not be poisoned")
            .push(sid.to_string());
    }
    StatusCode::NO_CONTENT
}

fn run_hook_cmd(args: &[&str], stdin_json: &str, envs: &[(&str, &str)]) -> CommandOutput {
    let mut cmd = Command::new(ov_agy_bin());
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for key in [
        "OPENVIKING_MEMORY_ENABLED",
        "OPENVIKING_URL",
        "OPENVIKING_BASE_URL",
        "OPENVIKING_API_KEY",
        "OPENVIKING_BEARER_TOKEN",
        "OPENVIKING_CLI_CONFIG_FILE",
        "OPENVIKING_CONFIG_FILE",
        "OPENVIKING_PLUGIN_DATA",
        "OPENVIKING_HOME",
        "OPENVIKING_PEER_ID",
        "OPENVIKING_CREDENTIAL_SOURCE",
        "PLUGIN_DATA",
    ] {
        cmd.env_remove(key);
    }

    for (key, value) in envs {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().expect("ov-agy binary must spawn");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_json.as_bytes())
            .expect("writing hook JSON to stdin should succeed");
    }
    let output = child
        .wait_with_output()
        .expect("ov-agy process must finish");
    CommandOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn setup_test_ovcli(dir: &Path, base_url: &str, api_key: &str) -> TestConfigPaths {
    let ov_home = dir.join("ov-home");
    let plugin_data = dir.join("plugin-data");
    fs::create_dir_all(&ov_home).expect("ov-home directory should be created");
    fs::create_dir_all(&plugin_data).expect("plugin-data directory should be created");
    let cli_conf = ov_home.join("ovcli.conf");
    let config_body = serde_json::to_string_pretty(&json!({
        "url": base_url,
        "api_key": api_key,
        "plugin": {
            "antigravity": {
                "enabled": true
            }
        }
    }))
    .expect("ovcli.conf JSON must serialize");
    fs::write(&cli_conf, config_body).expect("ovcli.conf should be written");
    TestConfigPaths {
        ov_home,
        plugin_data,
        cli_conf,
    }
}

// ── Test 1: `PreToolUse` Guard ─────────────────────────────────────────────

#[test]
fn test_1_pre_tool_use_guard() {
    let temp_dir = TempDir::new().expect("tempdir should be created");
    let home = temp_dir
        .path()
        .to_str()
        .expect("tempdir path must be UTF-8");

    // 1. `viking://~/memories/foo.md` -> decision: "deny"
    let deny_res = run_hook_cmd(
        &["hook", "pre-tool"],
        r#"{"toolCall":{"id":"call_1","name":"view_file","args":{"AbsolutePath":"viking://~/memories/foo.md"}}}"#,
        &[("HOME", home)],
    );
    assert_eq!(deny_res.exit_code, 0, "pre-tool must exit 0 on deny");
    assert!(
        deny_res.stderr.is_empty(),
        "stderr must remain clean during normal hook execution, got: {}",
        deny_res.stderr
    );
    let deny_json: Value =
        serde_json::from_str(deny_res.stdout.trim()).expect("pre-tool stdout must be valid JSON");
    assert_eq!(
        deny_json["decision"], "deny",
        "viking:// URI must trigger decision=deny"
    );
    assert!(
        deny_json["reason"]
            .as_str()
            .unwrap_or("")
            .contains("viking://"),
        "deny reason must mention viking:// virtual filesystem"
    );

    // 2. Local `view_file` -> decision: "allow" (never `{}`)
    let allow_res = run_hook_cmd(
        &["hook", "pre-tool"],
        r#"{"toolCall":{"id":"call_2","name":"view_file","args":{"AbsolutePath":"/Users/dev/project/README.md"}}}"#,
        &[("HOME", home)],
    );
    assert_eq!(allow_res.exit_code, 0, "pre-tool must exit 0 on allow");
    assert_ne!(
        allow_res.stdout.trim(),
        "{}",
        "PreToolUse must never emit empty JSON object"
    );
    let allow_json: Value =
        serde_json::from_str(allow_res.stdout.trim()).expect("allow output must be valid JSON");
    assert_eq!(
        allow_json,
        json!({ "decision": "allow" }),
        "read-only view_file on local path must return decision=allow"
    );

    // 3. Local `write_to_file` -> decision: "ask" (never `{}`)
    let ask_res = run_hook_cmd(
        &["hook", "pre-tool"],
        r#"{"toolCall":{"id":"call_3","name":"write_to_file","args":{"TargetFile":"/Users/dev/project/src/lib.rs","CodeContent":"pub fn ok() {}"}}}"#,
        &[("HOME", home)],
    );
    assert_eq!(ask_res.exit_code, 0, "pre-tool must exit 0 on ask");
    assert_ne!(
        ask_res.stdout.trim(),
        "{}",
        "PreToolUse must never emit empty JSON object on write tools"
    );
    let ask_json: Value =
        serde_json::from_str(ask_res.stdout.trim()).expect("ask output must be valid JSON");
    assert_eq!(
        ask_json,
        json!({ "decision": "ask" }),
        "write_to_file on local path must return decision=ask"
    );
}

// ── Test 2: `PreInvocation` Loop Deduplication ─────────────────────────────

#[tokio::test]
async fn test_2_pre_invocation_loop_dedup() {
    let mock_state = Arc::new(MockServerState::default());
    let server = spawn_mock_ov_server(mock_state.clone()).await;
    let base_url = format!("http://{}", server.addr);

    let temp_dir = TempDir::new().expect("tempdir should be created");
    let paths = setup_test_ovcli(temp_dir.path(), &base_url, "user-key-valid-12345");

    let transcript_path = temp_dir.path().join("transcript.jsonl");
    fs::write(
        &transcript_path,
        concat!(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nHow should we structure rustls HTTP client timeouts?\n</USER_REQUEST>"}"#,
            "\n"
        ),
    )
    .expect("initial transcript should write");

    let envs_owned = vec![
        ("HOME".to_string(), temp_dir.path().display().to_string()),
        (
            "OPENVIKING_HOME".to_string(),
            paths.ov_home.display().to_string(),
        ),
        (
            "OPENVIKING_PLUGIN_DATA".to_string(),
            paths.plugin_data.display().to_string(),
        ),
        (
            "OPENVIKING_CLI_CONFIG_FILE".to_string(),
            paths.cli_conf.display().to_string(),
        ),
    ];

    // Invocation 1: hits `/api/v1/search/search` once and returns `ephemeralMessage`.
    let stdin_inv_1 = json!({
        "conversationId": "conv-loop-42",
        "workspacePaths": [temp_dir.path().display().to_string()],
        "transcriptPath": transcript_path.display().to_string(),
        "invocationNum": 1
    })
    .to_string();

    let out_inv_1 = task::spawn_blocking({
        let envs_clone = envs_owned.clone();
        move || {
            let refs: Vec<(&str, &str)> = envs_clone
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            run_hook_cmd(&["hook", "pre-invocation"], &stdin_inv_1, &refs)
        }
    })
    .await
    .expect("blocking task for invocation 1 should succeed");

    assert_eq!(out_inv_1.exit_code, 0, "pre-invocation 1 must exit 0");
    assert_eq!(
        mock_state.search_hits.load(Ordering::SeqCst),
        1,
        "first invocation of user turn must trigger exactly 1 search call"
    );

    let json_1: Value = serde_json::from_str(out_inv_1.stdout.trim())
        .expect("invocation 1 stdout must be valid JSON");
    assert!(
        json_1.get("decision").is_none(),
        "PreInvocation output must NEVER include a `decision` key"
    );
    assert!(
        !out_inv_1.stdout.contains("\"metadata\""),
        "PreInvocation output must NEVER include `metadata` (proto_utils.go panic hazard)"
    );
    let ephemeral_msg = json_1["injectSteps"][0]["ephemeralMessage"]
        .as_str()
        .expect("injectSteps[0].ephemeralMessage must be present");
    assert!(
        ephemeral_msg.contains("<openviking-context>"),
        "injected message must wrap recalled content in <openviking-context>"
    );
    assert!(
        ephemeral_msg
            .contains("Recalled note for: How should we structure rustls HTTP client timeouts?"),
        "injected message must carry recalled server body"
    );

    let search_req = mock_state
        .last_search_body
        .lock()
        .expect("last_search_body mutex should not be poisoned")
        .clone()
        .expect("search body must have been recorded");
    assert_eq!(search_req["mode"], "context", "search mode must be context");
    assert_eq!(
        search_req["dedup_turns"], 5,
        "dedup_turns must default to 5"
    );
    assert_eq!(search_req["limit"], 6, "recall limit must default to 6");
    assert_eq!(
        search_req["session_id"], "agy-conv-loop-42",
        "session_id must follow agy-<conversationId>"
    );

    let auth_hdrs = mock_state
        .observed_auth_headers
        .lock()
        .expect("observed_auth_headers mutex should not be poisoned")
        .clone();
    assert!(!auth_hdrs.is_empty(), "auth headers must be sent on search");
    assert_eq!(
        auth_hdrs[0].authorization, "Bearer user-key-valid-12345",
        "Authorization Bearer header must match configured key"
    );
    assert_eq!(
        auth_hdrs[0].api_key, "user-key-valid-12345",
        "X-API-Key header must match configured key"
    );

    // Append model tool-call step (same user turn) and invoke `invocationNum: 2`.
    let mut append_file = OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .expect("transcript file should open for append");
    writeln!(
        append_file,
        r#"{{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","content":"","tool_calls":[{{"name":"view_file"}}]}}"#
    )
    .expect("assistant step should append");
    drop(append_file);

    let stdin_inv_2 = json!({
        "conversationId": "conv-loop-42",
        "workspacePaths": [temp_dir.path().display().to_string()],
        "transcriptPath": transcript_path.display().to_string(),
        "invocationNum": 2
    })
    .to_string();

    let out_inv_2 = task::spawn_blocking({
        let envs_clone = envs_owned.clone();
        move || {
            let refs: Vec<(&str, &str)> = envs_clone
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            run_hook_cmd(&["hook", "pre-invocation"], &stdin_inv_2, &refs)
        }
    })
    .await
    .expect("blocking task for invocation 2 should succeed");

    assert_eq!(out_inv_2.exit_code, 0, "pre-invocation 2 must exit 0");
    assert_eq!(
        mock_state.search_hits.load(Ordering::SeqCst),
        1,
        "invocationNum 2 within same user turn must make ZERO network requests"
    );
    assert_eq!(
        out_inv_2.stdout.trim(),
        "{}",
        "deduplicated inner loop step must emit empty object `{{}}`"
    );

    // Append a new user turn -> search must fire again!
    let mut append_file = OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .expect("transcript file should open for second user turn");
    writeln!(
        append_file,
        r#"{{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nNow check SSE reconnection semantics\n</USER_REQUEST>"}}"#
    )
    .expect("second user turn should append");
    drop(append_file);

    let stdin_inv_3 = json!({
        "conversationId": "conv-loop-42",
        "workspacePaths": [temp_dir.path().display().to_string()],
        "transcriptPath": transcript_path.display().to_string(),
        "invocationNum": 3
    })
    .to_string();

    let out_inv_3 = task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = envs_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_hook_cmd(&["hook", "pre-invocation"], &stdin_inv_3, &refs)
    })
    .await
    .expect("blocking task for invocation 3 should succeed");

    assert_eq!(out_inv_3.exit_code, 0, "pre-invocation 3 must exit 0");
    assert_eq!(
        mock_state.search_hits.load(Ordering::SeqCst),
        2,
        "new user turn must trigger a second search request"
    );
    let json_3: Value = serde_json::from_str(out_inv_3.stdout.trim())
        .expect("invocation 3 stdout must be valid JSON");
    assert!(
        json_3["injectSteps"][0]["ephemeralMessage"]
            .as_str()
            .expect("invocation 3 ephemeralMessage must be string")
            .contains("Now check SSE reconnection semantics"),
        "invocation 3 must inject recalled context for the new prompt"
    );
    assert!(
        paths
            .ov_home
            .join("state")
            .join("last-recall.json")
            .is_file(),
        "last-recall.json audit snapshot must exist"
    );
}

// ── Test 3: Git Worktree Peer Normalization ────────────────────────────────

#[tokio::test]
async fn test_3_git_worktree_peer_normalization() {
    let mock_state = Arc::new(MockServerState::default());
    let server = spawn_mock_ov_server(mock_state.clone()).await;
    let base_url = format!("http://{}", server.addr);

    let temp_dir = TempDir::new().expect("tempdir should be created");
    let paths = setup_test_ovcli(temp_dir.path(), &base_url, "user-key-peer-test");

    let main_repo = temp_dir.path().join("repo-ssh");
    let main_git = main_repo.join(".git");
    let wt_admin = main_git.join("worktrees").join("subagent-wt-1");
    fs::create_dir_all(&wt_admin).expect("worktree git admin dir should be created");
    fs::write(
        main_git.join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:AcmeOrg/CoreService.git\n",
    )
    .expect("main repo .git/config should write");

    let linked_wt = temp_dir.path().join("isolated-worktree-dir");
    let linked_wt_subdir = linked_wt.join("src").join("handlers");
    fs::create_dir_all(&linked_wt_subdir).expect("linked worktree subdir should be created");
    fs::write(
        linked_wt.join(".git"),
        format!("gitdir: {}\n", wt_admin.display()),
    )
    .expect("linked worktree .git pointer should write");
    fs::write(wt_admin.join("commondir"), "../..\n").expect("commondir file should write");

    let https_repo = temp_dir.path().join("repo-https");
    fs::create_dir_all(https_repo.join(".git")).expect("https repo .git dir should be created");
    fs::write(
        https_repo.join(".git").join("config"),
        "[remote \"origin\"]\n\turl = https://github.com/acmeorg/coreservice.git/\n",
    )
    .expect("https repo .git/config should write");

    let envs_vec = vec![
        ("HOME".to_string(), temp_dir.path().display().to_string()),
        (
            "OPENVIKING_HOME".to_string(),
            paths.ov_home.display().to_string(),
        ),
        (
            "OPENVIKING_PLUGIN_DATA".to_string(),
            paths.plugin_data.display().to_string(),
        ),
        (
            "OPENVIKING_CLI_CONFIG_FILE".to_string(),
            paths.cli_conf.display().to_string(),
        ),
    ];

    for (target_dir, expect_worktree) in [
        (&main_repo, false),
        (&linked_wt_subdir, true),
        (&https_repo, false),
    ] {
        let dir_str = target_dir.display().to_string();
        let envs_clone = envs_vec.clone();
        let doc_out = task::spawn_blocking(move || {
            let refs: Vec<(&str, &str)> = envs_clone
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            run_hook_cmd(
                &["doctor", "--json", "--offline", "--cwd", &dir_str],
                "",
                &refs,
            )
        })
        .await
        .expect("doctor blocking task should succeed");

        assert_eq!(doc_out.exit_code, 0, "doctor must exit 0");
        let report: Value = serde_json::from_str(doc_out.stdout.trim())
            .expect("doctor --json output must be valid JSON");
        assert_eq!(
            report["peer_id"], "github.com-acmeorg-coreservice",
            "main repo, worktree, and HTTPS clone must resolve to identical peer_id"
        );
        assert_eq!(
            report["git_is_worktree"],
            expect_worktree,
            "git_is_worktree mismatch for {}",
            target_dir.display()
        );
    }

    let start_stdin = json!({
        "conversationId": "conv-wt-check",
        "workspacePaths": [linked_wt_subdir.display().to_string()]
    })
    .to_string();
    let ss_out = task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = envs_vec
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_hook_cmd(&["hook", "session-start"], &start_stdin, &refs)
    })
    .await
    .expect("session-start blocking task should succeed");

    assert_eq!(ss_out.exit_code, 0, "session-start must exit 0");
    let ss_json: Value =
        serde_json::from_str(ss_out.stdout.trim()).expect("session-start stdout must be JSON");
    let ss_msg = ss_json["injectSteps"][0]["ephemeralMessage"]
        .as_str()
        .expect("session-start must emit ephemeralMessage");
    assert!(
        ss_msg.contains("<openviking-session>"),
        "session-start must include <openviking-session>"
    );
    assert!(
        ss_msg.contains("peer=github.com-acmeorg-coreservice"),
        "session-start banner must report normalized worktree peer"
    );
    assert!(
        ss_msg.contains("<available-skills>")
            && ss_msg.contains("rust-audit")
            && ss_msg.contains("deploy-runbook"),
        "session-start must inject remote skills from viking://~/skills and viking://agent/skills"
    );

    let peers = mock_state
        .observed_peers
        .lock()
        .expect("observed_peers mutex should not be poisoned")
        .clone();
    assert!(
        peers.iter().all(|p| p == "github.com-acmeorg-coreservice"),
        "every HTTP request from worktree must carry X-OpenViking-Actor-Peer=github.com-acmeorg-coreservice, got {peers:?}"
    );
}

// ── Test 4: `Worker` Sanitization & Offline WAL ────────────────────────────

#[tokio::test]
async fn test_4_worker_sanitization_and_offline_wal() {
    let mock_state = Arc::new(MockServerState::default());
    mock_state.fail_messages_503.store(true, Ordering::SeqCst);

    let server = spawn_mock_ov_server(mock_state.clone()).await;
    let base_url = format!("http://{}", server.addr);

    let temp_dir = TempDir::new().expect("tempdir should be created");
    let paths = setup_test_ovcli(temp_dir.path(), &base_url, "user-key-worker-999");

    let transcript_path = temp_dir.path().join("transcript.jsonl");
    fs::write(
        &transcript_path,
        concat!(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<openviking-context>\nsecret-old-memory\n</openviking-context>\n<openviking-session>banner</openviking-session>\n<USER_REQUEST>\nWe standardized on rustls-tls for static musl binaries.\n</USER_REQUEST>"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","content":"Confirmed: using rustls-tls with zero OpenSSL dependency.\n<available-skills>- stale-skill</available-skills>"}"#,
            "\n"
        ),
    )
    .expect("fixture transcript should write");

    let envs_vec = vec![
        ("HOME".to_string(), temp_dir.path().display().to_string()),
        (
            "OPENVIKING_HOME".to_string(),
            paths.ov_home.display().to_string(),
        ),
        (
            "OPENVIKING_PLUGIN_DATA".to_string(),
            paths.plugin_data.display().to_string(),
        ),
        (
            "OPENVIKING_CLI_CONFIG_FILE".to_string(),
            paths.cli_conf.display().to_string(),
        ),
        ("OPENVIKING_COMMIT_EVERY_TURNS".to_string(), "2".to_string()),
    ];

    // Phase 1: HTTP 503 -> sanitized turns must land in `wal/pending.jsonl`.
    let tp_str = transcript_path.display().to_string();
    let ws_str = temp_dir.path().display().to_string();
    let envs_clone = envs_vec.clone();
    let worker_1 = task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = envs_clone
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_hook_cmd(
            &[
                "worker",
                "--conversation-id",
                "conv-wal-1",
                "--transcript-path",
                &tp_str,
                "--cwd",
                &ws_str,
            ],
            "",
            &refs,
        )
    })
    .await
    .expect("worker phase 1 task should complete");
    assert_eq!(worker_1.exit_code, 0, "worker must exit 0 during HTTP 503");
    assert!(
        mock_state
            .captured_messages
            .lock()
            .expect("captured_messages mutex should not be poisoned")
            .is_empty(),
        "no messages should be recorded on server while returning 503"
    );

    let wal_path = paths.plugin_data.join("wal").join("pending.jsonl");
    assert!(wal_path.is_file(), "WAL file must be created on HTTP 503");
    let wal_raw = fs::read_to_string(&wal_path).expect("WAL file should be readable");
    assert!(
        !wal_raw.contains("secret-old-memory"),
        "WAL must never persist unstripped <openviking-context> blocks"
    );
    assert!(
        wal_raw.contains("rustls-tls"),
        "WAL must contain the sanitized user message"
    );

    // Phase 2: Restore server (`200 OK`) and trigger `hook stop` (`fullyIdle: true`).
    mock_state.fail_messages_503.store(false, Ordering::SeqCst);

    let mut append_file = OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .expect("transcript should open for append");
    writeln!(
        append_file,
        r#"{{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"Also enable thin LTO in release profile."}}"#
    )
    .expect("third transcript line should append");
    drop(append_file);

    let stop_stdin = json!({
        "conversationId": "conv-wal-1",
        "workspacePaths": [temp_dir.path().display().to_string()],
        "transcriptPath": transcript_path.display().to_string(),
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL"
    })
    .to_string();

    let stop_out = task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = envs_vec
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_hook_cmd(&["hook", "stop"], &stop_stdin, &refs)
    })
    .await
    .expect("stop hook blocking task should succeed");
    assert_eq!(stop_out.exit_code, 0, "stop hook must exit 0");
    assert_eq!(stop_out.stdout.trim(), "{}", "stop hook must emit `{{}}`");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = mock_state
            .captured_messages
            .lock()
            .expect("captured_messages mutex should not be poisoned")
            .len();
        if count >= 3 && !wal_path.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for detached worker to replay WAL (captured={count}, wal_exists={})",
            wal_path.exists()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let captured = mock_state
        .captured_messages
        .lock()
        .expect("captured_messages mutex should not be poisoned")
        .clone();
    assert_eq!(
        captured.len(),
        3,
        "all 3 sanitized turns (2 from WAL + 1 new) must arrive in order"
    );
    for item in &captured {
        assert_eq!(
            item.session_id, "agy-conv-wal-1",
            "session ID must match agy-<conversationId>"
        );
        let serialized =
            serde_json::to_string(&item.body).expect("captured payload must serialize");
        assert!(
            !serialized.contains("secret-old-memory")
                && !serialized.contains("<openviking-context>")
                && !serialized.contains("<openviking-session>")
                && !serialized.contains("<available-skills>"),
            "captured payload must strip all injected XML blocks, got: {serialized}"
        );
    }
    assert_eq!(
        captured[0].body["parts"][0]["text"],
        "We standardized on rustls-tls for static musl binaries.",
        "first replayed turn text mismatch"
    );
    assert_eq!(
        captured[1].body["parts"][0]["text"],
        "Confirmed: using rustls-tls with zero OpenSSL dependency.",
        "second replayed turn text mismatch"
    );
    assert_eq!(
        captured[2].body["parts"][0]["text"], "Also enable thin LTO in release profile.",
        "new delta turn text mismatch"
    );

    let commits = mock_state
        .committed_sessions
        .lock()
        .expect("committed_sessions mutex should not be poisoned")
        .clone();
    assert!(
        commits.contains(&"agy-conv-wal-1".to_string()),
        "session commit must fire once uncommitted turns reach threshold"
    );
    assert!(
        !wal_path.exists(),
        "pending.jsonl WAL file must be removed once fully replayed"
    );
}

// ── Test 5: `McpProxy` JSON + SSE + 404 Session Recovery + EOF DELETE ──────

#[tokio::test]
async fn test_5_mcp_proxy_json_sse_and_404_recovery() {
    let mock_state = Arc::new(MockServerState::default());
    let server = spawn_mock_ov_server(mock_state.clone()).await;
    let base_url = format!("http://{}", server.addr);

    let temp_dir = TempDir::new().expect("tempdir should be created");
    let paths = setup_test_ovcli(temp_dir.path(), &base_url, "user-key-mcp-777");

    let mut child = Command::new(ov_agy_bin())
        .arg("mcp-proxy")
        .env("HOME", temp_dir.path())
        .env("OPENVIKING_HOME", &paths.ov_home)
        .env("OPENVIKING_PLUGIN_DATA", &paths.plugin_data)
        .env("OPENVIKING_CLI_CONFIG_FILE", &paths.cli_conf)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mcp-proxy child process must spawn");

    let mut child_stdin = child.stdin.take().expect("mcp-proxy stdin must be piped");
    let child_stdout = child.stdout.take().expect("mcp-proxy stdout must be piped");

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    thread::spawn(move || {
        let stdout_lines = BufReader::new(child_stdout).lines().map_while(Result::ok);
        for line in stdout_lines {
            let _ = tx.send(line);
        }
    });

    // 1. `initialize` -> `application/json` response + `mcp-session-id: mcp-sess-1`.
    writeln!(
        child_stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-03-26","capabilities":{{}},"clientInfo":{{"name":"agy","version":"1.5.13"}}}}}}"#
    )
    .expect("initialize frame should write to proxy stdin");
    child_stdin.flush().expect("proxy stdin should flush");

    let line1 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for initialize response")
        .expect("proxy stdout channel closed unexpectedly");
    let init_resp: Value =
        serde_json::from_str(&line1).expect("initialize stdout line must be valid JSON");
    assert_eq!(init_resp["id"], 1, "initialize response id must match");
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"], "openviking-mock",
        "initialize response must preserve upstream payload"
    );
    assert_eq!(
        mock_state.mcp_initialize_calls.load(Ordering::SeqCst),
        1,
        "server should record 1 initialize call"
    );

    // 2. `notifications/initialized` (`202 Accepted` empty body -> zero stdout lines) + `tools/list` (`text/event-stream`).
    writeln!(
        child_stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .expect("initialized notification should write");
    writeln!(
        child_stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#
    )
    .expect("tools/list request should write");
    child_stdin.flush().expect("proxy stdin should flush");

    let line2 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for SSE tools/list response")
        .expect("proxy stdout channel closed unexpectedly");
    assert!(
        !line2.trim().is_empty(),
        "HTTP 202 notification must not emit blank lines to stdout"
    );
    let tools_resp: Value =
        serde_json::from_str(&line2).expect("SSE tools/list response must be single-line JSON");
    assert_eq!(tools_resp["id"], 2, "tools/list response id must be 2");
    assert_eq!(
        tools_resp["result"]["tools"][0]["name"], "search",
        "SSE data payload must be forwarded intact"
    );

    // 3. Invalidate `mcp-sess-1` (`404 Not Found`) -> proxy transparently re-initializes once (`mcp-sess-2`) and retries!
    *mock_state
        .mcp_active_session
        .lock()
        .expect("mcp_active_session mutex should not be poisoned") =
        "restarted-invalidated".to_string();

    writeln!(
        child_stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"read","arguments":{{"uri":"viking://resources/doc.md"}}}}}}"#
    )
    .expect("tools/call request should write");
    child_stdin.flush().expect("proxy stdin should flush");

    let line3 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for retried tools/call response")
        .expect("proxy stdout channel closed unexpectedly");
    let call_resp: Value =
        serde_json::from_str(&line3).expect("tools/call stdout frame must be valid JSON");
    assert_eq!(
        call_resp["id"], 3,
        "retried tools/call id must be preserved"
    );
    assert_eq!(
        call_resp["result"]["content"][0]["text"], "Executed read under mcp-sess-2",
        "retried tools/call must execute under newly recovered mcp-sess-2"
    );
    assert_eq!(
        mock_state.mcp_initialize_calls.load(Ordering::SeqCst),
        2,
        "proxy must replay initialize exactly once upon HTTP 404"
    );
    assert_eq!(
        mock_state
            .mcp_initialized_notif_calls
            .load(Ordering::SeqCst),
        2,
        "proxy must replay notifications/initialized during session recovery"
    );

    // 4. Close `stdin` (EOF) -> proxy must issue `DELETE /mcp` with `mcp-session-id: mcp-sess-2`.
    drop(child_stdin);
    let exit_status = task::spawn_blocking(move || child.wait().expect("child wait must succeed"))
        .await
        .expect("child wait task must join");
    assert!(exit_status.success(), "mcp-proxy must exit 0 on stdin EOF");

    let deleted = mock_state
        .mcp_deleted_sessions
        .lock()
        .expect("mcp_deleted_sessions mutex should not be poisoned")
        .clone();
    assert_eq!(
        deleted,
        vec!["mcp-sess-2".to_string()],
        "DELETE /mcp must clean up active mcp-sess-2 on shutdown"
    );
}

// ── Test 6: Root-Key 403 Guard & Offline Fail-Open Guarantee ───────────────

#[tokio::test]
async fn test_6_root_key_guard_and_fail_open_when_server_down() {
    let mock_state = Arc::new(MockServerState::default());
    mock_state.mcp_return_403.store(true, Ordering::SeqCst);
    let server = spawn_mock_ov_server(mock_state).await;
    let base_url = format!("http://{}", server.addr);

    let temp_dir = TempDir::new().expect("tempdir should be created");
    let paths = setup_test_ovcli(temp_dir.path(), &base_url, "root-admin-secret");
    let ov_conf = paths.ov_home.join("ov.conf");
    let ov_conf_body = serde_json::to_string_pretty(&json!({
        "server": {
            "port": server.addr.port(),
            "root_api_key": "root-admin-secret"
        }
    }))
    .expect("ov.conf JSON must serialize");
    fs::write(&ov_conf, ov_conf_body).expect("ov.conf should write");

    let envs_vec = [
        ("HOME".to_string(), temp_dir.path().display().to_string()),
        (
            "OPENVIKING_HOME".to_string(),
            paths.ov_home.display().to_string(),
        ),
        (
            "OPENVIKING_PLUGIN_DATA".to_string(),
            paths.plugin_data.display().to_string(),
        ),
        (
            "OPENVIKING_CLI_CONFIG_FILE".to_string(),
            paths.cli_conf.display().to_string(),
        ),
        (
            "OPENVIKING_CONFIG_FILE".to_string(),
            ov_conf.display().to_string(),
        ),
    ];

    let doc_out = task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = envs_vec
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        run_hook_cmd(&["doctor", "--json"], "", &refs)
    })
    .await
    .expect("doctor blocking task must succeed");

    assert_eq!(
        doc_out.exit_code, 0,
        "doctor must exit 0 even when /mcp 403s"
    );
    let doc: Value =
        serde_json::from_str(doc_out.stdout.trim()).expect("doctor stdout must be valid JSON");
    assert_eq!(
        doc["mcp_status_code"], 403,
        "doctor must report HTTP 403 on /mcp"
    );
    assert!(
        doc["root_key_warning"]
            .as_str()
            .unwrap_or("")
            .contains("ov config"),
        "doctor must surface remediation instructions via `ov config`"
    );
    assert!(
        !doc_out.stdout.contains("root-admin-secret"),
        "doctor output must mask raw API key"
    );

    // Fail-open check when server is unreachable.
    let home_str = temp_dir
        .path()
        .to_str()
        .expect("tempdir path must be UTF-8");
    let dead_envs = [
        ("HOME", home_str),
        ("OPENVIKING_MEMORY_ENABLED", "1"),
        ("OPENVIKING_CREDENTIAL_SOURCE", "env"),
        ("OPENVIKING_URL", "http://127.0.0.1:9"),
        ("OPENVIKING_TIMEOUT_MS", "500"),
    ];
    let dead_out = run_hook_cmd(
        &["hook", "pre-invocation"],
        r#"{"conversationId":"dead-srv","lastUserInput":"test fail open"}"#,
        &dead_envs,
    );
    assert_eq!(
        dead_out.exit_code, 0,
        "hooks must exit 0 when OpenViking server is unreachable"
    );
    assert_eq!(
        dead_out.stdout.trim(),
        "{}",
        "unreachable server must emit safe no-op JSON `{{}}`"
    );
}
