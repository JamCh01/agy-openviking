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
    /// Commits idle conversations and retires old state; started detached by `SessionStart`,
    /// because agy has no `SessionEnd` hook.
    Sweep(SweepArgs),
    /// Diagnoses the install, configuration, credentials, server, MCP endpoint and recent
    /// activity. Exits 1 when a check fails (`--json` always exits 0 and reports `ok`).
    Doctor(DoctorArgs),
    /// Shows what the plugin does right now: server, identity, toggles, and the last
    /// injection, recall and capture.
    Status(StatusArgs),
    /// Writes the server url and api key to `ovcli.conf` (merged, backed up, mode 0600)
    /// after checking them against the server.
    Setup(SetupArgs),
}

/// Lifecycle hook events invoked by `agy`.
#[derive(Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    #[command(name = "session-start")]
    SessionStart,
    #[command(name = "pre-invocation")]
    PreInvocation,
    #[command(name = "post-invocation")]
    PostInvocation,
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

    /// Commits whatever is uncommitted, regardless of the token threshold. Used by the sweep
    /// for conversations that went idle, and after a compaction.
    #[arg(long)]
    pub finalize: bool,

    /// Set by the in-process idle sweep, which skips a conversation whose lock is held instead
    /// of waiting for it; not a command-line flag.
    #[arg(skip)]
    pub from_sweep: bool,
}

/// Arguments for `ov-agy sweep`.
#[derive(Args, Debug, Clone, Default)]
pub struct SweepArgs {
    /// Conversation that is starting now; it is never swept.
    #[arg(long = "exclude")]
    pub exclude: Option<String>,

    /// Workspace of the starting session, used for its own offline-queue replay.
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

/// Arguments for `ov-agy status`.
#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// Emits machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Skips the `/health` probe.
    #[arg(long)]
    pub offline: bool,

    /// Workspace directory to evaluate (defaults to the current working directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

/// Arguments for `ov-agy setup`. Without flags on a terminal it asks for each value.
#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Server root URL, e.g. `http://127.0.0.1:1933`.
    #[arg(long)]
    pub url: Option<String>,

    /// Reads the api key from the first line of stdin (keeps it out of shell history).
    #[arg(long = "api-key-stdin", conflicts_with = "api_key_env")]
    pub api_key_stdin: bool,

    /// Reads the api key from this environment variable.
    #[arg(long = "api-key-env", value_name = "VAR")]
    pub api_key_env: Option<String>,

    /// Account for servers in trusted mode.
    #[arg(long)]
    pub account: Option<String>,

    /// User for servers in trusted mode.
    #[arg(long)]
    pub user: Option<String>,

    /// Writes without checking the values against the server.
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Never prompts; missing values keep what `ovcli.conf` already has.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Shows the merged file (key masked) without writing it.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}
