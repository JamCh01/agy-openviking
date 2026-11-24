//! `PreInvocation` hook performing turn-deduplicated `OpenViking` context recall.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use sha2::Sha256;

use crate::client::OvClient;
use crate::config::{self, ResolvedConfig};
use crate::hooks::transcript;
use crate::wire::{InjectOutput, LastRecallSnapshot, PreInvocationInput};

const PRE_INVOCATION_BUDGET: Duration = Duration::from_millis(4200);
const STATE_LOCK_BUDGET: Duration = Duration::from_millis(150);

/// Executes `PreInvocation` with turn-level SHA-256 deduplication to preserve prompt cache hits.
pub async fn run(raw_stdin: &str) -> InjectOutput {
    match tokio::time::timeout(PRE_INVOCATION_BUDGET, run_inner(raw_stdin)).await {
        Ok(output) => output,
        Err(_) => InjectOutput::empty(),
    }
}

async fn run_inner(raw_stdin: &str) -> InjectOutput {
    let parsed: PreInvocationInput = match serde_json::from_str(raw_stdin) {
        Ok(val) => val,
        Err(_) => return InjectOutput::empty(),
    };

    let ws_path = parsed.common.effective_workspace().map(Path::new);
    let cfg = ResolvedConfig::load(ws_path);
    if !cfg.enabled || !cfg.auto_recall {
        cfg.log_debug(
            "hook.pre_invocation",
            "disabled",
            json!({ "enabled": cfg.enabled, "autoRecall": cfg.auto_recall }),
        );
        return InjectOutput::empty();
    }

    let conversation_id = parsed
        .common
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");

    if parsed.invocation_num.unwrap_or(0) != 0 {
        return InjectOutput::empty();
    }
    let lock_path = cfg.lock_file_path(&format!("recall-{conversation_id}"));
    let Ok(Some(_guard)) = config::try_acquire_exclusive_lock(&lock_path) else {
        return InjectOutput::empty();
    };
    let recorded = parsed.common.transcript_path.as_deref().and_then(|path| {
        transcript::extract_latest_user_turn_tail(
            &transcript::preferred_transcript(Path::new(path)),
            parsed.initial_num_steps,
        )
    });
    let host_query = parsed
        .common
        .last_user_input
        .as_deref()
        .map(transcript::strip_injected_blocks)
        .filter(|s| !s.is_empty());
    let (query, turn_id) = match (host_query, recorded) {
        (Some(query), Some(ref step))
            if step.truncated || query.split_whitespace().eq(step.text.split_whitespace()) =>
        {
            if step.answered {
                return InjectOutput::empty();
            }
            (
                query,
                step.step_index
                    .map_or(step.byte_offset, |i| i.saturating_add(1)),
            )
        }
        (Some(query), _) => (query, parsed.initial_num_steps.unwrap_or(0)),
        (None, Some(step)) if !step.answered => (
            step.text,
            step.step_index
                .map_or(step.byte_offset, |i| i.saturating_add(1)),
        ),
        _ => return InjectOutput::empty(),
    };
    let turn_hash = compute_turn_dedup_hash(conversation_id, turn_id, &query);
    let existing_state = config::load_conversation_state(&cfg, conversation_id);
    if existing_state.last_recall_hash.as_deref() == Some(turn_hash.as_str()) {
        cfg.log_debug(
            "hook.pre_invocation",
            "dedup_skip",
            json!({
                "conversation_id": conversation_id,
                "invocation_num": parsed.invocation_num,
                "turn": turn_id,
            }),
        );
        return InjectOutput::empty();
    }

    let compiled_filters = config::compile_input_filters(&cfg.recall_query_filters);
    let filtered = config::apply_input_filters(&query, &compiled_filters, "user");
    if filtered.dropped || filtered.text.chars().count() < cfg.min_query_length {
        let _ = config::update_conversation_state_with_budget(
            &cfg,
            conversation_id,
            STATE_LOCK_BUDGET,
            |state| {
                state.last_recall_hash = Some(turn_hash.clone());
                state.last_recall_at_ms = Some(config::now_epoch_ms());
                if !cfg.peer_id.is_empty() {
                    state.peer_id = Some(cfg.peer_id.clone());
                }
            },
        );
        cfg.log_debug(
            "hook.pre_invocation",
            "query_filtered_or_short",
            json!({ "dropped": filtered.dropped, "len": filtered.text.len() }),
        );
        return InjectOutput::empty();
    }

    let Ok(client) = OvClient::try_new(cfg.clone()) else {
        return InjectOutput::empty();
    };

    let ov_session_id = format!("agy-{conversation_id}");
    let request_timeout = Duration::from_millis(cfg.timeout_ms.min(3800));

    let recalled = match client
        .search_context(&filtered.text, &ov_session_id, request_timeout)
        .await
    {
        Ok(res) => res,
        Err(err) => {
            cfg.log_debug(
                "hook.pre_invocation",
                "search_failed",
                json!({ "error": err.to_string() }),
            );
            return InjectOutput::empty();
        }
    };

    let now_ms = config::now_epoch_ms();
    let _ = config::update_conversation_state_with_budget(
        &cfg,
        conversation_id,
        STATE_LOCK_BUDGET,
        |state| {
            state.last_recall_hash = Some(turn_hash);
            state.last_recall_at_ms = Some(now_ms);
            if !cfg.peer_id.is_empty() {
                state.peer_id = Some(cfg.peer_id.clone());
            }
        },
    );

    let formatted_block = recalled
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(wrap_openviking_context);

    let snapshot = LastRecallSnapshot {
        timestamp_ms: now_ms,
        conversation_id: conversation_id.to_string(),
        ov_session_id,
        peer_id: cfg.peer_id.clone(),
        query_preview: filtered.text.chars().take(160).collect(),
        injected_chars: formatted_block.as_ref().map_or(0, String::len),
        injected: formatted_block.is_some(),
    };
    let _ = config::write_json_atomic(&cfg.last_recall_path(), &snapshot);

    match formatted_block {
        Some(block) => {
            cfg.log_debug(
                "hook.pre_invocation",
                "injected_context",
                json!({ "chars": block.len(), "invocation_num": parsed.invocation_num }),
            );
            InjectOutput::new_system(block)
        }
        None => InjectOutput::empty(),
    }
}

/// Computes the fingerprint of the conversation, user step (or byte offset) and text.
#[must_use]
pub fn compute_turn_dedup_hash(conversation_id: &str, turn_id: u64, text: &str) -> String {
    use sha2::Digest;

    let mut hasher = Sha256::new();
    hasher.update(conversation_id.as_bytes());
    hasher.update(b":");
    hasher.update(turn_id.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

pub(super) fn wrap_openviking_context(raw_body: &str) -> String {
    let safe_body = raw_body
        .replace("<openviking-context", "[openviking-context")
        .replace("</openviking-context>", "[/openviking-context]");
    format!(
        "<openviking-context>\nRelevant context from OpenViking (past history, rules, and indexed resources; live instructions take precedence):\n{safe_body}\n</openviking-context>"
    )
}
