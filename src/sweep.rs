//! Idle sweep started by `SessionStart`. agy has no `SessionEnd` hook, so a conversation
//! is committed once it has been idle for `idle_commit_ms`, the way the Codex plugin's
//! startup sweep covers sessions whose `SessionEnd` never ran.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::json;

use crate::cli::{SweepArgs, WorkerArgs};
use crate::client::OvClient;
use crate::config::{self, ResolvedConfig};
use crate::hooks::transcript;
use crate::wal::WalStore;
use crate::wire::ConversationState;
use crate::worker;

/// `SessionStart` starts at most one sweep per interval.
pub const SWEEP_INTERVAL: Duration = Duration::from_mins(10);

/// Committed conversations, and lock files nothing refers to, are removed after this long.
const RETIRE_AFTER_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Bounds the work of one sweep; the rest waits for the next one.
const MAX_FINALIZED_PER_SWEEP: usize = 20;

/// Lock names shared across conversations, which must never be deleted.
const SHARED_LOCK_PREFIXES: &[&str] = &["wal-", "sweep", "fetch-"];

/// Returns `true` when enough time passed since the previous sweep, and records this one.
#[must_use]
pub fn claim_sweep_slot(cfg: &ResolvedConfig) -> bool {
    let marker = cfg.state_dir.join("last-sweep");
    let recent = fs::metadata(&marker)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < SWEEP_INTERVAL);
    if recent {
        return false;
    }
    config::ensure_private_dir(&cfg.state_dir).is_ok()
        && config::open_private_file(&marker, config::PrivateOpenMode::TruncateWrite)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, config::now_epoch_ms().to_string().as_bytes())
            })
            .is_ok()
}

/// Commits idle conversations and retires stale state for this plugin data directory.
pub async fn run(args: SweepArgs) -> Result<()> {
    let cfg = ResolvedConfig::load(args.cwd.as_deref());
    if !cfg.enabled {
        return Ok(());
    }
    let Some(_guard) = config::try_acquire_exclusive_lock(&cfg.lock_file_path("sweep"))? else {
        return Ok(());
    };

    // The starting workspace's own queue would otherwise wait for its first idle Stop.
    if let Ok(client) = OvClient::try_new(cfg.clone()) {
        let _ = WalStore::new(&cfg).replay(&client).await;
    }

    let now = config::now_epoch_ms();
    let mut finalized = 0usize;
    let mut retired = Vec::new();
    for (path, state) in list_states(&cfg.state_dir) {
        if args.exclude.as_deref() == Some(state.conversation_id.as_str()) {
            continue;
        }
        let updated = state
            .updated_at_ms
            .or_else(|| modified_ms(&path))
            .unwrap_or(now);
        let idle_ms = now.saturating_sub(updated);
        let pending = state.uncommitted_turns > 0 || has_unread_tail(&state);
        if pending && idle_ms >= cfg.idle_commit_ms {
            if finalized >= MAX_FINALIZED_PER_SWEEP {
                continue;
            }
            finalized += 1;
            let outcome = worker::run(WorkerArgs {
                conversation_id: state.conversation_id.clone(),
                transcript_path: state.transcript_path.as_ref().map(PathBuf::from),
                cwd: state.workspace.as_ref().map(PathBuf::from),
                finalize: true,
                from_sweep: true,
            })
            .await;
            cfg.log_debug(
                "sweep",
                "finalized",
                json!({
                    "conversation_id": state.conversation_id,
                    "idle_ms": idle_ms,
                    "error": outcome.err().map(|e| e.to_string()),
                }),
            );
        } else if !pending && idle_ms >= RETIRE_AFTER_MS {
            let _ = fs::remove_file(&path);
            for name in [
                state.conversation_id.clone(),
                format!("recall-{}", state.conversation_id),
            ] {
                let _ = fs::remove_file(cfg.lock_file_path(&name));
            }
            retired.push(state.conversation_id);
        }
    }
    let orphan_locks = remove_stale_locks(&cfg.state_dir.join("locks"), now);

    cfg.log_debug(
        "sweep",
        "completed",
        json!({
            "finalized": finalized,
            "retired": retired.len(),
            "orphan_locks_removed": orphan_locks,
        }),
    );
    Ok(())
}

fn list_states(state_dir: &Path) -> Vec<(PathBuf, ConversationState)> {
    fs::read_dir(state_dir.join("state"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let state = config::read_json_optional::<ConversationState>(&path)?;
            (!state.conversation_id.is_empty()).then_some((path, state))
        })
        .collect()
}

/// Only states written by this version record their transcript; older cursors may refer to
/// the other serialization, so they are never caught up.
fn has_unread_tail(state: &ConversationState) -> bool {
    let Some(path) = state.transcript_path.as_deref().map(Path::new) else {
        return false;
    };
    let selected = transcript::preferred_transcript(path);
    let matches_cursor = state.transcript_full.is_none_or(|full| {
        full == selected
            .file_name()
            .is_some_and(|name| name == "transcript_full.jsonl")
    });
    matches_cursor && fs::metadata(&selected).is_ok_and(|meta| meta.len() > state.last_byte_offset)
}

fn modified_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// Removes per-conversation lock files untouched for `RETIRE_AFTER_MS`. Shared locks stay.
fn remove_stale_locks(locks_dir: &Path, now: u64) -> usize {
    let mut removed = 0;
    for entry in fs::read_dir(locks_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SHARED_LOCK_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let stale =
            modified_ms(&entry.path()).is_some_and(|ms| now.saturating_sub(ms) >= RETIRE_AFTER_MS);
        if stale && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}
