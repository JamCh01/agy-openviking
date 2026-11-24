//! `ov-agy status`: one screen of what the plugin does right now, like the Claude Code
//! plugin's `/ov` command: server, identity, toggles, and the last injection, recall and capture.

use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;

use crate::cli::StatusArgs;
use crate::client::OvClient;
use crate::config::{self, ResolvedConfig};
use crate::doctor::describe_age;
use crate::wal::WalStore;
use crate::wire::{LastCaptureSnapshot, LastRecallSnapshot};

/// Short enough to answer at once when the server is down.
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Default, Serialize)]
struct ServerStatus {
    url: String,
    checked: bool,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LastInject {
    at_ms: u64,
    bytes: u64,
    source: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    version: String,
    enabled: bool,
    enabled_reason: String,
    bypassed: bool,
    server: ServerStatus,
    peer: String,
    peer_source: String,
    auto_recall: bool,
    recall_limit: usize,
    recall_timeout_ms: u64,
    auto_capture: bool,
    capture_tool_calls: bool,
    commit_token_threshold: u64,
    usage_view: String,
    usage_output: String,
    auto_commit_on_compact: bool,
    idle_sweep: bool,
    pending_wal_entries: usize,
    data_dir: String,
    last_inject: Option<LastInject>,
    last_recall: Option<LastRecallSnapshot>,
    last_capture: Option<LastCaptureSnapshot>,
}

/// Prints the status and returns the exit code (always 0; `doctor` is the pass/fail check).
pub async fn run(args: StatusArgs) -> i32 {
    let cwd = args.cwd.clone().or_else(|| std::env::current_dir().ok());
    let cfg = ResolvedConfig::load(cwd.as_deref());
    let mut server = ServerStatus {
        url: cfg.base_url.clone(),
        ..ServerStatus::default()
    };
    if !args.offline && cfg.enabled {
        probe(&cfg, &mut server).await;
    }
    let report = StatusReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        enabled: cfg.enabled,
        enabled_reason: cfg.enabled_reason.clone(),
        bypassed: cfg.is_bypassed(None, cwd.as_deref().and_then(|p| p.to_str())),
        server,
        peer: cfg.peer_id.clone(),
        peer_source: cfg.peer_origin.clone(),
        auto_recall: cfg.auto_recall,
        recall_limit: cfg.recall_limit,
        recall_timeout_ms: cfg.recall_timeout_ms,
        auto_capture: cfg.auto_capture,
        capture_tool_calls: cfg.capture_tool_calls,
        commit_token_threshold: cfg.commit_token_threshold,
        usage_view: cfg.usage_view.clone(),
        usage_output: cfg.usage_output.clone(),
        auto_commit_on_compact: cfg.auto_commit_on_compact,
        idle_sweep: cfg.idle_sweep,
        pending_wal_entries: WalStore::new(&cfg).stats().entry_count,
        data_dir: cfg.state_dir.display().to_string(),
        last_inject: last_inject(&cfg),
        last_recall: config::read_json_optional(&cfg.last_recall_path()),
        last_capture: config::read_json_optional(&cfg.last_capture_path()),
    };
    if args.json {
        if let Ok(pretty) = serde_json::to_string_pretty(&report) {
            crate::write_stdout_raw_line(&pretty);
        }
    } else {
        crate::write_stdout_raw_line(&render(&report));
    }
    0
}

async fn probe(cfg: &ResolvedConfig, server: &mut ServerStatus) {
    server.checked = true;
    let Ok(client) = OvClient::try_new(cfg.clone()) else {
        return;
    };
    match client.health(STATUS_PROBE_TIMEOUT).await {
        Ok(health) => {
            server.reachable = true;
            server.latency_ms = Some(health.latency_ms);
            server.version = health.response.version;
            server.account = health.response.account_id;
            server.user = health.response.user_id;
            server.role = health.response.role;
        }
        Err(err) => server.error = Some(err.to_string()),
    }
}

/// `last-inject.md` starts with `<!-- source=<startup|compaction> at=<ms> -->`.
fn last_inject(cfg: &ResolvedConfig) -> Option<LastInject> {
    let path = cfg.state_dir.join("last-inject.md");
    let meta = fs::metadata(&path).ok()?;
    let head = fs::read_to_string(&path).ok()?;
    let first = head.lines().next().unwrap_or_default();
    let field = |key: &str| {
        first
            .split_whitespace()
            .find_map(|part| part.strip_prefix(key))
            .map(str::to_string)
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    Some(LastInject {
        at_ms: field("at=")
            .and_then(|v| v.parse().ok())
            .unwrap_or(modified),
        bytes: meta.len(),
        source: field("source=").unwrap_or_else(|| "startup".to_string()),
        path: path.display().to_string(),
    })
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "off"
    }
}

fn render(r: &StatusReport) -> String {
    let mut lines = vec![format!(
        "OpenViking for agy v{}: {} ({}){}",
        r.version,
        if r.enabled { "enabled" } else { "DISABLED" },
        r.enabled_reason,
        if r.bypassed {
            ", bypassed in this directory"
        } else {
            ""
        }
    )];
    let s = &r.server;
    let server = if !s.checked {
        format!("{} (not checked)", s.url)
    } else if s.reachable {
        format!(
            "{} · reachable · v{} · {} ms",
            s.url,
            s.version
                .as_deref()
                .map_or("?", |v| v.trim_start_matches('v')),
            s.latency_ms.unwrap_or(0)
        )
    } else {
        format!(
            "{} · UNREACHABLE: {}",
            s.url,
            s.error.as_deref().unwrap_or("no answer")
        )
    };
    lines.push(format!("  server    {server}"));
    let identity = match (&s.user, &s.account) {
        (Some(user), Some(account)) => format!(
            "user={user} account={account}{}",
            s.role
                .as_deref()
                .map_or_else(String::new, |role| format!(" role={role}"))
        ),
        _ => "unknown (server not checked)".to_string(),
    };
    let peer = if r.peer.is_empty() { "none" } else { &r.peer };
    lines.push(format!(
        "  identity  {identity} · peer {peer} ({})",
        r.peer_source
    ));

    let recall = r.last_recall.as_ref().map_or_else(
        || "never".to_string(),
        |last| {
            format!(
                "{} {}, {} chars, {} ms",
                if last.outcome.is_empty() {
                    "recorded"
                } else {
                    &last.outcome
                },
                describe_age(last.timestamp_ms),
                last.injected_chars,
                last.latency_ms
            )
        },
    );
    lines.push(format!(
        "  recall    {} · limit {} · {} ms · last: {recall}",
        on_off(r.auto_recall),
        r.recall_limit,
        r.recall_timeout_ms
    ));
    let capture = r.last_capture.as_ref().map_or_else(
        || "never".to_string(),
        |last| {
            format!(
                "{} captured, {} queued, {} rejected, commit {}{} {}",
                last.turns_captured,
                last.turns_queued,
                last.turns_failed,
                last.commit,
                last.pending_tokens
                    .map_or_else(String::new, |p| format!(" ({p}/{})", last.commit_threshold)),
                describe_age(last.timestamp_ms)
            )
        },
    );
    lines.push(format!(
        "  capture   {} · tool calls {} · commit at {} tokens · last: {capture}",
        on_off(r.auto_capture),
        on_off(r.capture_tool_calls),
        r.commit_token_threshold
    ));
    let inject = r.last_inject.as_ref().map_or_else(
        || "never".to_string(),
        |last| {
            format!(
                "{} context {}, {} bytes ({})",
                last.source,
                describe_age(last.at_ms),
                last.bytes,
                last.path
            )
        },
    );
    lines.push(format!("  inject    {inject}"));
    lines.push(format!(
        "  queue     {} pending · data {}",
        r.pending_wal_entries, r.data_dir
    ));
    lines.push(format!(
        "  toggles   usage {}/{} · commit on compaction {} · idle sweep {}",
        r.usage_view,
        r.usage_output,
        on_off(r.auto_commit_on_compact),
        on_off(r.idle_sweep)
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn renders_the_last_activity_lines() {
        let report = StatusReport {
            version: "0.0.0".into(),
            enabled: true,
            enabled_reason: "file:ovcli.conf".into(),
            bypassed: false,
            server: ServerStatus {
                url: "http://127.0.0.1:1933".into(),
                checked: true,
                reachable: false,
                error: Some("connection refused".into()),
                ..ServerStatus::default()
            },
            peer: String::new(),
            peer_source: "none".into(),
            auto_recall: true,
            recall_limit: 6,
            recall_timeout_ms: 3800,
            auto_capture: true,
            capture_tool_calls: true,
            commit_token_threshold: 20_000,
            usage_view: "summary".into(),
            usage_output: "notice".into(),
            auto_commit_on_compact: true,
            idle_sweep: true,
            pending_wal_entries: 2,
            data_dir: "/tmp/data".into(),
            last_inject: None,
            last_recall: None,
            last_capture: Some(LastCaptureSnapshot {
                timestamp_ms: config::now_epoch_ms(),
                conversation_id: "c".into(),
                ov_session_id: "agy-c".into(),
                finalize: false,
                turns_captured: 3,
                turns_queued: 0,
                turns_failed: 0,
                batches: 1,
                pending_tokens: Some(4210),
                commit_threshold: 20_000,
                commit: "pending".into(),
                error: None,
            }),
        };
        let text = render(&report);
        assert!(text.contains("UNREACHABLE: connection refused"), "{text}");
        assert!(text.contains("3 captured, 0 queued, 0 rejected, commit pending (4210/20000)"));
        assert!(text.contains("queue     2 pending"));
        let _: Value = serde_json::to_value(&report).unwrap();
    }
}
