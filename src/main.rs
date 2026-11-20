//! Binary entrypoint for `ov-agy` implementing the two-tier synchronous/async runtime split.

mod cli;
mod client;
mod config;
mod git_peer;
mod hooks;
mod mcp_proxy;
mod wal;
mod wire;
mod worker;

use std::io::{self, Read, Write};
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use serde_json::json;
use tokio::runtime::{Builder, Runtime};

use crate::cli::{Cli, Command, DoctorArgs, HookEvent};
use crate::client::OvClient;
use crate::config::ResolvedConfig;
use crate::wal::WalStore;
use crate::wire::{DoctorReport, InjectOutput};

/// Entrypoint intentionally omits `#[tokio::main]`:
/// - `hook pre-tool` and `hook stop` run on a zero-Tokio synchronous fast path (`< 1ms`).
/// - Async subcommands construct a single-threaded `new_current_thread` Tokio runtime.
fn main() {
    let cli = match Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = err.print();
            std::process::exit(err.exit_code());
        }
    };

    match cli.command {
        Command::Hook { event } => run_hook_fail_open(event),
        Command::McpProxy => {
            if let Ok(runtime) = build_single_thread_runtime() {
                if let Err(err) = runtime.block_on(mcp_proxy::run()) {
                    config::debug_log_from_env(
                        "mcp_proxy",
                        "fatal_error",
                        json!({ "error": err.to_string() }),
                    );
                }
            }
        }
        Command::Worker(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                if let Err(err) = runtime.block_on(worker::run(args)) {
                    config::debug_log_from_env(
                        "worker",
                        "fatal_error",
                        json!({ "error": err.to_string() }),
                    );
                }
            }
        }
        Command::Doctor(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                runtime.block_on(run_doctor(args));
            }
        }
    }
}

fn build_single_thread_runtime() -> io::Result<Runtime> {
    Builder::new_current_thread().enable_all().build()
}

fn read_stdin_string() -> String {
    let mut buf = Vec::new();
    let _ = io::stdin().read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Executes lifecycle hooks with fallible error recovery and safe JSON fallbacks (`exit 0`),
/// without relying on `panic::catch_unwind` (compatible with `panic = "abort"`).
fn run_hook_fail_open(event: HookEvent) {
    let raw_stdin = read_stdin_string();

    match event {
        HookEvent::PreTool => {
            let output = hooks::pre_tool::run_sync(&raw_stdin);
            write_stdout_json(&output, "{\"decision\":\"ask\"}");
        }
        HookEvent::Stop => {
            let out_str = hooks::stop::run_sync(&raw_stdin);
            write_stdout_raw_line(out_str);
        }
        HookEvent::SessionStart => {
            let output = match build_single_thread_runtime() {
                Ok(runtime) => runtime.block_on(hooks::session_start::run(&raw_stdin)),
                Err(err) => {
                    config::debug_log_from_env(
                        "hook.session_start",
                        "runtime_init_error",
                        json!({ "error": err.to_string() }),
                    );
                    InjectOutput::empty()
                }
            };
            write_stdout_json(&output, "{}");
        }
        HookEvent::PreInvocation => {
            let output = match build_single_thread_runtime() {
                Ok(runtime) => runtime.block_on(hooks::pre_invocation::run(&raw_stdin)),
                Err(err) => {
                    config::debug_log_from_env(
                        "hook.pre_invocation",
                        "runtime_init_error",
                        json!({ "error": err.to_string() }),
                    );
                    InjectOutput::empty()
                }
            };
            write_stdout_json(&output, "{}");
        }
    }
}

fn write_stdout_json<T: Serialize>(value: &T, fallback: &str) {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| fallback.to_string());
    write_stdout_raw_line(&serialized);
}

fn write_stdout_raw_line(line: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

async fn run_doctor(args: DoctorArgs) {
    let cfg = ResolvedConfig::load(args.cwd.as_deref());
    let wal = WalStore::new(&cfg);
    let wal_stats = wal.stats();

    let mut warnings = cfg.warnings.clone();
    let mut root_key_warning = cfg.root_key_warning.clone();

    let mut health_ok = None;
    let mut health_latency_ms = None;
    let mut server_version = None;
    let mut server_auth_mode = None;
    let mut mcp_handshake_ok = None;
    let mut mcp_status_code = None;
    let mut mcp_session_id_present = None;

    if !args.offline {
        let probe_timeout = Duration::from_millis(args.timeout_ms.clamp(250, 30_000));
        if let Ok(client) = OvClient::try_new(cfg.clone()) {
            match client.health(probe_timeout).await {
                Ok(probe) => {
                    health_ok = Some(probe.response.healthy.unwrap_or(true));
                    health_latency_ms = Some(probe.latency_ms);
                    server_version = probe.response.version;
                    server_auth_mode = probe.response.auth_mode;
                }
                Err(err) => {
                    health_ok = Some(false);
                    warnings.push(format!("GET /health failed: {err}"));
                }
            }

            let mcp_probe = client.probe_mcp_handshake(probe_timeout).await;
            mcp_handshake_ok = Some(mcp_probe.ok);
            mcp_status_code = mcp_probe.status_code;
            mcp_session_id_present = Some(mcp_probe.has_session_id);
            if mcp_probe.is_forbidden_root_key && root_key_warning.is_none() {
                let msg = "POST /mcp returned 403 Forbidden. OpenViking rejects `server.root_api_key` on `/mcp` and tenant memory endpoints; configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string();
                root_key_warning = Some(msg.clone());
                warnings.push(msg);
            } else if let Some(err_msg) = mcp_probe.error {
                warnings.push(format!("POST /mcp probe failed: {err_msg}"));
            }
        }
    }

    let peer_source = if cfg.peer_id.is_empty() {
        None
    } else {
        Some(
            cfg.peer_resolution
                .as_ref()
                .map_or_else(|| "explicit_config".to_string(), |r| r.source.to_string()),
        )
    };

    let workspace_config_paths: Vec<String> = cfg
        .workspace_config_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    let report = DoctorReport {
        plugin_version: env!("CARGO_PKG_VERSION").to_string(),
        enabled: cfg.enabled,
        enabled_reason: cfg.enabled_reason.clone(),
        credential_source: cfg.credential_source.clone(),
        base_url: cfg.base_url.clone(),
        mcp_url: cfg.mcp_url.clone(),
        api_key_masked: cfg.mask_api_key(),
        has_api_key: !cfg.api_key.is_empty(),
        root_key_warning,
        account: if cfg.account.is_empty() {
            None
        } else {
            Some(cfg.account.clone())
        },
        user: if cfg.user.is_empty() {
            None
        } else {
            Some(cfg.user.clone())
        },
        peer_id: if cfg.peer_id.is_empty() {
            None
        } else {
            Some(cfg.peer_id.clone())
        },
        peer_source,
        git_is_worktree: cfg.peer_resolution.as_ref().is_some_and(|r| r.is_worktree),
        cli_config_path: cfg
            .cli_config_path
            .as_ref()
            .map(|p| p.display().to_string()),
        ov_config_path: cfg.ov_config_path.as_ref().map(|p| p.display().to_string()),
        workspace_config_paths,
        state_dir: cfg.state_dir.display().to_string(),
        pending_wal_entries: wal_stats.entry_count,
        pending_wal_bytes: wal_stats.byte_size,
        health_ok,
        health_latency_ms,
        server_version,
        server_auth_mode,
        mcp_handshake_ok,
        mcp_status_code,
        mcp_session_id_present,
        warnings,
    };

    if args.json {
        if let Ok(pretty) = serde_json::to_string_pretty(&report) {
            write_stdout_raw_line(&pretty);
        }
    } else {
        print_human_doctor_report(&report);
    }
}

// `doctor` (without `--json`) renders human-facing CLI diagnostics directly to the terminal;
// lifecycle hooks and `mcp-proxy` never call this function.
#[allow(clippy::print_stdout)]
fn print_human_doctor_report(report: &DoctorReport) {
    let mut lines = Vec::new();
    lines.push(format!(
        "OpenViking Antigravity Plugin Doctor (v{})",
        report.plugin_version
    ));
    lines.push("==================================================".to_string());
    lines.push(format!(
        "Enabled:            {} ({})",
        report.enabled, report.enabled_reason
    ));
    lines.push(format!("Credential Source:  {}", report.credential_source));
    lines.push(format!("Base URL:           {}", report.base_url));
    lines.push(format!("MCP URL:            {}", report.mcp_url));
    lines.push(format!("API Key:            {}", report.api_key_masked));
    lines.push(format!(
        "Git Peer ID:        {} (source={}, worktree={})",
        report.peer_id.as_deref().unwrap_or("<none>"),
        report.peer_source.as_deref().unwrap_or("none"),
        report.git_is_worktree
    ));
    lines.push(format!("State Directory:    {}", report.state_dir));
    lines.push(format!(
        "Pending Offline WAL: {} entries ({} bytes)",
        report.pending_wal_entries, report.pending_wal_bytes
    ));

    if let Some(ok) = report.health_ok {
        lines.push(format!(
            "GET /health:        {} (latency={}ms, version={}, auth_mode={})",
            if ok { "OK" } else { "FAILED" },
            report
                .health_latency_ms
                .map_or_else(|| "-".to_string(), |m| m.to_string()),
            report.server_version.as_deref().unwrap_or("-"),
            report.server_auth_mode.as_deref().unwrap_or("-")
        ));
    } else {
        lines.push("GET /health:        SKIPPED (--offline)".to_string());
    }

    if let Some(ok) = report.mcp_handshake_ok {
        lines.push(format!(
            "POST /mcp:          {} (status={}, session_id={})",
            if ok { "OK" } else { "FAILED" },
            report
                .mcp_status_code
                .map_or_else(|| "-".to_string(), |c| c.to_string()),
            report.mcp_session_id_present.unwrap_or(false)
        ));
    } else {
        lines.push("POST /mcp:          SKIPPED (--offline)".to_string());
    }

    if let Some(ref rk_warn) = report.root_key_warning {
        lines.push(format!("\n[ROOT KEY REMEDIATION]\n  ! {rk_warn}"));
    }

    if !report.warnings.is_empty() {
        lines.push("\nWarnings:".to_string());
        for warning in &report.warnings {
            lines.push(format!("  - {warning}"));
        }
    }

    println!("{}", lines.join("\n"));
}
