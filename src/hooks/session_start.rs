//! `SessionStart` hook injecting `<openviking-session>` and `<available-skills>` context.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::client::{ClientError, OvClient};
use crate::config::ResolvedConfig;
use crate::wire::{CommonHookInput, InjectOutput, OvFsEntry};

const SESSION_START_BUDGET: Duration = Duration::from_millis(1500);
const PER_REQUEST_TIMEOUT: Duration = Duration::from_millis(1200);

/// Executes `SessionStart` within a strict `1500ms` timeout budget.
pub async fn run(raw_stdin: &str) -> InjectOutput {
    match tokio::time::timeout(SESSION_START_BUDGET, run_inner(raw_stdin)).await {
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

    let Ok(client) = OvClient::try_new(cfg.clone()) else {
        return InjectOutput::empty();
    };

    let (health_res, status_res, user_skills_res, shared_skills_res) = tokio::join!(
        client.health(PER_REQUEST_TIMEOUT),
        client.system_status(PER_REQUEST_TIMEOUT),
        client.list_skills("viking://~/skills", PER_REQUEST_TIMEOUT),
        client.list_skills("viking://agent/skills", PER_REQUEST_TIMEOUT),
    );

    let mut root_key_403 = cfg.root_key_warning.clone();
    let error_codes = [
        status_res.as_ref().err().and_then(ClientError::status_code),
        user_skills_res
            .as_ref()
            .err()
            .and_then(ClientError::status_code),
    ];
    for code in error_codes.into_iter().flatten() {
        if code == 403 && root_key_403.is_none() {
            root_key_403 = Some(
                "OpenViking returned HTTP 403 Forbidden (likely `server.root_api_key` used as client key). Configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string(),
            );
        }
    }

    let Ok(health_probe) = health_res else {
        if let Some(warning) = root_key_403 {
            let block = format!(
                "<openviking-session>\nstatus: warning\nwarning: {warning}\n</openviking-session>"
            );
            return InjectOutput::new_ephemeral(block);
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
    if let Some(ref conv_id) = parsed.conversation_id {
        if !conv_id.trim().is_empty() {
            session_lines.push(format!("session_id: agy-{}", conv_id.trim()));
        }
    }
    if let Some(warning) = root_key_403 {
        session_lines.push(format!("warning: {warning}"));
    }
    session_lines.push("</openviking-session>".to_string());

    let mut combined_sections = vec![session_lines.join("\n")];

    let skills_block = format_available_skills(
        user_skills_res.unwrap_or_default(),
        shared_skills_res.unwrap_or_default(),
    );
    if let Some(sb) = skills_block {
        combined_sections.push(sb);
    }

    let message = combined_sections.join("\n\n");
    cfg.log_debug(
        "hook.session_start",
        "injected",
        json!({ "chars": message.len(), "peer": cfg.peer_id }),
    );
    InjectOutput::new_ephemeral(message)
}

fn format_available_skills(
    user_entries: Vec<OvFsEntry>,
    shared_entries: Vec<OvFsEntry>,
) -> Option<String> {
    let mut seen_names = HashSet::new();
    let mut lines = Vec::new();

    for (scope_uri, entries) in [
        ("viking://~/skills", user_entries),
        ("viking://agent/skills", shared_entries),
    ] {
        for entry in entries {
            if entry.access.as_deref() == Some("denied") {
                continue;
            }
            let name = entry
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty() && !n.starts_with('.'))
                .or_else(|| {
                    entry
                        .uri
                        .as_deref()
                        .and_then(|u| u.trim_end_matches('/').rsplit('/').next())
                        .filter(|n| !n.is_empty() && !n.starts_with('.'))
                });
            let Some(skill_name) = name else {
                continue;
            };
            if !seen_names.insert(skill_name.to_string()) {
                continue;
            }
            let canonical_uri = entry
                .uri
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{scope_uri}/{skill_name}"));
            let summary = entry
                .abstract_text
                .as_deref()
                .or(entry.overview.as_deref())
                .map(|s| s.lines().next().unwrap_or("").trim())
                .unwrap_or("");

            if summary.is_empty() {
                lines.push(format!("- {skill_name} ({canonical_uri})"));
            } else {
                let short_desc: String = summary.chars().take(160).collect();
                lines.push(format!("- {skill_name} ({canonical_uri}): {short_desc}"));
            }
            if lines.len() >= 25 {
                break;
            }
        }
        if lines.len() >= 25 {
            break;
        }
    }

    if lines.is_empty() {
        None
    } else {
        let joined = lines.join("\n");
        Some(format!(
            "<available-skills>\nRemote skills in OpenViking (read `<uri>/SKILL.md` via OpenViking MCP `read` tool):\n{joined}\n</available-skills>"
        ))
    }
}
