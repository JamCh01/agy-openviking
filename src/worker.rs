//! Detached background worker for turn capture, anti-poisoning sanitization, and WAL replay.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cli::WorkerArgs;
use crate::client::OvClient;
use crate::config::{self, ResolvedConfig};
use crate::hooks::transcript;
use crate::wal::{WalEntry, WalOp, WalStore};
use crate::wire::{ConversationState, LastCaptureSnapshot, OvAddMessageRequest};

/// A worker runs detached from the hook, so it can wait for an earlier worker of the same
/// conversation instead of skipping the final turn.
const CONVERSATION_LOCK_WAIT: Duration = Duration::from_secs(60);

/// Server limit for `POST /api/v1/sessions/{id}/messages/batch`.
const BATCH_LIMIT: usize = 100;

/// Executes a single locked capture + WAL replay pass for `args.conversation_id`.
pub async fn run(args: WorkerArgs) -> Result<()> {
    let mut cfg = ResolvedConfig::load(args.cwd.as_deref());
    if !cfg.enabled || !cfg.auto_capture {
        cfg.log_debug(
            "worker",
            "disabled",
            json!({ "enabled": cfg.enabled, "autoCapture": cfg.auto_capture }),
        );
        return Ok(());
    }

    let lock_path = cfg.lock_file_path(&args.conversation_id);
    // The sweep never waits: a held lock means a Stop worker owns the conversation right now,
    // and the next sweep comes back to it. A compaction commit waits for that worker instead;
    // skipping would leave the compacted turns uncommitted until the conversation goes idle.
    let lock = if args.from_sweep {
        config::try_acquire_exclusive_lock(&lock_path)
            .ok()
            .flatten()
    } else {
        config::acquire_exclusive_lock(&lock_path, CONVERSATION_LOCK_WAIT).ok()
    };
    let Some(_conv_lock) = lock else {
        cfg.log_debug(
            "worker",
            "lock_busy",
            json!({ "conversation_id": args.conversation_id, "finalize": args.finalize }),
        );
        return Ok(());
    };

    let initial_state = config::load_conversation_state(&cfg, &args.conversation_id);
    // A conversation keeps the peer it started with, so its messages, request headers and
    // offline queue stay together even if the workspace configuration changes later.
    if let Some(pinned) = initial_state.peer_id.as_ref().filter(|p| !p.is_empty()) {
        cfg.peer_id.clone_from(pinned);
    }

    let client = OvClient::try_new(cfg.clone())?;
    let wal = WalStore::new(&cfg);

    // 1. Replay pending offline WAL entries before appending new deltas.
    let mut offline = match wal.replay(&client).await {
        Ok(report) => {
            if report.replayed > 0 || report.remaining > 0 {
                cfg.log_debug(
                    "worker",
                    "wal_replay",
                    json!({
                        "replayed": report.replayed,
                        "remaining": report.remaining,
                        "dropped": report.dropped,
                    }),
                );
            }
            report.remaining > 0
        }
        Err(err) => {
            cfg.log_debug(
                "worker",
                "wal_replay_error",
                json!({ "error": err.to_string() }),
            );
            true
        }
    };

    // 2. Read the finished turns past the cursor.
    let transcript_path = args
        .transcript_path
        .clone()
        .or_else(|| initial_state.transcript_path.as_ref().map(PathBuf::from));
    let mut cursor = Cursor {
        offset: initial_state.last_byte_offset,
        full: initial_state.transcript_full,
    };
    let mut turns = Vec::new();
    if let Some(path) = transcript_path.as_deref() {
        if let Some((delta, full)) = read_delta(path, &initial_state, args.finalize)? {
            cursor = Cursor {
                offset: delta.new_byte_offset,
                full: Some(full),
            };
            turns = delta.turns;
        }
    }

    // 3. Anti-poisoning sanitization, `captureFilters`, and tool parts.
    let capture_filters = config::compile_input_filters(&cfg.capture_filters);
    let peer_opt = (!cfg.peer_id.trim().is_empty()).then(|| cfg.peer_id.clone());
    let payloads: Vec<OvAddMessageRequest> = turns
        .iter()
        .filter_map(|turn| {
            transcript::shape_turn_parts(
                turn,
                &capture_filters,
                cfg.capture_tool_max_chars,
                cfg.capture_tool_calls,
            )
            .map(|parts| OvAddMessageRequest {
                role: turn.role.clone(),
                parts,
                peer_id: peer_opt.clone(),
            })
        })
        .collect();

    let ov_session_id = format!("agy-{}", args.conversation_id);
    let mut uncommitted = initial_state.uncommitted_turns;
    let mut uncommitted_bytes = initial_state.uncommitted_bytes;

    if payloads.is_empty() && !(args.finalize && uncommitted > 0) {
        if cursor.offset != initial_state.last_byte_offset
            || cursor.full != initial_state.transcript_full
            || !args.finalize
        {
            save_state(
                &cfg,
                &args,
                transcript_path.as_deref(),
                peer_opt.as_ref(),
                |state| {
                    state.last_byte_offset = cursor.offset;
                    state.transcript_full = cursor.full;
                },
            )?;
        }
        return Ok(());
    }

    // 4. Send, queueing whatever cannot be delivered now.
    let mut wal_batch: Vec<WalEntry> = Vec::new();
    let sent = if offline {
        SendReport::default()
    } else {
        send_messages(&client, &cfg, &ov_session_id, &payloads, &mut offline).await
    };
    if offline {
        let unsent = &payloads[sent.processed..];
        wal_batch.extend(
            unsent
                .iter()
                .map(|payload| WalEntry::add_message(&ov_session_id, payload)),
        );
        uncommitted_bytes += unsent.iter().map(payload_bytes).sum::<usize>();
    }
    uncommitted += sent.delivered + wal_batch.len();
    uncommitted_bytes += sent.delivered_bytes;

    // 5. Commit once enough is pending, or unconditionally when the sweep finalizes.
    let mut commit = "pending";
    let mut pending_tokens = None;
    if uncommitted > 0 {
        let due = if args.finalize || cfg.commit_every_turns.is_some_and(|n| uncommitted >= n) {
            true
        } else {
            let pending =
                pending_tokens_estimate(&client, &cfg, &ov_session_id, offline, uncommitted_bytes)
                    .await;
            pending_tokens = Some(pending);
            pending >= cfg.commit_token_threshold
        };
        if due {
            commit =
                commit_or_queue(&client, &cfg, &ov_session_id, &mut offline, &mut wal_batch).await;
            if commit != "failed" {
                uncommitted = 0;
                uncommitted_bytes = 0;
            }
        }
    }

    // 6. Sync unsent operations to the WAL BEFORE advancing `last_byte_offset`.
    if !wal_batch.is_empty() {
        wal.append(&wal_batch)?;
    }
    let queued_msgs = wal_batch
        .iter()
        .filter(|entry| entry.op == WalOp::AddMessage)
        .count();

    save_state(
        &cfg,
        &args,
        transcript_path.as_deref(),
        peer_opt.as_ref(),
        |state| {
            state.last_byte_offset = cursor.offset;
            state.transcript_full = cursor.full;
            state.uncommitted_turns = uncommitted;
            state.uncommitted_bytes = uncommitted_bytes;
            state.total_captured_turns += sent.delivered + queued_msgs;
        },
    )?;

    let snapshot = LastCaptureSnapshot {
        timestamp_ms: config::now_epoch_ms(),
        conversation_id: args.conversation_id.clone(),
        ov_session_id: ov_session_id.clone(),
        finalize: args.finalize,
        turns_captured: sent.delivered,
        turns_queued: queued_msgs,
        turns_failed: sent.rejected,
        batches: sent.batches,
        pending_tokens,
        commit_threshold: cfg.commit_token_threshold,
        commit: commit.to_string(),
        error: sent.last_error.clone(),
    };
    if config::ensure_private_dir(&cfg.state_dir).is_ok() {
        let _ = config::write_json_atomic(&cfg.last_capture_path(), &snapshot);
    }

    cfg.log_debug(
        "worker",
        "completed",
        json!({
            "conversation_id": args.conversation_id,
            "sent": sent.delivered,
            "batches": sent.batches,
            "queued_wal": wal_batch.len(),
            "new_offset": cursor.offset,
            "uncommitted_turns": uncommitted,
            "finalize": args.finalize,
        }),
    );

    Ok(())
}

struct Cursor {
    offset: u64,
    full: Option<bool>,
}

/// Reads the turns past the saved cursor, translating it when agy switched from
/// `transcript.jsonl` to `transcript_full.jsonl`. `None` means there is nothing to read.
fn read_delta(
    transcript_path: &Path,
    state: &ConversationState,
    finalize: bool,
) -> Result<Option<(transcript::TranscriptDelta, bool)>> {
    let selected = transcript::preferred_transcript(transcript_path);
    let full = selected
        .file_name()
        .is_some_and(|name| name == "transcript_full.jsonl");
    let previous_full = state.transcript_full.unwrap_or_else(|| {
        transcript_path
            .file_name()
            .is_some_and(|name| name == "transcript_full.jsonl")
    });
    if !selected.is_file() || (previous_full && !full) {
        return Ok(None);
    }
    let mut offset = state.last_byte_offset;
    if full && !previous_full && offset > 0 {
        let short = selected.with_file_name("transcript.jsonl");
        offset = transcript::translate_cursor(&short, &selected, offset)?;
    }
    Ok(Some((
        transcript::read_new_turns_from_offset(&selected, offset, finalize)?,
        full,
    )))
}

#[derive(Default)]
struct SendReport {
    /// Leading payloads handled, whether accepted or rejected for good; when `offline` is
    /// set, the payloads from here on must be queued.
    processed: usize,
    delivered: usize,
    delivered_bytes: usize,
    batches: usize,
    /// Messages the server rejected for good; they are not queued.
    rejected: usize,
    last_error: Option<String>,
}

/// Sends `payloads` in batches of 100, falling back to one request per message when the
/// server has no batch endpoint or rejects a batch outright. Sets `offline` on a retryable
/// failure; payloads from `delivered` on must then be queued.
async fn send_messages(
    client: &OvClient,
    cfg: &ResolvedConfig,
    session: &str,
    payloads: &[OvAddMessageRequest],
    offline: &mut bool,
) -> SendReport {
    let mut report = SendReport::default();
    let mut serial = false;
    for chunk in payloads.chunks(BATCH_LIMIT) {
        if !serial {
            match client.add_session_messages_batch(session, chunk).await {
                Ok(true) => {
                    report.processed += chunk.len();
                    report.delivered += chunk.len();
                    report.delivered_bytes += chunk.iter().map(payload_bytes).sum::<usize>();
                    report.batches += 1;
                    continue;
                }
                Ok(false) => serial = true,
                Err(err) if err.should_queue_to_wal() => {
                    *offline = true;
                    cfg.log_debug(
                        "worker",
                        "queueing_to_wal",
                        json!({ "error": err.to_string() }),
                    );
                    report.last_error = Some(err.to_string());
                    return report;
                }
                // One bad message must not cost the whole batch: retry this chunk one by one.
                Err(err) => cfg.log_debug(
                    "worker",
                    "batch_rejected",
                    json!({ "error": err.to_string(), "size": chunk.len() }),
                ),
            }
        }
        for payload in chunk {
            match client.add_session_message(session, payload).await {
                Ok(()) => {
                    report.delivered += 1;
                    report.delivered_bytes += payload_bytes(payload);
                }
                Err(err) if err.should_queue_to_wal() => {
                    *offline = true;
                    cfg.log_debug(
                        "worker",
                        "queueing_to_wal",
                        json!({ "error": err.to_string() }),
                    );
                    report.last_error = Some(err.to_string());
                    return report;
                }
                Err(err) => {
                    cfg.log_debug(
                        "worker",
                        "message_non_retryable_error",
                        json!({ "error": err.to_string() }),
                    );
                    report.rejected += 1;
                    report.last_error = Some(err.to_string());
                }
            }
            report.processed += 1;
        }
    }
    report
}

/// Codex and Claude Code commit when the server reports `commitTokenThreshold` pending
/// tokens, so extraction sees whole stretches of work instead of one turn at a time.
async fn pending_tokens_estimate(
    client: &OvClient,
    cfg: &ResolvedConfig,
    session: &str,
    offline: bool,
    uncommitted_bytes: usize,
) -> u64 {
    // About four bytes per token for both English and UTF-8 CJK text.
    let estimate = (uncommitted_bytes / 4) as u64;
    let pending = if offline {
        estimate
    } else {
        match client.session_meta(session).await {
            Ok(meta) => meta.pending_tokens.unwrap_or(estimate),
            Err(err) => {
                cfg.log_debug(
                    "worker",
                    "session_meta_error",
                    json!({ "error": err.to_string() }),
                );
                estimate
            }
        }
    };
    cfg.log_debug(
        "worker",
        "pending_tokens",
        json!({ "pending": pending, "threshold": cfg.commit_token_threshold }),
    );
    pending
}

fn payload_bytes(payload: &OvAddMessageRequest) -> usize {
    payload
        .parts
        .iter()
        .map(crate::wire::OvMessagePart::approx_bytes)
        .sum()
}

fn save_state<F>(
    cfg: &ResolvedConfig,
    args: &WorkerArgs,
    transcript_path: Option<&Path>,
    peer: Option<&String>,
    mutator: F,
) -> Result<()>
where
    F: FnOnce(&mut ConversationState),
{
    config::update_conversation_state(cfg, &args.conversation_id, |state| {
        mutator(state);
        if state.peer_id.is_none() {
            state.peer_id = peer.cloned();
        }
        if let Some(path) = transcript_path {
            state.transcript_path = Some(path.display().to_string());
        }
        if let Some(ref cwd) = args.cwd {
            state.workspace = Some(cwd.display().to_string());
        }
        state.updated_at_ms = Some(config::now_epoch_ms());
    })
    .map(drop)
}

/// `committed`, `queued` (to replay with the offline queue) or `failed` (rejected for good).
async fn commit_or_queue(
    client: &OvClient,
    cfg: &ResolvedConfig,
    session: &str,
    offline: &mut bool,
    batch: &mut Vec<WalEntry>,
) -> &'static str {
    if !*offline {
        match client.commit_session(session).await {
            Ok(()) => return "committed",
            Err(err) if err.should_queue_to_wal() => *offline = true,
            Err(err) => {
                cfg.log_debug(
                    "worker",
                    "commit_non_retryable_error",
                    json!({ "error": err.to_string() }),
                );
                return "failed";
            }
        }
    }
    batch.push(WalEntry::commit(session));
    "queued"
}
