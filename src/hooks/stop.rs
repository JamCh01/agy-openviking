//! Synchronous `Stop` hook launcher spawning the detached `ov-agy worker` process.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use crate::config::{self, ResolvedConfig};
use crate::wire::StopInput;

/// Evaluates `Stop` synchronously and spawns `ov-agy worker` with null stdio streams when idle.
#[must_use]
pub fn run_sync(raw_stdin: &str) -> &'static str {
    let parsed: StopInput = match serde_json::from_str(raw_stdin) {
        Ok(val) => val,
        Err(err) => {
            config::debug_log_from_env(
                "hook.stop",
                "parse_error",
                json!({ "error": err.to_string() }),
            );
            return "{}";
        }
    };

    let fully_idle = parsed.fully_idle.unwrap_or(false);
    let is_error_stop = parsed
        .termination_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().eq_ignore_ascii_case("error"));

    if !fully_idle || is_error_stop {
        config::debug_log_from_env(
            "hook.stop",
            "skip_not_idle_or_error",
            json!({
                "fullyIdle": parsed.fully_idle,
                "terminationReason": parsed.termination_reason,
            }),
        );
        return "{}";
    }

    let workspace_path = parsed.common.effective_workspace().map(Path::new);
    let cfg = ResolvedConfig::load(workspace_path);
    if !cfg.enabled || !cfg.auto_capture {
        cfg.log_debug(
            "hook.stop",
            "skip_disabled",
            json!({
                "enabled": cfg.enabled,
                "autoCapture": cfg.auto_capture,
                "reason": cfg.enabled_reason,
            }),
        );
        return "{}";
    }

    let Some(conversation_id) = parsed
        .common
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        cfg.log_debug("hook.stop", "skip_missing_conversation_id", json!({}));
        return "{}";
    };

    let Ok(exe_path) = std::env::current_exe() else {
        cfg.log_debug("hook.stop", "current_exe_failed", json!({}));
        return "{}";
    };

    let mut cmd = Command::new(exe_path);
    cmd.arg("worker")
        .arg("--conversation-id")
        .arg(conversation_id);

    if let Some(transcript_path) = parsed
        .common
        .transcript_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cmd.arg("--transcript-path").arg(transcript_path);
    }

    if let Some(ws) = parsed.common.effective_workspace() {
        cmd.arg("--cwd").arg(ws);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match cmd.spawn() {
        Ok(child) => {
            cfg.log_debug(
                "hook.stop",
                "spawned_worker",
                json!({
                    "worker_pid": child.id(),
                    "conversation_id": conversation_id,
                }),
            );
        }
        Err(err) => {
            cfg.log_debug(
                "hook.stop",
                "spawn_worker_error",
                json!({ "error": err.to_string() }),
            );
        }
    }

    "{}"
}
