//! Layered configuration loader, input regex filters, file locks, and state persistence.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::git_peer::{self, PeerResolution};
use crate::wire::ConversationState;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:1933";
pub const DEFAULT_RECALL_LIMIT: usize = 6;
pub const DEFAULT_RECALL_DEDUP_TURNS: u32 = 5;
pub const DEFAULT_SCORE_THRESHOLD: f64 = 0.35;
pub const DEFAULT_MIN_QUERY_LENGTH: usize = 3;
pub const DEFAULT_CAPTURE_MAX_LENGTH: usize = 24_000;
pub const DEFAULT_COMMIT_EVERY_TURNS: usize = 6;
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// Credential resolution mode controlled by `OPENVIKING_CREDENTIAL_SOURCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    Auto,
    Cli,
    Env,
}

/// Fully resolved runtime configuration for hooks, worker, MCP proxy, and doctor.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub enabled: bool,
    pub enabled_reason: String,
    pub credential_source: String,
    pub base_url: String,
    pub mcp_url: String,
    pub api_key: String,
    pub root_key_warning: Option<String>,
    pub account: String,
    pub user: String,
    pub peer_id: String,
    pub peer_resolution: Option<PeerResolution>,
    pub ov_home_dir: PathBuf,
    pub state_dir: PathBuf,
    pub debug: bool,
    pub debug_log_path: PathBuf,
    pub timeout_ms: u64,
    pub auto_recall: bool,
    pub auto_capture: bool,
    pub recall_limit: usize,
    pub recall_dedup_turns: u32,
    pub score_threshold: f64,
    pub min_query_length: usize,
    pub recall_peer_scope: String,
    pub recall_exclude_uris: Vec<String>,
    pub recall_query_filters: Vec<String>,
    pub capture_filters: Vec<String>,
    pub capture_max_length: usize,
    pub commit_every_turns: usize,
    pub cli_config_path: Option<PathBuf>,
    pub ov_config_path: Option<PathBuf>,
    pub workspace_config_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl ResolvedConfig {
    /// Resolves configuration from process environment variables and standard config files.
    #[must_use]
    pub fn load(workspace_dir: Option<&Path>) -> Self {
        let env_map: HashMap<String, String> = std::env::vars().collect();
        Self::load_with_env(workspace_dir, &env_map)
    }

    /// Resolves configuration against an explicit environment map for hermetic testing.
    #[must_use]
    pub fn load_with_env(workspace_dir: Option<&Path>, env: &HashMap<String, String>) -> Self {
        let home_dir = env_non_empty(env, "HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));

        let ov_home_dir = env_non_empty(env, "OPENVIKING_HOME").map_or_else(
            || home_dir.join(".openviking"),
            |p| expand_tilde(&p, &home_dir),
        );

        let state_dir = env_non_empty(env, "OPENVIKING_PLUGIN_DATA")
            .or_else(|| env_non_empty(env, "PLUGIN_DATA"))
            .or_else(|| env_non_empty(env, "OPENVIKING_STATE_DIR"))
            .map_or_else(
                || ov_home_dir.join("agy-plugin"),
                |p| expand_tilde(&p, &home_dir),
            );

        let cred_mode = match env_non_empty(env, "OPENVIKING_CREDENTIAL_SOURCE")
            .or_else(|| env_non_empty(env, "OPENVIKING_CREDENTIALS_SOURCE"))
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "cli" | "ovcli" | "file" | "config" => CredentialMode::Cli,
            "env" | "environment" => CredentialMode::Env,
            _ => CredentialMode::Auto,
        };

        let mut warnings = Vec::new();

        let cli_candidate = env_non_empty(env, "OPENVIKING_CLI_CONFIG_FILE").map_or_else(
            || ov_home_dir.join("ovcli.conf"),
            |p| expand_tilde(&p, &home_dir),
        );
        let ov_candidate = env_non_empty(env, "OPENVIKING_CONFIG_FILE").map_or_else(
            || ov_home_dir.join("ov.conf"),
            |p| expand_tilde(&p, &home_dir),
        );

        let (mut cli_json, mut cli_path) = if cred_mode == CredentialMode::Env {
            (None, None)
        } else {
            load_json_object(&cli_candidate, &mut warnings)
        };
        let (mut ov_json, mut ov_path) = if cred_mode == CredentialMode::Env {
            (None, None)
        } else {
            load_json_object(&ov_candidate, &mut warnings)
        };

        // Backward compatibility: treat ovcli-shaped `OPENVIKING_CONFIG_FILE` as `ovcli.conf`.
        if cred_mode != CredentialMode::Env
            && env_non_empty(env, "OPENVIKING_CONFIG_FILE").is_some()
            && env_non_empty(env, "OPENVIKING_CLI_CONFIG_FILE").is_none()
            && cli_json.is_none()
            && ov_json.as_ref().is_some_and(looks_like_ovcli)
        {
            cli_json = ov_json.take();
            cli_path = ov_path.take();
        }

        let effective_cwd = workspace_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        let ws_root = effective_cwd
            .as_deref()
            .and_then(|cwd| git_peer::find_workspace_root(cwd, Some(&home_dir)));

        let mut workspace_config_paths = Vec::new();
        let ws_team = ws_root.as_ref().and_then(|root| {
            let path = root.join(".openviking").join("config.json");
            let (obj, found) = load_json_object(&path, &mut warnings);
            if let Some(found_path) = found {
                workspace_config_paths.push(found_path);
            }
            obj
        });
        let ws_local = ws_root.as_ref().and_then(|root| {
            let path = root.join(".openviking").join("config.local.json");
            let (obj, found) = load_json_object(&path, &mut warnings);
            if let Some(found_path) = found {
                workspace_config_paths.push(found_path);
            }
            obj
        });

        let plugin_shared = cli_json
            .as_ref()
            .and_then(|m| m.get("plugin"))
            .and_then(Value::as_object);
        let plugin_agy = plugin_shared.and_then(|p| {
            p.get("antigravity")
                .or_else(|| p.get("agy"))
                .and_then(Value::as_object)
        });
        let ov_agy_section = ov_json
            .as_ref()
            .and_then(|m| m.get("antigravity").or_else(|| m.get("agy")))
            .and_then(Value::as_object);
        let ov_server = ov_json
            .as_ref()
            .and_then(|m| m.get("server"))
            .and_then(Value::as_object);

        // 1. Enablement resolution waterfall.
        let (enabled, enabled_reason) =
            if let Some(raw_env) = env_non_empty(env, "OPENVIKING_MEMORY_ENABLED") {
                match parse_bool_str(&raw_env) {
                    Some(true) => (true, "env:OPENVIKING_MEMORY_ENABLED=true".to_string()),
                    Some(false) => (false, "env:OPENVIKING_MEMORY_ENABLED=false".to_string()),
                    None => (true, "env:OPENVIKING_MEMORY_ENABLED(default)".to_string()),
                }
            } else if let Some(flag) = plugin_agy.and_then(|m| json_bool(m.get("enabled"))) {
                (
                    flag,
                    format!("ovcli.conf:plugin.antigravity.enabled={flag}"),
                )
            } else if let Some(flag) = plugin_shared.and_then(|m| json_bool(m.get("enabled"))) {
                (flag, format!("ovcli.conf:plugin.enabled={flag}"))
            } else if let Some(flag) = ov_agy_section.and_then(|m| json_bool(m.get("enabled"))) {
                (flag, format!("ov.conf:antigravity.enabled={flag}"))
            } else if cli_path.is_some() {
                (true, "file:ovcli.conf".to_string())
            } else if ov_path.is_some() {
                (true, "file:ov.conf".to_string())
            } else if cred_mode == CredentialMode::Env
                && (env_non_empty(env, "OPENVIKING_URL").is_some()
                    || env_non_empty(env, "OPENVIKING_BASE_URL").is_some())
            {
                (true, "env:OPENVIKING_CREDENTIAL_SOURCE=env".to_string())
            } else {
                (false, "no_config_files".to_string())
            };

        // 2. Credential and connection priority resolution.
        let allow_env_creds = cred_mode != CredentialMode::Cli;
        let allow_file_creds = cred_mode != CredentialMode::Env;

        let (base_url_raw, url_source) = first_non_empty(&[
            if allow_env_creds {
                (
                    env_non_empty(env, "OPENVIKING_URL")
                        .or_else(|| env_non_empty(env, "OPENVIKING_BASE_URL")),
                    "env",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    ws_local.as_ref().and_then(|m| {
                        json_str(m.get("url")).or_else(|| json_str(m.get("baseUrl")))
                    }),
                    "workspace:config.local.json",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    ws_team.as_ref().and_then(|m| {
                        json_str(m.get("url")).or_else(|| json_str(m.get("baseUrl")))
                    }),
                    "workspace:config.json",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    cli_json.as_ref().and_then(|m| json_str(m.get("url"))),
                    "ovcli.conf",
                )
            } else {
                (None, "")
            },
            if allow_file_creds && cred_mode != CredentialMode::Cli {
                (
                    ov_server
                        .map(|s| json_str(s.get("url")).unwrap_or_else(|| derive_ov_server_url(s))),
                    "ov.conf",
                )
            } else {
                (None, "")
            },
        ])
        .unwrap_or_else(|| (DEFAULT_BASE_URL.to_string(), "default"));

        let base_url = base_url_raw.trim_end_matches('/').to_string();
        let mcp_url = if allow_env_creds {
            env_non_empty(env, "OPENVIKING_MCP_URL").unwrap_or_else(|| format!("{base_url}/mcp"))
        } else {
            format!("{base_url}/mcp")
        };

        let server_root_key = ov_server
            .and_then(|s| json_str(s.get("root_api_key")))
            .or_else(|| {
                cli_json
                    .as_ref()
                    .and_then(|m| json_str(m.get("root_api_key")))
            });

        let (api_key, key_source) = first_non_empty(&[
            if allow_env_creds {
                (
                    env_non_empty(env, "OPENVIKING_API_KEY")
                        .or_else(|| env_non_empty(env, "OPENVIKING_BEARER_TOKEN")),
                    "env",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    ws_local.as_ref().and_then(|m| {
                        json_str(m.get("api_key")).or_else(|| json_str(m.get("apiKey")))
                    }),
                    "workspace:config.local.json",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    ws_team.as_ref().and_then(|m| {
                        json_str(m.get("api_key")).or_else(|| json_str(m.get("apiKey")))
                    }),
                    "workspace:config.json",
                )
            } else {
                (None, "")
            },
            if allow_file_creds {
                (
                    plugin_agy
                        .and_then(|m| {
                            json_str(m.get("apiKey")).or_else(|| json_str(m.get("api_key")))
                        })
                        .or_else(|| {
                            plugin_shared.and_then(|m| {
                                json_str(m.get("apiKey")).or_else(|| json_str(m.get("api_key")))
                            })
                        })
                        .or_else(|| cli_json.as_ref().and_then(|m| json_str(m.get("api_key")))),
                    "ovcli.conf",
                )
            } else {
                (None, "")
            },
            if allow_file_creds && cred_mode != CredentialMode::Cli {
                (
                    ov_agy_section
                        .and_then(|m| {
                            json_str(m.get("apiKey")).or_else(|| json_str(m.get("api_key")))
                        })
                        .or_else(|| server_root_key.clone()),
                    "ov.conf",
                )
            } else {
                (None, "")
            },
        ])
        .unwrap_or_else(|| (String::new(), "none"));

        let credential_source = if key_source == "none" {
            url_source.to_string()
        } else {
            key_source.to_string()
        };

        // 3. Root API key misconfiguration guard.
        let root_key_warning = match (&server_root_key, api_key.is_empty()) {
            (Some(rk), false) if rk == &api_key => {
                let msg = "Configured API key matches `server.root_api_key` in ov.conf. OpenViking rejects Root API keys on `/mcp` (403 Forbidden) and personal memory endpoints. Provision a User/Admin API key via `ov config` (or `POST /api/v1/admin/accounts/<account>/users`) and set `api_key` in `~/.openviking/ovcli.conf`.".to_string();
                warnings.push(msg.clone());
                Some(msg)
            }
            _ => None,
        };

        let account = first_non_empty_val(&[
            if allow_env_creds {
                env_non_empty(env, "OPENVIKING_ACCOUNT")
            } else {
                None
            },
            if allow_file_creds {
                ws_local.as_ref().and_then(|m| {
                    json_str(m.get("account")).or_else(|| json_str(m.get("accountId")))
                })
            } else {
                None
            },
            if allow_file_creds {
                ws_team.as_ref().and_then(|m| {
                    json_str(m.get("account")).or_else(|| json_str(m.get("accountId")))
                })
            } else {
                None
            },
            if allow_file_creds {
                plugin_agy
                    .and_then(|m| json_str(m.get("accountId")))
                    .or_else(|| plugin_shared.and_then(|m| json_str(m.get("accountId"))))
                    .or_else(|| {
                        cli_json.as_ref().and_then(|m| {
                            json_str(m.get("account")).or_else(|| json_str(m.get("account_id")))
                        })
                    })
            } else {
                None
            },
            if allow_file_creds && cred_mode != CredentialMode::Cli {
                ov_agy_section.and_then(|m| json_str(m.get("accountId")))
            } else {
                None
            },
        ])
        .unwrap_or_default();

        let user = first_non_empty_val(&[
            if allow_env_creds {
                env_non_empty(env, "OPENVIKING_USER")
            } else {
                None
            },
            if allow_file_creds {
                ws_local
                    .as_ref()
                    .and_then(|m| json_str(m.get("user")).or_else(|| json_str(m.get("userId"))))
            } else {
                None
            },
            if allow_file_creds {
                ws_team
                    .as_ref()
                    .and_then(|m| json_str(m.get("user")).or_else(|| json_str(m.get("userId"))))
            } else {
                None
            },
            if allow_file_creds {
                plugin_agy
                    .and_then(|m| json_str(m.get("userId")))
                    .or_else(|| plugin_shared.and_then(|m| json_str(m.get("userId"))))
                    .or_else(|| {
                        cli_json.as_ref().and_then(|m| {
                            json_str(m.get("user")).or_else(|| json_str(m.get("user_id")))
                        })
                    })
            } else {
                None
            },
            if allow_file_creds && cred_mode != CredentialMode::Cli {
                ov_agy_section.and_then(|m| json_str(m.get("userId")))
            } else {
                None
            },
        ])
        .unwrap_or_default();

        // 4. Git worktree-aware `peer` derivation.
        let explicit_peer = first_non_empty_val(&[
            if allow_env_creds {
                env_non_empty(env, "OPENVIKING_PEER_ID")
            } else {
                None
            },
            ws_local.as_ref().and_then(|m| {
                nested_str(m, &["peer", "id"])
                    .or_else(|| json_str(m.get("peerId")))
                    .or_else(|| json_str(m.get("peer_id")))
            }),
            ws_team.as_ref().and_then(|m| {
                nested_str(m, &["peer", "id"])
                    .or_else(|| json_str(m.get("peerId")))
                    .or_else(|| json_str(m.get("peer_id")))
            }),
            if allow_file_creds {
                plugin_agy
                    .and_then(|m| json_str(m.get("peerId")).or_else(|| json_str(m.get("peer_id"))))
                    .or_else(|| {
                        plugin_shared.and_then(|m| {
                            json_str(m.get("peerId")).or_else(|| json_str(m.get("peer_id")))
                        })
                    })
                    .or_else(|| {
                        cli_json.as_ref().and_then(|m| {
                            json_str(m.get("actor_peer_id")).or_else(|| json_str(m.get("peer_id")))
                        })
                    })
            } else {
                None
            },
            if allow_file_creds && cred_mode != CredentialMode::Cli {
                ov_agy_section
                    .and_then(|m| json_str(m.get("peerId")).or_else(|| json_str(m.get("peer_id"))))
            } else {
                None
            },
        ]);

        let git_peer_res = effective_cwd
            .as_deref()
            .and_then(|cwd| git_peer::resolve_git_peer(cwd, Some(&home_dir)));

        let peer_id = if let Some(explicit) = explicit_peer {
            git_peer::sanitize_peer_id(&explicit)
        } else if let Some(ref resolved_peer) = git_peer_res {
            resolved_peer.peer_id.clone()
        } else {
            String::new()
        };

        // 5. Behavioral knobs.
        let layers: Vec<&Map<String, Value>> = [
            ws_local.as_ref(),
            ws_team.as_ref(),
            plugin_agy,
            plugin_shared,
            ov_agy_section,
        ]
        .into_iter()
        .flatten()
        .collect();

        let debug = env_bool(env, "OPENVIKING_DEBUG")
            .or_else(|| layer_bool(&layers, &["debug"]))
            .unwrap_or(false);

        let debug_log_path = env_non_empty(env, "OPENVIKING_DEBUG_LOG")
            .or_else(|| layer_str(&layers, &["debugLogPath"]))
            .map_or_else(
                || ov_home_dir.join("logs").join("agy.jsonl"),
                |p| expand_tilde(&p, &home_dir),
            );

        let timeout_ms = env_u64(env, "OPENVIKING_TIMEOUT_MS")
            .or_else(|| layer_u64(&layers, &["timeoutMs", "requestTimeoutMs"]))
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(500, 300_000);

        let auto_recall = env_bool(env, "OPENVIKING_AUTO_RECALL")
            .or_else(|| ws_nested_bool(ws_local.as_ref(), ws_team.as_ref(), &["recall", "enabled"]))
            .or_else(|| layer_bool(&layers, &["autoRecall"]))
            .unwrap_or(true);

        let auto_capture = env_bool(env, "OPENVIKING_AUTO_CAPTURE")
            .or_else(|| {
                ws_nested_bool(ws_local.as_ref(), ws_team.as_ref(), &["capture", "enabled"])
            })
            .or_else(|| layer_bool(&layers, &["autoCapture", "syncTurns"]))
            .unwrap_or(true);

        let recall_limit = env_usize(env, "OPENVIKING_RECALL_LIMIT")
            .or_else(|| {
                ws_nested_usize(
                    ws_local.as_ref(),
                    ws_team.as_ref(),
                    &["recall", "max_items"],
                )
            })
            .or_else(|| layer_usize(&layers, &["recallLimit"]))
            .unwrap_or(DEFAULT_RECALL_LIMIT)
            .clamp(1, 50);

        let recall_dedup_turns = env_usize(env, "OPENVIKING_RECALL_DEDUP_TURNS")
            .or_else(|| {
                ws_nested_usize(
                    ws_local.as_ref(),
                    ws_team.as_ref(),
                    &["recall", "dedup_turns"],
                )
            })
            .or_else(|| layer_usize(&layers, &["recallDedupTurns"]))
            .map_or(DEFAULT_RECALL_DEDUP_TURNS, |v| v.min(20) as u32);

        let score_threshold = env_f64(env, "OPENVIKING_SCORE_THRESHOLD")
            .or_else(|| {
                ws_nested_f64(
                    ws_local.as_ref(),
                    ws_team.as_ref(),
                    &["recall", "score_threshold"],
                )
            })
            .or_else(|| layer_f64(&layers, &["scoreThreshold", "recallScoreThreshold"]))
            .unwrap_or(DEFAULT_SCORE_THRESHOLD)
            .clamp(0.0, 1.0);

        let min_query_length = env_usize(env, "OPENVIKING_MIN_QUERY_LENGTH")
            .or_else(|| layer_usize(&layers, &["minQueryLength", "recallMinQueryLength"]))
            .unwrap_or(DEFAULT_MIN_QUERY_LENGTH)
            .clamp(1, 64);

        let recall_peer_scope = env_non_empty(env, "OPENVIKING_RECALL_PEER_SCOPE")
            .or_else(|| {
                ws_nested_str(
                    ws_local.as_ref(),
                    ws_team.as_ref(),
                    &["recall", "peer_scope"],
                )
            })
            .or_else(|| layer_str(&layers, &["recallPeerScope"]))
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| s == "actor" || s == "all")
            .unwrap_or_else(|| "all".to_string());

        let recall_exclude_uris = env_list(env, "OPENVIKING_RECALL_EXCLUDE_URIS")
            .or_else(|| layer_list(&layers, &["recallExcludeUris"]))
            .unwrap_or_default();

        let recall_query_filters = env_list(env, "OPENVIKING_RECALL_QUERY_FILTERS")
            .or_else(|| layer_list(&layers, &["recallQueryFilters"]))
            .unwrap_or_default();

        let capture_filters = env_list(env, "OPENVIKING_CAPTURE_FILTERS")
            .or_else(|| layer_list(&layers, &["captureFilters"]))
            .unwrap_or_default();

        let capture_max_length = env_usize(env, "OPENVIKING_CAPTURE_MAX_LENGTH")
            .or_else(|| layer_usize(&layers, &["captureMaxLength"]))
            .unwrap_or(DEFAULT_CAPTURE_MAX_LENGTH)
            .clamp(200, 100_000);

        let commit_every_turns = env_usize(env, "OPENVIKING_COMMIT_EVERY_TURNS")
            .or_else(|| env_usize(env, "OPENVIKING_COMMIT_TURN_THRESHOLD"))
            .or_else(|| layer_usize(&layers, &["commitEveryTurns", "commitTurnThreshold"]))
            .unwrap_or(DEFAULT_COMMIT_EVERY_TURNS)
            .clamp(1, 1_000);

        Self {
            enabled,
            enabled_reason,
            credential_source,
            base_url,
            mcp_url,
            api_key,
            root_key_warning,
            account,
            user,
            peer_id,
            peer_resolution: git_peer_res,
            ov_home_dir,
            state_dir,
            debug,
            debug_log_path,
            timeout_ms,
            auto_recall,
            auto_capture,
            recall_limit,
            recall_dedup_turns,
            score_threshold,
            min_query_length,
            recall_peer_scope,
            recall_exclude_uris,
            recall_query_filters,
            capture_filters,
            capture_max_length,
            commit_every_turns,
            cli_config_path: cli_path,
            ov_config_path: ov_path,
            workspace_config_paths,
            warnings,
        }
    }

    /// Returns a redacted representation of `api_key` safe for diagnostics.
    #[must_use]
    pub fn mask_api_key(&self) -> String {
        mask_secret(&self.api_key)
    }

    /// Appends a structured JSONL debug record to `debug_log_path` when debug mode is active.
    pub fn log_debug(&self, component: &str, stage: &str, detail: Value) {
        if !self.debug {
            return;
        }
        append_debug_jsonl(&self.debug_log_path, component, stage, detail);
    }

    /// Returns the per-conversation state file path inside `state_dir`.
    #[must_use]
    pub fn state_file_path(&self, conversation_id: &str) -> PathBuf {
        let safe_id = sanitize_filename(conversation_id);
        self.state_dir.join("state").join(format!("{safe_id}.json"))
    }

    /// Returns the advisory lock file path for `name`.
    #[must_use]
    pub fn lock_file_path(&self, name: &str) -> PathBuf {
        let safe_name = sanitize_filename(name);
        self.state_dir
            .join("locks")
            .join(format!("{safe_name}.lock"))
    }

    /// Returns `~/.openviking/state/last-recall.json` (honoring `OPENVIKING_HOME`).
    #[must_use]
    pub fn last_recall_path(&self) -> PathBuf {
        self.ov_home_dir.join("state").join("last-recall.json")
    }
}

/// Checks `OPENVIKING_MEMORY_ENABLED` without disk I/O for the `< 1ms` `pre-tool` fast path.
#[must_use]
pub fn is_env_explicitly_disabled() -> bool {
    if let Ok(val) = std::env::var("OPENVIKING_MEMORY_ENABLED") {
        if let Some(false) = parse_bool_str(&val) {
            return true;
        }
    }
    false
}

/// Appends a structured JSONL event to `log_path` (never `stdout`).
pub fn append_debug_jsonl(log_path: &Path, component: &str, stage: &str, detail: Value) {
    let ts_ms = now_epoch_ms();
    let mut entry = Map::with_capacity(5);
    entry.insert("ts_ms".to_string(), json!(ts_ms));
    entry.insert("pid".to_string(), json!(std::process::id()));
    entry.insert("component".to_string(), json!(component));
    entry.insert("stage".to_string(), json!(stage));
    entry.insert("detail".to_string(), detail);
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        if let Ok(serialized) = serde_json::to_string(&entry) {
            let _ = writeln!(file, "{serialized}");
        }
    }
}

/// Emits a structured JSONL debug entry if `OPENVIKING_DEBUG=1` is set in the environment.
pub fn debug_log_from_env(component: &str, stage: &str, detail: Value) {
    let enabled = std::env::var("OPENVIKING_DEBUG")
        .ok()
        .and_then(|v| parse_bool_str(&v))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let log_path = std::env::var("OPENVIKING_DEBUG_LOG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map_or_else(
            || {
                let ov_home = std::env::var("OPENVIKING_HOME")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .map_or_else(|| home.join(".openviking"), |p| expand_tilde(&p, &home));
                ov_home.join("logs").join("agy.jsonl")
            },
            |p| expand_tilde(&p, &home),
        );
    append_debug_jsonl(&log_path, component, stage, detail);
}

/// Returns the current Unix epoch timestamp in milliseconds.
#[must_use]
pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Sanitizes an arbitrary identifier for safe single-segment filenames.
#[must_use]
pub fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else if cleaned.len() > 120 {
        cleaned[..120].to_string()
    } else {
        cleaned
    }
}

/// Masks a secret string so raw credentials never leak into logs or terminal output.
#[must_use]
pub fn mask_secret(secret: &str) -> String {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return "<none>".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return format!("*** ({} chars)", chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}...{suffix} ({} chars)", chars.len())
}

// ── Advisory File Lock & Atomic JSON Persistence ──────────────────────────

/// RAII guard holding an exclusive `fs4` advisory file lock.
#[must_use]
pub struct AdvisoryLock {
    file: File,
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        use fs4::fs_std::FileExt;
        let _ = FileExt::unlock(&self.file);
    }
}

/// Acquires an exclusive advisory file lock on `lock_path` within `wait_budget`.
pub fn acquire_exclusive_lock(lock_path: &Path, wait_budget: Duration) -> Result<AdvisoryLock> {
    use fs4::fs_std::FileExt;

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create lock directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;

    let start = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(AdvisoryLock { file }),
            Err(err) => {
                if start.elapsed() >= wait_budget {
                    return Err(err).with_context(|| {
                        format!(
                            "timed out or failed acquiring exclusive lock on {}",
                            lock_path.display()
                        )
                    });
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Atomically serializes `value` as JSON via temporary file write + `sync_all()` + `rename`.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent directory {}", parent.display()))?;
    }
    let serialized = serde_json::to_vec_pretty(value)?;
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut temp_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("failed creating temp file {}", tmp_path.display()))?;
        temp_file.write_all(&serialized)?;
        temp_file.sync_all()?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed renaming {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Reads and deserializes an optional JSON file, returning `None` if missing or invalid.
#[must_use]
pub fn read_json_optional<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Loads `ConversationState` for `conversation_id`, or returns a fresh default record.
#[must_use]
pub fn load_conversation_state(cfg: &ResolvedConfig, conversation_id: &str) -> ConversationState {
    let path = cfg.state_file_path(conversation_id);
    read_json_optional::<ConversationState>(&path)
        .unwrap_or_else(|| ConversationState::new(conversation_id))
}

/// Atomically mutates `ConversationState` under an exclusive advisory lock.
pub fn update_conversation_state<F>(
    cfg: &ResolvedConfig,
    conversation_id: &str,
    mutate: F,
) -> Result<ConversationState>
where
    F: FnOnce(&mut ConversationState),
{
    let lock_path = cfg.lock_file_path(&format!("state-{conversation_id}"));
    let _guard = acquire_exclusive_lock(&lock_path, Duration::from_secs(2))?;
    let state_path = cfg.state_file_path(conversation_id);
    let mut state = read_json_optional::<ConversationState>(&state_path)
        .unwrap_or_else(|| ConversationState::new(conversation_id));
    if state.conversation_id.is_empty() {
        state.conversation_id = conversation_id.to_string();
    }
    if state.ov_session_id.is_empty() {
        state.ov_session_id = format!("agy-{conversation_id}");
    }
    mutate(&mut state);
    write_json_atomic(&state_path, &state)?;
    Ok(state)
}

// ── Regex Input Filter Engine (sed-style s/d/k + plain regex drop) ────────

/// Operation mode for a compiled input filter rule.
#[derive(Debug, Clone)]
pub enum FilterOp {
    Substitute { replacement: String, global: bool },
    Drop,
    Keep,
}

/// Compiled regular-expression filter rule for recall queries or captured turns.
#[derive(Debug, Clone)]
pub struct CompiledFilter {
    pub scope: Option<String>,
    pub regex: Regex,
    pub op: FilterOp,
}

/// Outcome of applying a sequence of `CompiledFilter` rules to input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterVerdict {
    pub text: String,
    pub dropped: bool,
}

/// Compiles raw filter strings, skipping malformed rules safely without panicking.
#[must_use]
pub fn compile_input_filters(raw_rules: &[String]) -> Vec<CompiledFilter> {
    raw_rules
        .iter()
        .filter_map(|rule| parse_single_filter(rule))
        .collect()
}

fn parse_single_filter(raw: &str) -> Option<CompiledFilter> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (scope, rest) = if let Some(after) = trimmed.strip_prefix("user:") {
        (Some("user".to_string()), after)
    } else if let Some(after) = trimmed.strip_prefix("assistant:") {
        (Some("assistant".to_string()), after)
    } else {
        (None, trimmed)
    };

    let mut chars = rest.chars();
    let first = chars.next()?;
    let second = chars.next();

    if matches!(first, 's' | 'd' | 'k') {
        if let Some(delim) = second {
            if !delim.is_ascii_alphanumeric() && !delim.is_whitespace() && delim != '\\' {
                let body = &rest[first.len_utf8() + delim.len_utf8()..];
                return parse_delimited_rule(scope, first, delim, body);
            }
        }
    }

    let regex = Regex::new(rest).ok()?;
    Some(CompiledFilter {
        scope,
        regex,
        op: FilterOp::Drop,
    })
}

fn parse_delimited_rule(
    scope: Option<String>,
    op_char: char,
    delim: char,
    body: &str,
) -> Option<CompiledFilter> {
    let (pattern, after_first) = scan_delimited(body, delim);
    if pattern.is_empty() {
        return None;
    }

    let (op, flags_str) = if op_char == 's' {
        let after_idx = after_first?;
        let (replacement, after_second) = scan_delimited(&body[after_idx..], delim);
        let after_rep_idx = after_second?;
        let flags = &body[after_idx + after_rep_idx..];
        let global = flags.contains('g');
        (
            FilterOp::Substitute {
                replacement,
                global,
            },
            flags,
        )
    } else {
        let flags = match after_first {
            Some(idx) => &body[idx..],
            None => "",
        };
        let op = if op_char == 'd' {
            FilterOp::Drop
        } else {
            FilterOp::Keep
        };
        (op, flags)
    };

    let mut builder = RegexBuilder::new(&pattern);
    for flag in flags_str.chars() {
        match flag {
            'i' => {
                builder.case_insensitive(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            's' => {
                builder.dot_matches_new_line(true);
            }
            'u' | 'g' => {}
            _ => return None,
        }
    }
    let regex = builder.build().ok()?;
    Some(CompiledFilter { scope, regex, op })
}

fn scan_delimited(input: &str, delim: char) -> (String, Option<usize>) {
    let mut out = String::new();
    let mut iter = input.char_indices();
    while let Some((idx, ch)) = iter.next() {
        if ch == '\\' {
            if let Some((_, next_ch)) = iter.next() {
                if next_ch == delim {
                    out.push(delim);
                } else {
                    out.push('\\');
                    out.push(next_ch);
                }
            } else {
                out.push('\\');
            }
            continue;
        }
        if ch == delim {
            return (out, Some(idx + ch.len_utf8()));
        }
        out.push(ch);
    }
    (out, None)
}

/// Applies compiled filter rules in order for the given `role` (`"user"` or `"assistant"`).
#[must_use]
pub fn apply_input_filters(text: &str, rules: &[CompiledFilter], role: &str) -> FilterVerdict {
    let mut current = text.to_string();
    for rule in rules {
        if let Some(ref rule_scope) = rule.scope {
            if rule_scope != role {
                continue;
            }
        }
        match &rule.op {
            FilterOp::Substitute {
                replacement,
                global,
            } => {
                current = if *global {
                    rule.regex
                        .replace_all(&current, replacement.as_str())
                        .into_owned()
                } else {
                    rule.regex
                        .replace(&current, replacement.as_str())
                        .into_owned()
                };
            }
            FilterOp::Drop => {
                if rule.regex.is_match(&current) {
                    return FilterVerdict {
                        text: String::new(),
                        dropped: true,
                    };
                }
            }
            FilterOp::Keep => {
                if !rule.regex.is_match(&current) {
                    return FilterVerdict {
                        text: String::new(),
                        dropped: true,
                    };
                }
            }
        }
    }

    FilterVerdict {
        text: current.trim().to_string(),
        dropped: false,
    }
}

// ── Internal Config Parsing Helpers ───────────────────────────────────────

fn expand_tilde(raw: &str, home_dir: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir.join(rest)
    } else {
        PathBuf::from(trimmed)
    }
}

fn env_non_empty(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn env_bool(env: &HashMap<String, String>, key: &str) -> Option<bool> {
    env_non_empty(env, key).and_then(|v| parse_bool_str(&v))
}

fn env_u64(env: &HashMap<String, String>, key: &str) -> Option<u64> {
    env_non_empty(env, key).and_then(|v| v.parse::<u64>().ok())
}

fn env_usize(env: &HashMap<String, String>, key: &str) -> Option<usize> {
    env_non_empty(env, key).and_then(|v| v.parse::<usize>().ok())
}

fn env_f64(env: &HashMap<String, String>, key: &str) -> Option<f64> {
    env_non_empty(env, key)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

fn env_list(env: &HashMap<String, String>, key: &str) -> Option<Vec<String>> {
    let raw = env_non_empty(env, key)?;
    if raw.starts_with('[') {
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(&raw) {
            let mut items = Vec::with_capacity(arr.len());
            for item in arr {
                let trimmed = item.trim();
                if !trimmed.is_empty() {
                    items.push(trimmed.to_string());
                }
            }
            return Some(items);
        }
    }
    let mut items = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            items.push(trimmed.to_string());
        }
    }
    Some(items)
}

/// Parses standard truthy/falsy string literals (`1|true|yes|on` vs `0|false|no|off`).
#[must_use]
pub fn parse_bool_str(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn load_json_object(
    path: &Path,
    warnings: &mut Vec<String>,
) -> (Option<Map<String, Value>>, Option<PathBuf>) {
    if !path.is_file() {
        return (None, None);
    }
    match fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(Value::Object(map)) => (Some(map), Some(path.to_path_buf())),
            Ok(_) => {
                warnings.push(format!(
                    "Config file {} does not contain a top-level JSON object",
                    path.display()
                ));
                (None, None)
            }
            Err(err) => {
                warnings.push(format!("Failed to parse {}: {}", path.display(), err));
                (None, None)
            }
        },
        Err(err) => {
            warnings.push(format!("Failed to read {}: {}", path.display(), err));
            (None, None)
        }
    }
}

fn looks_like_ovcli(map: &Map<String, Value>) -> bool {
    if map.get("server").and_then(Value::as_object).is_some() {
        return false;
    }
    ["url", "api_key", "account", "user", "plugin"]
        .iter()
        .any(|key| map.contains_key(*key))
}

fn derive_ov_server_url(server: &Map<String, Value>) -> String {
    let host = json_str(server.get("host"))
        .unwrap_or_else(|| "127.0.0.1".to_string())
        .replace("0.0.0.0", "127.0.0.1");
    let port = server.get("port").and_then(Value::as_u64).unwrap_or(1933);
    format!("http://{host}:{port}")
}

fn json_str(val: Option<&Value>) -> Option<String> {
    match val? {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn json_bool(val: Option<&Value>) -> Option<bool> {
    match val? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => parse_bool_str(s),
        _ => None,
    }
}

fn json_u64(val: Option<&Value>) -> Option<u64> {
    match val? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn json_f64(val: Option<&Value>) -> Option<f64> {
    match val? {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

fn nested_val<'a>(map: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = map.get(*first)?;
    for segment in rest {
        current = current.as_object()?.get(*segment)?;
    }
    Some(current)
}

fn nested_str(map: &Map<String, Value>, path: &[&str]) -> Option<String> {
    json_str(nested_val(map, path))
}

fn ws_nested_bool(
    ws_local: Option<&Map<String, Value>>,
    ws_team: Option<&Map<String, Value>>,
    path: &[&str],
) -> Option<bool> {
    ws_local
        .and_then(|m| json_bool(nested_val(m, path)))
        .or_else(|| ws_team.and_then(|m| json_bool(nested_val(m, path))))
}

fn ws_nested_usize(
    ws_local: Option<&Map<String, Value>>,
    ws_team: Option<&Map<String, Value>>,
    path: &[&str],
) -> Option<usize> {
    ws_local
        .and_then(|m| json_u64(nested_val(m, path)).map(|v| v as usize))
        .or_else(|| ws_team.and_then(|m| json_u64(nested_val(m, path)).map(|v| v as usize)))
}

fn ws_nested_f64(
    ws_local: Option<&Map<String, Value>>,
    ws_team: Option<&Map<String, Value>>,
    path: &[&str],
) -> Option<f64> {
    ws_local
        .and_then(|m| json_f64(nested_val(m, path)))
        .or_else(|| ws_team.and_then(|m| json_f64(nested_val(m, path))))
}

fn ws_nested_str(
    ws_local: Option<&Map<String, Value>>,
    ws_team: Option<&Map<String, Value>>,
    path: &[&str],
) -> Option<String> {
    ws_local
        .and_then(|m| nested_str(m, path))
        .or_else(|| ws_team.and_then(|m| nested_str(m, path)))
}

fn layer_str(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<String> {
    for layer in layers {
        for key in keys {
            if let Some(s) = json_str(layer.get(*key)) {
                return Some(s);
            }
        }
    }
    None
}

fn layer_bool(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<bool> {
    for layer in layers {
        for key in keys {
            if let Some(b) = json_bool(layer.get(*key)) {
                return Some(b);
            }
        }
    }
    None
}

fn layer_u64(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<u64> {
    for layer in layers {
        for key in keys {
            if let Some(n) = json_u64(layer.get(*key)) {
                return Some(n);
            }
        }
    }
    None
}

fn layer_usize(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<usize> {
    layer_u64(layers, keys).map(|v| v as usize)
}

fn layer_f64(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<f64> {
    for layer in layers {
        for key in keys {
            if let Some(n) = json_f64(layer.get(*key)) {
                return Some(n);
            }
        }
    }
    None
}

fn layer_list(layers: &[&Map<String, Value>], keys: &[&str]) -> Option<Vec<String>> {
    for layer in layers {
        for key in keys {
            if let Some(val) = layer.get(*key) {
                if let Some(arr) = val.as_array() {
                    let mut items = Vec::with_capacity(arr.len());
                    for entry in arr {
                        if let Some(s) = entry.as_str() {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                items.push(trimmed.to_string());
                            }
                        }
                    }
                    return Some(items);
                }
                if let Some(s) = val.as_str() {
                    let mut items = Vec::new();
                    for part in s.split(',') {
                        let trimmed = part.trim();
                        if !trimmed.is_empty() {
                            items.push(trimmed.to_string());
                        }
                    }
                    return Some(items);
                }
            }
        }
    }
    None
}

fn first_non_empty<'a>(candidates: &[(Option<String>, &'a str)]) -> Option<(String, &'a str)> {
    for (val, source) in candidates {
        if let Some(v) = val {
            if !v.trim().is_empty() {
                return Some((v.clone(), *source));
            }
        }
    }
    None
}

fn first_non_empty_val(candidates: &[Option<String>]) -> Option<String> {
    for v in candidates.iter().flatten() {
        if !v.trim().is_empty() {
            return Some(v.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn silently_disables_when_no_config_or_env() {
        let temp_dir = TempDir::new().expect("tempdir should be created");
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), temp_dir.path().display().to_string());
        let cfg = ResolvedConfig::load_with_env(Some(temp_dir.path()), &env);
        assert!(
            !cfg.enabled,
            "plugin must default to disabled when neither ovcli.conf nor ov.conf exists"
        );
        assert_eq!(
            cfg.base_url, "http://127.0.0.1:1933",
            "default base_url should point to loopback port 1933"
        );
    }

    #[test]
    fn detects_root_api_key_misconfiguration() {
        let temp_dir = TempDir::new().expect("tempdir should be created");
        let ov_dir = temp_dir.path().join(".openviking");
        fs::create_dir_all(&ov_dir).expect("ov config dir should be created");
        fs::write(
            ov_dir.join("ov.conf"),
            r#"{"server":{"port":1933,"root_api_key":"root-secret-123"}}"#,
        )
        .expect("ov.conf should be written");
        fs::write(
            ov_dir.join("ovcli.conf"),
            r#"{"url":"http://127.0.0.1:1933","api_key":"root-secret-123"}"#,
        )
        .expect("ovcli.conf should be written");

        let mut env = HashMap::new();
        env.insert("HOME".to_string(), temp_dir.path().display().to_string());
        let cfg = ResolvedConfig::load_with_env(Some(temp_dir.path()), &env);
        assert!(
            cfg.enabled,
            "plugin should be enabled when ovcli.conf exists"
        );
        assert!(
            cfg.root_key_warning.is_some(),
            "matching server.root_api_key must trigger root_key_warning"
        );
    }

    #[test]
    fn applies_sed_and_plain_input_filters() {
        let rules = compile_input_filters(&[
            "s/^\\s*ultrathink\\s+//i".to_string(),
            "user:d|^/clear|".to_string(),
        ]);

        let sub = apply_input_filters("Ultrathink explain rustls", &rules, "user");
        assert!(!sub.dropped, "substitution filter must not drop prompt");
        assert_eq!(
            sub.text, "explain rustls",
            "case-insensitive prefix substitution should strip 'Ultrathink'"
        );

        let dropped = apply_input_filters("/clear", &rules, "user");
        assert!(dropped.dropped, "drop filter should match /clear command");
    }
}
