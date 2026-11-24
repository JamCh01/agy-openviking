//! Binary entrypoint for `ov-agy` implementing the two-tier synchronous/async runtime split.

mod cli;
mod client;
mod config;
mod doctor;
mod git_peer;
mod hooks;
mod mcp_proxy;
mod setup;
mod startup;
mod status;
mod sweep;
mod wal;
mod wire;
mod worker;

use std::io::{self, Read, Write};

use clap::Parser;
use serde::Serialize;
use serde_json::json;
use tokio::runtime::{Builder, Runtime};

use crate::cli::{Cli, Command, HookEvent};
use crate::wire::InjectOutput;

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
        Command::Sweep(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                if let Err(err) = runtime.block_on(sweep::run(args)) {
                    config::debug_log_from_env(
                        "sweep",
                        "fatal_error",
                        json!({ "error": err.to_string() }),
                    );
                }
            }
        }
        Command::Doctor(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                let code = runtime.block_on(doctor::run(args));
                std::process::exit(code);
            }
        }
        Command::Status(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                let code = runtime.block_on(status::run(args));
                std::process::exit(code);
            }
        }
        Command::Setup(args) => {
            if let Ok(runtime) = build_single_thread_runtime() {
                let code = runtime.block_on(setup::run(args));
                std::process::exit(code);
            }
        }
    }
}

const MAX_HOOK_STDIN_BYTES: u64 = 16 * 1024 * 1024;

fn build_single_thread_runtime() -> io::Result<Runtime> {
    Builder::new_current_thread().enable_all().build()
}

fn read_stdin_string() -> String {
    let mut buf = Vec::new();
    let _ = io::stdin().take(MAX_HOOK_STDIN_BYTES).read_to_end(&mut buf);
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
        HookEvent::PostInvocation => {
            let output = hooks::post_invocation::run_sync(&raw_stdin);
            write_stdout_json(&output, "{}");
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
