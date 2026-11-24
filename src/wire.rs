//! Wire protocol types for Antigravity CLI hooks, `OpenViking` REST APIs, and local state.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Common metadata fields sent by `agy` to lifecycle hooks on `stdin`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommonHookInput {
    #[serde(rename = "conversationId", alias = "session_id", default)]
    pub conversation_id: Option<String>,

    #[serde(rename = "workspacePaths", default)]
    pub workspace_paths: Vec<String>,

    #[serde(rename = "transcriptPath", alias = "transcript_path", default)]
    pub transcript_path: Option<String>,

    #[serde(rename = "lastUserInput", default)]
    pub last_user_input: Option<String>,

    #[serde(default)]
    pub cwd: Option<String>,
}

impl CommonHookInput {
    /// Returns the primary workspace path from `workspacePaths[0]`, falling back to `cwd`.
    #[must_use]
    pub fn effective_workspace(&self) -> Option<&str> {
        self.workspace_paths
            .first()
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| self.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    }
}

/// `stdin` payload for the `PreInvocation` hook.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreInvocationInput {
    #[serde(flatten)]
    pub common: CommonHookInput,

    #[serde(rename = "invocationNum", default)]
    pub invocation_num: Option<u64>,
    #[serde(rename = "initialNumSteps", default)]
    pub initial_num_steps: Option<u64>,
}

/// Nested tool invocation descriptor inside `PreToolUse` payloads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolCallPayload {
    #[serde(default)]
    pub name: Option<String>,

    #[serde(default)]
    pub args: Value,
}

/// `stdin` payload for the `PreToolUse` hook.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreToolInput {
    #[serde(rename = "toolCall", default)]
    pub tool_call: Option<ToolCallPayload>,

    #[serde(rename = "toolName", alias = "tool_name", default)]
    pub tool_name: Option<String>,

    #[serde(rename = "toolInput", alias = "tool_input", alias = "args", default)]
    pub tool_input: Value,
}

impl PreToolInput {
    /// Returns the resolved tool name from either `toolCall.name` or flat `toolName`.
    #[must_use]
    pub fn effective_tool_name(&self) -> &str {
        if let Some(ref tool_call) = self.tool_call {
            if let Some(ref name) = tool_call.name {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
        self.tool_name.as_deref().map_or("", str::trim)
    }
}

/// `stdin` payload for the `Stop` hook.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StopInput {
    #[serde(flatten)]
    pub common: CommonHookInput,

    #[serde(rename = "terminationReason", default)]
    pub termination_reason: Option<String>,

    #[serde(rename = "fullyIdle", default)]
    pub fully_idle: Option<bool>,
}

/// Safe Antigravity injection step using `systemMessage`.
///
/// Intentionally omits `systemMessage.metadata`: agy 1.5.16 hides a step that carries
/// `metadata.user_markdown` from the model entirely, even when `systemMessage` is non-empty,
/// and older builds panicked in `proto_utils.go`. `decision` is never emitted on
/// `SessionStart`/`PreInvocation`/`Stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemStep {
    #[serde(rename = "systemMessage")]
    pub system_message: SystemMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemMessage {
    #[serde(rename = "systemMessage")]
    pub text: String,
    /// Only for user-facing notices: `{"user_markdown": ...}` renders inline for the user and
    /// hides the whole step from the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<UserMarkdown>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMarkdown {
    pub user_markdown: String,
}

/// `stdout` response envelope for `SessionStart` and `PreInvocation` hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectOutput {
    #[serde(rename = "injectSteps", skip_serializing_if = "Vec::is_empty")]
    pub inject_steps: Vec<SystemStep>,
}

impl InjectOutput {
    /// Constructs a no-op `{}` hook response.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inject_steps: Vec::new(),
        }
    }

    /// Constructs an injection response containing a single `systemMessage` step.
    #[must_use]
    pub fn new_system(content: String) -> Self {
        if content.trim().is_empty() {
            Self::empty()
        } else {
            Self {
                inject_steps: vec![SystemStep {
                    system_message: SystemMessage {
                        text: content,
                        metadata: None,
                    },
                }],
            }
        }
    }

    /// A step only the user sees: agy renders `user_markdown` inline and the model gets nothing.
    #[must_use]
    pub fn user_notice(markdown: String) -> Self {
        Self {
            inject_steps: vec![SystemStep {
                system_message: SystemMessage {
                    text: String::new(),
                    metadata: Some(UserMarkdown {
                        user_markdown: markdown,
                    }),
                },
            }],
        }
    }
}

/// Policy decision returned by `PreToolUse`.
///
/// `allow` is deliberately absent: it auto-approves the call and would bypass the user's own
/// permission rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreToolDecision {
    Deny,
    Ask,
}

/// `stdout` response payload for `PreToolUse`. agy 1.5.16 denies the call when `decision`
/// is missing or empty, so `{}` must never be emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolOutput {
    pub decision: PreToolDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PreToolOutput {
    /// Hands the call to the user's normal permission flow; no prompt appears unless the
    /// user's mode or rules would have asked anyway.
    #[must_use]
    pub fn ask() -> Self {
        Self {
            decision: PreToolDecision::Ask,
            reason: None,
        }
    }

    /// Blocks tool execution with an explanatory message.
    #[must_use]
    pub fn deny(reason: &str) -> Self {
        Self {
            decision: PreToolDecision::Deny,
            reason: Some(reason.to_string()),
        }
    }
}

// ── OpenViking HTTP REST Wire Types ────────────────────────────────────────

/// Error payload inside an `OpenViking` JSON response envelope.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvApiError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Standard `OpenViking` HTTP JSON response wrapper.
#[derive(Debug, Clone, Deserialize)]
pub struct OvEnvelope<T> {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub result: Option<T>,
    #[serde(default)]
    pub error: Option<OvApiError>,
}

/// Response body from `GET /health`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OvHealthResponse {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub healthy: Option<bool>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

/// Result payload from `GET /api/v1/system/status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvSystemStatusResult {
    #[serde(default)]
    pub user: Option<String>,
}

/// Directory or file entry returned by `GET /api/v1/fs/ls`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvFsEntry {
    #[serde(default, rename = "isDir")]
    pub is_dir: bool,
    #[serde(default)]
    pub rel_path: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(rename = "abstract", default)]
    pub abstract_text: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
}

/// Request body for `POST /api/v1/search/search` (`mode: "context"`).
#[derive(Debug, Clone, Serialize)]
pub struct OvContextSearchRequest {
    pub query: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quotas: Option<Value>,
    pub purpose: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_scope: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_uris: Vec<String>,
    /// `off` skips the server's query-expansion step, which a short hook budget cannot wait for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_expansion: Option<String>,
}

/// Fallback request body for `POST /api/v1/search/find` on legacy servers.
#[derive(Debug, Clone, Serialize)]
pub struct OvFindRequest {
    pub query: String,
    pub target_uri: String,
    pub limit: usize,
    pub score_threshold: f64,
}

/// Context assembly statistics inside `OvSearchResult`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvContextStats {
    #[serde(default)]
    pub rewrite: Option<String>,
}

/// Context entry returned in `mode: "context"` search responses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvContextEntry {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(rename = "abstract", default)]
    pub abstract_text: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Ranked hit item returned in `mode: "list"` or `find` responses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvMatchedItem {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub context_type: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(rename = "abstract", default)]
    pub abstract_text: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Unified search result supporting both `mode: "context"` and `mode: "list"` payloads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvSearchResult {
    #[serde(default)]
    pub rendered: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub entries: Vec<OvContextEntry>,
    #[serde(default)]
    pub stats: Option<OvContextStats>,
    #[serde(default)]
    pub memories: Vec<OvMatchedItem>,
    #[serde(default)]
    pub resources: Vec<OvMatchedItem>,
    #[serde(default)]
    pub skills: Vec<OvMatchedItem>,
}

/// Single message part in `OpenViking` session message payloads.
///
/// Tool parts carry one finished call: `tool_input` from the agy step's `tool_calls`
/// entry and `tool_output` from the `GENERIC` step that answered it, matching the
/// combined `{"type": "tool", ..., "tool_status": "completed", "tool_output": ...}`
/// form in the sessions API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OvMessagePart {
    Text {
        text: String,
    },
    Tool {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_id: Option<String>,
        tool_name: String,
        /// `completed` or `error`.
        tool_status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_input: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_output: Option<String>,
    },
}

impl OvMessagePart {
    /// Creates a `{"type": "text", "text": ...}` message part.
    #[must_use]
    pub fn new_text(content: String) -> Self {
        Self::Text { text: content }
    }

    /// Serialized size used to estimate uncommitted tokens when the server has no meta.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Tool {
                tool_name,
                tool_input,
                tool_output,
                ..
            } => {
                tool_name.len()
                    + tool_input.as_ref().map_or(0, |v| v.to_string().len())
                    + tool_output.as_ref().map_or(0, String::len)
            }
        }
    }
}

/// Request body for `POST /api/v1/sessions/{session_id}/messages`, and one element of
/// `messages` in `POST /api/v1/sessions/{session_id}/messages/batch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OvAddMessageRequest {
    pub role: String,
    pub parts: Vec<OvMessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
}

/// Request body for `POST /api/v1/sessions/{session_id}/messages/batch` (at most 100).
#[derive(Debug, Clone, Serialize)]
pub struct OvBatchAddMessagesRequest<'a> {
    pub messages: &'a [OvAddMessageRequest],
}

/// `result` of `GET /api/v1/skills`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvSkillList {
    #[serde(default)]
    pub skills: Vec<OvSkillItem>,
}

/// One skill package from `GET /api/v1/skills`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvSkillItem {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Session metadata from `GET /api/v1/sessions/{session_id}`; only what the commit policy reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OvSessionMeta {
    #[serde(default)]
    pub pending_tokens: Option<u64>,
}

/// Request body for `POST /api/v1/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvCreateSessionRequest {
    pub session_id: String,
}

/// Request body for `POST /api/v1/sessions/{session_id}/commit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvCommitRequest {
    pub keep_recent_count: usize,
}

// ── Persistent State & Diagnostic Snapshots ────────────────────────────────

/// Per-conversation cursor and deduplication state stored under `${STATE_DIR}/state/`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationState {
    pub conversation_id: String,
    pub ov_session_id: String,
    #[serde(default)]
    pub last_recall_hash: Option<String>,
    #[serde(default)]
    pub last_recall_at_ms: Option<u64>,
    #[serde(default)]
    pub last_byte_offset: u64,
    #[serde(default)]
    pub transcript_full: Option<bool>,
    #[serde(default)]
    pub uncommitted_turns: usize,
    #[serde(default)]
    pub total_captured_turns: usize,
    #[serde(default)]
    pub peer_id: Option<String>,
    /// Serialized bytes sent since the last commit; the token estimate when the server
    /// reports no `pending_tokens`.
    #[serde(default)]
    pub uncommitted_bytes: usize,
    /// Transcript and workspace the conversation was captured from, so a later sweep can
    /// catch up its tail and commit it with the identity it started with.
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    /// Step index of the newest compaction `CHECKPOINT` already answered with a re-injection.
    #[serde(default)]
    pub last_checkpoint_step: Option<u64>,
    /// Turn whose recall injected `recalled_uris`, for the per-answer source summary.
    #[serde(default)]
    pub recall_turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recalled_uris: Vec<String>,
    /// Turn whose source summary was already shown.
    #[serde(default)]
    pub usage_reported_turn: Option<u64>,
}

impl ConversationState {
    /// Creates an initial state record for `conversation_id`.
    #[must_use]
    pub fn new(conversation_id: &str) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            ov_session_id: format!("agy-{conversation_id}"),
            ..Self::default()
        }
    }
}

/// Outcome of the latest recall attempt, written to `<plugin data>/last-recall.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastRecallSnapshot {
    pub timestamp_ms: u64,
    pub conversation_id: String,
    pub ov_session_id: String,
    pub peer_id: String,
    /// `injected`, `no_relevant` or `failed`.
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub query_chars: usize,
    pub injected_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Outcome of the latest capture pass, written to `<plugin data>/last-capture.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastCaptureSnapshot {
    pub timestamp_ms: u64,
    pub conversation_id: String,
    pub ov_session_id: String,
    #[serde(default)]
    pub finalize: bool,
    pub turns_captured: usize,
    /// Waiting in the offline queue after a retryable failure.
    pub turns_queued: usize,
    /// Rejected for good (401/403/404/422); these turns are not retried.
    pub turns_failed: usize,
    #[serde(default)]
    pub batches: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_tokens: Option<u64>,
    pub commit_threshold: u64,
    /// `committed`, `queued`, `failed`, or `pending` while below the threshold.
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Offline queue of one identity scope inside a data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalScopeReport {
    pub scope: String,
    pub current_identity: bool,
    pub entries: usize,
    pub bytes: u64,
    pub oldest_created_at_ms: Option<u64>,
}

/// A plugin data directory found by `doctor`. agy keeps one per product
/// (`~/.gemini/<product>/plugin_data/<plugin>`); runs outside agy use `~/.openviking/agy-plugin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataDirReport {
    pub path: String,
    pub active: bool,
    pub conversations: usize,
    pub last_activity_ms: Option<u64>,
    pub wal_scopes: Vec<WalScopeReport>,
    pub legacy_wal_entries: usize,
    pub last_recall: Option<LastRecallSnapshot>,
    #[serde(default)]
    pub last_capture: Option<LastCaptureSnapshot>,
}

/// Severity of one `doctor` finding; any `fail` makes the human report exit 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorLevel {
    Ok,
    Info,
    Warn,
    Fail,
}

/// One `doctor` finding, with what to do about it when it is a problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorFinding {
    pub level: DoctorLevel,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// A titled group of findings (install, configuration, server, activity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorSection {
    pub title: String,
    pub findings: Vec<DoctorFinding>,
}

/// Diagnostic report emitted by `ov-agy doctor [--json]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub plugin_version: String,
    pub enabled: bool,
    pub enabled_reason: String,
    pub credential_source: String,
    pub base_url: String,
    pub mcp_url: String,
    pub api_key_masked: String,
    pub has_api_key: bool,
    pub root_key_warning: Option<String>,
    pub account: Option<String>,
    pub user: Option<String>,
    pub peer_id: Option<String>,
    pub peer_source: Option<String>,
    pub git_is_worktree: bool,
    pub cli_config_path: Option<String>,
    pub ov_config_path: Option<String>,
    pub workspace_config_paths: Vec<String>,
    pub state_dir: String,
    pub pending_wal_entries: usize,
    pub pending_wal_bytes: u64,
    pub data_dirs: Vec<DataDirReport>,
    pub recall_timeout_ms: u64,
    pub recall_query_expansion: Option<String>,
    pub commit_token_threshold: u64,
    pub capture_tool_calls: bool,
    pub idle_sweep: bool,
    pub health_ok: Option<bool>,
    pub health_latency_ms: Option<u128>,
    pub server_version: Option<String>,
    pub server_auth_mode: Option<String>,
    pub mcp_handshake_ok: Option<bool>,
    pub mcp_status_code: Option<u16>,
    pub mcp_session_id_present: Option<bool>,
    pub mcp_tool_count: Option<usize>,
    /// The identity the server resolved for the key (`/health`): account, user and role.
    pub server_account: Option<String>,
    pub server_user: Option<String>,
    pub server_role: Option<String>,
    /// `/ready`: `None` when the server has no readiness endpoint or was not probed.
    pub server_ready: Option<bool>,
    /// Version in the installed plugin manifest, when an installation was found.
    pub installed_plugin_version: Option<String>,
    pub installed_plugin_path: Option<String>,
    /// No finding is a failure.
    pub ok: bool,
    pub sections: Vec<DoctorSection>,
    pub warnings: Vec<String>,
}
