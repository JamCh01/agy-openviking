//! Command-line argument definitions for the `ov-agy` binary.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level CLI entrypoint for `ov-agy`.
#[derive(Parser, Debug)]
#[command(
    name = "ov-agy",
    version,
    about = "OpenViking context, memory, and MCP bridge plugin for Google Antigravity CLI (agy)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Supported `ov-agy` subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Executes an Antigravity CLI lifecycle hook (`stdin` JSON -> `stdout` JSON).
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
    /// Runs the stdio <-> Streamable HTTP (`/mcp`) JSON-RPC bridge.
    #[command(name = "mcp-proxy")]
    McpProxy,
    /// Runs the detached background worker for turn capture, sanitization, and WAL replay.
    Worker(WorkerArgs),
    /// Diagnoses configuration, credentials, connectivity, MCP handshake, peer ID, and WAL state.
    Doctor(DoctorArgs),
}

/// Lifecycle hook events invoked by `agy`.
#[derive(Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    #[command(name = "session-start")]
    SessionStart,
    #[command(name = "pre-invocation")]
    PreInvocation,
    #[command(name = "pre-tool")]
    PreTool,
    #[command(name = "stop")]
    Stop,
}

/// Arguments for `ov-agy worker`.
#[derive(Args, Debug, Clone, Default)]
pub struct WorkerArgs {
    /// Antigravity conversation identifier.
    #[arg(long = "conversation-id")]
    pub conversation_id: String,

    /// Path to the Antigravity `transcript.jsonl` file.
    #[arg(long = "transcript-path")]
    pub transcript_path: Option<PathBuf>,

    /// Workspace directory path used for configuration and Git `peer` derivation.
    #[arg(long = "cwd")]
    pub cwd: Option<PathBuf>,
}

/// Arguments for `ov-agy doctor`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// Emits machine-readable JSON diagnostics.
    #[arg(long)]
    pub json: bool,

    /// Skips live HTTP `/health` and `/mcp` network probes.
    #[arg(long)]
    pub offline: bool,

    /// Per-probe HTTP timeout in milliseconds.
    #[arg(long = "timeout-ms", default_value_t = 3000)]
    pub timeout_ms: u64,

    /// Workspace directory to evaluate (defaults to the current working directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}
