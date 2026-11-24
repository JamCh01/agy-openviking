//! `SessionStart` hook injecting `<openviking-session>` status and the startup context
//! (profiles, memory index, skills catalog).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;

use crate::client::{ClientError, OvClient};
use crate::config::{self, ResolvedConfig};
use crate::startup::{self, PER_REQUEST_TIMEOUT};
use crate::wire::{CommonHookInput, InjectOutput};

/// Below the 5 s hook timeout; the reads run in parallel, each bounded by `PER_REQUEST_TIMEOUT`.
const SESSION_START_BUDGET: Duration = Duration::from_secs(4);

/// Executes `SessionStart` within `SESSION_START_BUDGET`.
pub async fn run(raw_stdin: &str) -> InjectOutput {
    match tokio::time::timeout(SESSION_START_BUDGET, Box::pin(run_inner(raw_stdin))).await {
        Ok(output) => output,
        Err(_) => InjectOutput::empty(),
    }
}

async fn run_inner(raw_stdin: &str) -> InjectOutput {
    let parsed: CommonHookInput = serde_json::from_str(raw_stdin).unwrap_or_default();
    let ws_path = parsed.effective_workspace().map(Path::new);
    let cfg = ResolvedConfig::load(ws_path);

    if !cfg.enabled {
        cfg.log_debug(
            "hook.session_start",
            "disabled",
            json!({ "reason": cfg.enabled_reason }),
        );
        return InjectOutput::empty();
    }

    start_idle_sweep(
        &cfg,
        parsed.conversation_id.as_deref(),
        parsed.effective_workspace(),
    );
    if cfg.is_bypassed(
        parsed.conversation_id.as_deref(),
        parsed.effective_workspace(),
    ) {
        cfg.log_debug("hook.session_start", "bypassed", json!({}));
        return InjectOutput::empty();
    }

    let Ok(client) = OvClient::try_new(cfg.clone()) else {
        return InjectOutput::empty();
    };

    let (health_res, status_res, startup) = tokio::join!(
        client.health(PER_REQUEST_TIMEOUT),
        client.system_status(PER_REQUEST_TIMEOUT),
        startup::build(&client, &cfg),
    );

    let mut root_key_403 = cfg.root_key_warning.clone();
    if root_key_403.is_none()
        && status_res.as_ref().err().and_then(ClientError::status_code) == Some(403)
    {
        root_key_403 = Some(
            "OpenViking returned HTTP 403 Forbidden (likely `server.root_api_key` used as client key). Configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string(),
        );
    }

    let Ok(health_probe) = health_res else {
        if let Some(warning) = root_key_403 {
            return InjectOutput::new_system(format!(
                "<openviking-session>\nstatus: warning\nwarning: {warning}\n</openviking-session>"
            ));
        }
        cfg.log_debug("hook.session_start", "health_unreachable", json!({}));
        return InjectOutput::empty();
    };

    let resolved_user = status_res
        .ok()
        .and_then(|s| s.user)
        .or(health_probe.response.user_id)
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg.user.is_empty() {
                "authenticated".to_string()
            } else {
                cfg.user.clone()
            }
        });
    let server_ver = health_probe
        .response
        .version
        .unwrap_or_else(|| "unknown".to_string());
    let peer_label = if cfg.peer_id.is_empty() {
        "none"
    } else {
        cfg.peer_id.as_str()
    };

    let mut session_lines = vec![
        "<openviking-session>".to_string(),
        format!(
            "status: connected (url={}, version={}, latency={}ms)",
            cfg.base_url, server_ver, health_probe.latency_ms
        ),
        format!("identity: user={resolved_user}, peer={peer_label}"),
    ];
    if let Some(conv_id) = parsed
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        session_lines.push(format!("session_id: agy-{conv_id}"));
    }
    if let Some(warning) = root_key_403 {
        session_lines.push(format!("warning: {warning}"));
    }
    session_lines.push("</openviking-session>".to_string());

    let mut sections = vec![session_lines.join("\n")];
    let mut stats = json!({});
    if let Some(block) = startup {
        sections.push(startup::wrap("startup", &block.text));
        stats = block.stats;
    }
    let message = sections.join("\n\n");
    record_injection(&cfg, "startup", &message);
    cfg.log_debug(
        "hook.session_start",
        "injected",
        json!({ "chars": message.len(), "peer": cfg.peer_id, "startup": stats }),
    );
    InjectOutput::new_system(message)
}

/// Keeps a copy of the last injected context for `ov-agy status` and audits, like the Claude
/// Code plugin's `last_inject.md`.
pub(crate) fn record_injection(cfg: &ResolvedConfig, source: &str, message: &str) {
    let body = format!(
        "<!-- source={source} at={} -->\n{message}\n",
        config::now_epoch_ms()
    );
    let path = cfg.state_dir.join("last-inject.md");
    if config::ensure_private_dir(&cfg.state_dir).is_ok() {
        if let Ok(mut file) =
            config::open_private_file(&path, config::PrivateOpenMode::TruncateWrite)
        {
            let _ = std::io::Write::write_all(&mut file, body.as_bytes());
        }
    }
}

/// agy has no `SessionEnd`: a new session is the moment to commit conversations that went
/// idle. The sweep runs detached so it never delays the session that is starting.
fn start_idle_sweep(cfg: &ResolvedConfig, conversation_id: Option<&str>, workspace: Option<&str>) {
    if !cfg.auto_capture || !cfg.idle_sweep || !crate::sweep::claim_sweep_slot(cfg) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("sweep");
    if let Some(id) = conversation_id.map(str::trim).filter(|id| !id.is_empty()) {
        cmd.arg("--exclude").arg(id);
    }
    if let Some(ws) = workspace {
        cmd.arg("--cwd").arg(ws);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::stop::detach_from_hook_process_group(&mut cmd);
    let spawned = cmd.spawn().map(|child| child.id());
    cfg.log_debug(
        "hook.session_start",
        "sweep_spawned",
        json!({ "pid": spawned.as_ref().ok(), "error": spawned.err().map(|e| e.to_string()) }),
    );
}
