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
    if !transcript_path.is_file() {
        return Ok(());
    }

    // 2. Read transcript incremental delta starting from saved `last_byte_offset`.
    let initial_state = config::load_conversation_state(&cfg, &args.conversation_id);
    let delta =
        transcript::read_new_turns_from_offset(transcript_path, initial_state.last_byte_offset)?;

    if delta.new_byte_offset == initial_state.last_byte_offset && delta.turns.is_empty() {
        return Ok(());
    }

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

    if payloads.is_empty() {
        let _ = config::update_conversation_state(&cfg, &args.conversation_id, |state| {
            state.last_byte_offset = delta.new_byte_offset;
        });
        return Ok(());
    }

    let ov_session_id = format!("agy-{}", args.conversation_id);
    let mut uncommitted = initial_state.uncommitted_turns;
    let mut sent_count = 0usize;
    let mut wal_batch: Vec<WalEntry> = Vec::new();
    let mut send_failed_to_offline = wal_still_blocked;

    for payload in &payloads {
        if send_failed_to_offline {
            wal_batch.push(WalEntry::add_message(&ov_session_id, payload));
            uncommitted += 1;
            if uncommitted >= cfg.commit_every_turns {
                wal_batch.push(WalEntry::commit(&ov_session_id));
                uncommitted = 0;
            }
            continue;
        }

        match client.add_session_message(&ov_session_id, payload).await {
            Ok(()) => {
                sent_count += 1;
                uncommitted += 1;
                if uncommitted >= cfg.commit_every_turns {
                    match client.commit_session(&ov_session_id).await {
                        Ok(()) => {
                            uncommitted = 0;
                        }
                        Err(err) if err.should_queue_to_wal() => {
                            send_failed_to_offline = true;
                            wal_batch.push(WalEntry::commit(&ov_session_id));
                            uncommitted = 0;
                        }
                        Err(err) => {
                            cfg.log_debug(
                                "worker",
                                "commit_non_retryable_error",
                                json!({ "error": err.to_string() }),
                            );
                        }
                    }
                }
            }
            Err(err) if err.should_queue_to_wal() => {
                cfg.log_debug(
                    "worker",
                    "queueing_to_wal",
                    json!({ "error": err.to_string(), "session_id": ov_session_id }),
                );
                send_failed_to_offline = true;
                wal_batch.push(WalEntry::add_message(&ov_session_id, payload));
                uncommitted += 1;
                if uncommitted >= cfg.commit_every_turns {
                    wal_batch.push(WalEntry::commit(&ov_session_id));
                    uncommitted = 0;
                }
            }
            Err(err) => {
                cfg.log_debug(
                    "worker",
                    "message_non_retryable_error",
                    json!({ "error": err.to_string() }),
                );
            }
        }
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
