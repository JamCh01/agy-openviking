//! Detached background worker for turn capture, anti-poisoning sanitization, and WAL replay.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cli::WorkerArgs;
use crate::client::OvClient;
use crate::config::{self, ResolvedConfig};
use crate::hooks::transcript;
use crate::wal::{WalEntry, WalOp, WalStore};
use crate::wire::{OvAddMessageRequest, OvMessagePart};

/// Executes a single locked capture + WAL replay pass for `args.conversation_id`.
pub async fn run(args: WorkerArgs) -> Result<()> {
    let cfg = ResolvedConfig::load(args.cwd.as_deref());
    if !cfg.enabled || !cfg.auto_capture {
        cfg.log_debug(
            "worker",
            "disabled",
            json!({ "enabled": cfg.enabled, "autoCapture": cfg.auto_capture }),
        );
        return Ok(());
    }

    let lock_path = cfg.lock_file_path(&args.conversation_id);
    let _conv_lock = match config::acquire_exclusive_lock(&lock_path, Duration::from_secs(5)) {
        Ok(guard) => guard,
        Err(err) => {
            cfg.log_debug(
                "worker",
                "lock_busy",
                json!({
                    "conversation_id": args.conversation_id,
                    "error": err.to_string(),
                }),
            );
            return Ok(());
        }
    };

    let client = OvClient::try_new(cfg.clone())?;
    let wal = WalStore::new(&cfg);

    // 1. Replay any pending offline WAL entries before appending new deltas.
    let mut wal_still_blocked = false;
    match wal.replay(&client).await {
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
            if report.remaining > 0 {
                wal_still_blocked = true;
            }
        }
        Err(err) => {
            wal_still_blocked = true;
            cfg.log_debug(
                "worker",
                "wal_replay_error",
                json!({ "error": err.to_string() }),
            );
        }
    }

    let Some(ref transcript_path) = args.transcript_path else {
        return Ok(());
    };
    let initial_state = config::load_conversation_state(&cfg, &args.conversation_id);
    let selected = transcript::preferred_transcript(transcript_path);
    let full = selected
        .file_name()
        .is_some_and(|name| name == "transcript_full.jsonl");
    let previous_full = initial_state.transcript_full.unwrap_or_else(|| {
        transcript_path
            .file_name()
            .is_some_and(|name| name == "transcript_full.jsonl")
    });
    if !selected.is_file() || (previous_full && !full) {
        return Ok(());
    }
    let mut offset = initial_state.last_byte_offset;
    if full && !previous_full && offset > 0 {
        let short = selected.with_file_name("transcript.jsonl");
        offset = transcript::translate_cursor(&short, &selected, offset)?;
    }
    let delta = transcript::read_new_turns_from_offset(&selected, offset)?;

    // 3. Anti-poisoning sanitization + `captureFilters`.
    let capture_filters = config::compile_input_filters(&cfg.capture_filters);
    let peer_opt = if cfg.peer_id.trim().is_empty() {
        None
    } else {
        Some(cfg.peer_id.clone())
    };

    let mut payloads: Vec<OvAddMessageRequest> = Vec::new();
    for turn in delta.turns {
        if let Some(clean_text) = transcript::sanitize_turn_text(
            &turn.text,
            &capture_filters,
            &turn.role,
            cfg.capture_max_length,
        ) {
            payloads.push(OvAddMessageRequest {
                role: turn.role,
                parts: vec![OvMessagePart::new_text(clean_text)],
                peer_id: peer_opt.clone(),
            });
        }
    }

    if payloads.is_empty() && initial_state.uncommitted_turns == 0 {
        if delta.new_byte_offset == initial_state.last_byte_offset
            && Some(full) == initial_state.transcript_full
        {
            return Ok(());
        }
        config::update_conversation_state(&cfg, &args.conversation_id, |state| {
            state.last_byte_offset = delta.new_byte_offset;
            state.transcript_full = Some(full);
        })?;
        return Ok(());
    }

    let ov_session_id = format!("agy-{}", args.conversation_id);
    let mut uncommitted = initial_state.uncommitted_turns;
    let mut sent_count = 0usize;
    let mut wal_batch: Vec<WalEntry> = Vec::new();
    let mut send_failed_to_offline = wal_still_blocked;

    for payload in &payloads {
        if !send_failed_to_offline {
            match client.add_session_message(&ov_session_id, payload).await {
                Ok(()) => sent_count += 1,
                Err(err) if err.should_queue_to_wal() => {
                    send_failed_to_offline = true;
                    cfg.log_debug(
                        "worker",
                        "queueing_to_wal",
                        json!({ "error": err.to_string() }),
                    );
                }
                Err(err) => {
                    cfg.log_debug(
                        "worker",
                        "message_non_retryable_error",
                        json!({ "error": err.to_string() }),
                    );
                    continue;
                }
            }
        }
        if send_failed_to_offline {
            wal_batch.push(WalEntry::add_message(&ov_session_id, payload));
        }
        uncommitted += 1;
        if uncommitted >= cfg.commit_every_turns
            && commit_or_queue(
                &client,
                &cfg,
                &ov_session_id,
                &mut send_failed_to_offline,
                &mut wal_batch,
            )
            .await
        {
            uncommitted = 0;
        }
    }

    // An idle Stop can be the final event of a short conversation.
    if uncommitted > 0
        && commit_or_queue(
            &client,
            &cfg,
            &ov_session_id,
            &mut send_failed_to_offline,
            &mut wal_batch,
        )
        .await
    {
        uncommitted = 0;
    }

    // 4. Sync unsent turns to WAL BEFORE advancing `last_byte_offset`.
    if !wal_batch.is_empty() {
        wal.append(&wal_batch)?;
    }

    let queued_msgs = wal_batch
        .iter()
        .filter(|entry| entry.op == WalOp::AddMessage)
        .count();

    config::update_conversation_state(&cfg, &args.conversation_id, |state| {
        state.last_byte_offset = delta.new_byte_offset;
        state.transcript_full = Some(full);
        state.uncommitted_turns = uncommitted;
        state.total_captured_turns += sent_count + queued_msgs;
        if let Some(ref peer) = peer_opt {
            state.peer_id = Some(peer.clone());
        }
    })?;

    cfg.log_debug(
        "worker",
        "completed",
        json!({
            "conversation_id": args.conversation_id,
            "sent": sent_count,
            "queued_wal": wal_batch.len(),
            "new_offset": delta.new_byte_offset,
            "uncommitted_turns": uncommitted,
        }),
    );

    Ok(())
}

async fn commit_or_queue(
    client: &OvClient,
    cfg: &ResolvedConfig,
    session: &str,
    offline: &mut bool,
    batch: &mut Vec<WalEntry>,
) -> bool {
    if !*offline {
        match client.commit_session(session).await {
            Ok(()) => return true,
            Err(err) if err.should_queue_to_wal() => *offline = true,
            Err(err) => {
                cfg.log_debug(
                    "worker",
                    "commit_non_retryable_error",
                    json!({ "error": err.to_string() }),
                );
                return false;
            }
        }
    }
    batch.push(WalEntry::commit(session));
    true
}
