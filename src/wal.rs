//! Bounded offline Write-Ahead Log (`wal/<scope>/pending.jsonl`) for failed capture requests.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::OvClient;
use crate::config::{self, PrivateOpenMode, ResolvedConfig};
use crate::wire::{OvAddMessageRequest, OvMessagePart};

/// Maximum size (`10 MiB`) of `${STATE_DIR}/wal/<scope>/pending.jsonl`.
pub const MAX_WAL_BYTES: u64 = 10 * 1024 * 1024;

/// Queued session operation stored in the offline WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalOp {
    AddMessage,
    Commit,
}

/// Single JSONL record in `wal/<scope>/pending.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry {
    pub op: WalOp,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<OvMessagePart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub created_at_ms: u64,
}

impl WalEntry {
    /// Constructs an `add_message` WAL entry.
    #[must_use]
    pub fn add_message(session_id: &str, req: &OvAddMessageRequest) -> Self {
        Self {
            op: WalOp::AddMessage,
            session_id: session_id.to_string(),
            role: Some(req.role.clone()),
            parts: req.parts.clone(),
            peer_id: req.peer_id.clone(),
            created_at_ms: config::now_epoch_ms(),
        }
    }

    /// Constructs a `commit` WAL entry.
    #[must_use]
    pub fn commit(session_id: &str) -> Self {
        Self {
            op: WalOp::Commit,
            session_id: session_id.to_string(),
            role: None,
            parts: Vec::new(),
            peer_id: None,
            created_at_ms: config::now_epoch_ms(),
        }
    }
}

/// Current entry count and byte size of `wal/<scope>/pending.jsonl`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalStats {
    pub entry_count: usize,
    pub byte_size: u64,
}

/// Summary of a WAL replay pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayReport {
    pub replayed: usize,
    pub remaining: usize,
    pub dropped: usize,
}

/// File-locked offline queue manager for `wal/<scope>/pending.jsonl`.
pub struct WalStore {
    wal_path: PathBuf,
    lock_path: PathBuf,
    replay_lock_path: PathBuf,
}

impl WalStore {
    /// Creates a `WalStore` rooted in `cfg.state_dir`.
    #[must_use]
    pub fn new(cfg: &ResolvedConfig) -> Self {
        // Keys can select a different tenant even when no explicit account/user headers are set.
        // Length prefixes keep tuple boundaries unambiguous; credentials never enter the WAL.
        let mut digest = Sha256::new();
        for part in [
            &cfg.base_url,
            &cfg.account,
            &cfg.user,
            &cfg.api_key,
            &cfg.peer_id,
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        let scope = hex::encode(digest.finalize());
        Self {
            wal_path: cfg.state_dir.join("wal").join(&scope).join("pending.jsonl"),
            lock_path: cfg.lock_file_path(&format!("wal-{scope}")),
            replay_lock_path: cfg.lock_file_path(&format!("wal-replay-{scope}")),
        }
    }

    /// Returns the number of queued entries and file size in bytes.
    #[must_use]
    pub fn stats(&self) -> WalStats {
        let Ok(meta) = fs::metadata(&self.wal_path) else {
            return WalStats::default();
        };
        let byte_size = meta.len();
        if byte_size == 0 {
            return WalStats::default();
        }
        let Ok(file) = File::open(&self.wal_path) else {
            return WalStats {
                entry_count: 0,
                byte_size,
            };
        };
        let lines = BufReader::new(file).lines().map_while(Result::ok);
        let mut entry_count = 0usize;
        for line in lines {
            if !line.trim().is_empty() {
                entry_count += 1;
            }
        }
        WalStats {
            entry_count,
            byte_size,
        }
    }

    /// Appends `entries` to `wal/<scope>/pending.jsonl` under an exclusive lock, enforcing `MAX_WAL_BYTES`.
    pub fn append(&self, entries: &[WalEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = config::acquire_exclusive_lock(&self.lock_path, Duration::from_secs(3))?;
        let mut lines = self.read_raw_lines_unlocked();
        for entry in entries {
            lines.push(serde_json::to_string(entry)?);
        }
        let bounded = enforce_byte_cap(lines, MAX_WAL_BYTES as usize);
        self.write_lines_atomic_unlocked(&bounded)
    }

    /// Replays queued WAL entries in FIFO order against `client` without holding `wal.lock`
    /// across network `.await` points so concurrent `append` calls never block on slow networks.
    pub async fn replay(&self, client: &OvClient) -> Result<ReplayReport> {
        if !self.wal_path.is_file() {
            return Ok(ReplayReport::default());
        }

        // Ensure at most one background worker replays the shared WAL at a time without blocking.
        let Some(_replay_guard) = config::try_acquire_exclusive_lock(&self.replay_lock_path)?
        else {
            return Ok(ReplayReport {
                remaining: self.stats().entry_count.max(1),
                ..ReplayReport::default()
            });
        };

        let snapshot_lines = {
            let _data_guard =
                config::acquire_exclusive_lock(&self.lock_path, Duration::from_secs(3))?;
            let lines = self.read_raw_lines_unlocked();
            if lines.is_empty() {
                let _ = fs::remove_file(&self.wal_path);
                return Ok(ReplayReport::default());
            }
            lines
        };
        let snapshot_len = snapshot_lines.len();

        let mut parsed_entries = Vec::with_capacity(snapshot_len);
        let mut dropped = 0usize;
        for line in &snapshot_lines {
            match serde_json::from_str::<WalEntry>(line) {
                Ok(entry) => parsed_entries.push(entry),
                Err(_) => dropped += 1,
            }
        }

        let mut replayed = 0usize;
        let mut first_unsent_idx: Option<usize> = None;

        for (idx, entry) in parsed_entries.iter().enumerate() {
            let send_res = match entry.op {
                WalOp::AddMessage => {
                    let req = OvAddMessageRequest {
                        role: entry.role.clone().unwrap_or_else(|| "user".to_string()),
                        parts: entry.parts.clone(),
                        peer_id: entry.peer_id.clone(),
                    };
                    client.add_session_message(&entry.session_id, &req).await
                }
                WalOp::Commit => client.commit_session(&entry.session_id).await,
            };

            match send_res {
                Ok(()) => {
                    replayed += 1;
                }
                Err(err) if err.should_queue_to_wal() => {
                    first_unsent_idx = Some(idx);
                    break;
                }
                Err(_) => {
                    dropped += 1;
                }
            }
        }

        // Re-acquire the short-lived data lock to merge unsent snapshot entries with any lines
        // concurrently appended by other conversations while HTTP replay was in flight.
        let _data_guard = config::acquire_exclusive_lock(&self.lock_path, Duration::from_secs(3))?;
        let current_lines = self.read_raw_lines_unlocked();
        let newly_appended: &[String] = if current_lines.len() > snapshot_len
            && current_lines[..snapshot_len] == snapshot_lines[..]
        {
            &current_lines[snapshot_len..]
        } else if current_lines == snapshot_lines {
            &[]
        } else {
            // WAL was rotated/capped concurrently while exceeds `MAX_WAL_BYTES`; preserve current tail.
            &current_lines
        };

        let mut merged = Vec::new();
        if let Some(unsent_start) = first_unsent_idx {
            for item in &parsed_entries[unsent_start..] {
                merged.push(serde_json::to_string(item)?);
            }
        }
        merged.extend(newly_appended.iter().cloned());

        if merged.is_empty() {
            let _ = fs::remove_file(&self.wal_path);
            Ok(ReplayReport {
                replayed,
                remaining: 0,
                dropped,
            })
        } else {
            let bounded = enforce_byte_cap(merged, MAX_WAL_BYTES as usize);
            let remaining = bounded.len();
            self.write_lines_atomic_unlocked(&bounded)?;
            Ok(ReplayReport {
                replayed,
                remaining,
                dropped,
            })
        }
    }

    fn read_raw_lines_unlocked(&self) -> Vec<String> {
        let Ok(file) = File::open(&self.wal_path) else {
            return Vec::new();
        };
        let reader_lines = BufReader::new(file).lines().map_while(Result::ok);
        let mut out = Vec::new();
        for line in reader_lines {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        out
    }

    fn write_lines_atomic_unlocked(&self, lines: &[String]) -> Result<()> {
        if lines.is_empty() {
            let _ = fs::remove_file(&self.wal_path);
            return Ok(());
        }
        if let Some(parent) = self.wal_path.parent() {
            config::ensure_private_dir(parent)
                .with_context(|| format!("failed creating WAL directory {}", parent.display()))?;
        }
        let tmp_path = self
            .wal_path
            .with_extension(format!("jsonl.tmp.{}", std::process::id()));
        let write_res = (|| -> Result<()> {
            let mut temp_file =
                config::open_private_file(&tmp_path, PrivateOpenMode::TruncateWrite)
                    .with_context(|| format!("failed opening temp WAL {}", tmp_path.display()))?;
            for line in lines {
                writeln!(temp_file, "{line}")?;
            }
            temp_file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }
        if let Err(err) = fs::rename(&tmp_path, &self.wal_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(err).with_context(|| {
                format!(
                    "failed atomically updating WAL {} -> {}",
                    tmp_path.display(),
                    self.wal_path.display()
                )
            });
        }
        Ok(())
    }
}

fn enforce_byte_cap(lines: Vec<String>, max_bytes: usize) -> Vec<String> {
    let total: usize = lines.iter().map(|l| l.len() + 1).sum();
    if total <= max_bytes {
        return lines;
    }

    let mut kept_rev = Vec::new();
    let mut used = 0usize;
    let reversed = lines.into_iter().rev();
    for line in reversed {
        let cost = line.len() + 1;
        if used + cost > max_bytes {
            break;
        }
        used += cost;
        kept_rev.push(line);
    }
    kept_rev.reverse();
    kept_rev
}
