//! `ov-agy setup`: writes the server url and api key to `ovcli.conf`, merged with what is
//! already there, after checking them against the server. The previous file is kept as
//! `ovcli.conf.bak-<epoch ms>`; both are mode 0600. The `OpenViking` CLI's `ov config` does the
//! same job for people who have it installed.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde_json::{Map, Value};

use crate::cli::SetupArgs;
use crate::config::{self, PrivateOpenMode, ResolvedConfig};
use crate::doctor::lint_base_url;
use crate::wire::DoctorLevel;

const DEFAULT_URL: &str = "http://127.0.0.1:1933";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);

/// Runs the setup and returns the exit code.
pub async fn run(args: SetupArgs) -> i32 {
    match setup(&args).await {
        Ok(()) => 0,
        Err(message) => {
            say(&format!("ov-agy setup: {message}"));
            1
        }
    }
}

/// Prompts and progress go to stderr, so `--dry-run` output on stdout stays clean.
fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

async fn setup(args: &SetupArgs) -> Result<(), String> {
    let cfg = ResolvedConfig::load(None);
    let path = target_path(&cfg);
    let existing = read_existing(&path)?;
    let interactive = !args.yes && !args.api_key_stdin && io::stdin().is_terminal();
    let current = |key: &str| {
        existing
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };

    let url = match args.url.as_deref() {
        Some(url) => url.trim().to_string(),
        None if interactive => {
            let shown = current("url").unwrap_or_else(|| DEFAULT_URL.to_string());
            prompt(&format!("OpenViking url [{shown}]: "))?.unwrap_or(shown)
        }
        None => current("url").unwrap_or_else(|| DEFAULT_URL.to_string()),
    };
    let url = url.trim_end_matches('/').to_string();
    if let Some((_, message, fix)) = lint_base_url(&url)
        .into_iter()
        .find(|(level, ..)| *level == DoctorLevel::Fail)
    {
        return Err(format!("{message}; {fix}"));
    }

    let key = if args.api_key_stdin {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("cannot read the api key from stdin: {err}"))?;
        Some(line.trim().to_string())
    } else if let Some(ref var) = args.api_key_env {
        Some(
            std::env::var(var)
                .map_err(|_| format!("{var} is not set"))?
                .trim()
                .to_string(),
        )
    } else if interactive {
        let keep = if current("api_key").is_some() {
            " (Enter keeps the current key)"
        } else {
            ""
        };
        prompt_secret(&format!("API key{keep}: "))?.or_else(|| current("api_key"))
    } else {
        current("api_key")
    };
    let key = key.filter(|k| !k.is_empty());
    let account = args.account.clone().or_else(|| current("account"));
    let user = args.user.clone().or_else(|| current("user"));

    if args.no_verify {
        say("Not checking the values against the server (--no-verify).");
    } else {
        let identity = verify(
            &cfg,
            &url,
            key.as_deref(),
            account.as_deref(),
            user.as_deref(),
        )
        .await?;
        say(&format!("Server accepted the credentials: {identity}"));
    }

    let mut merged = existing.clone();
    merged.insert("url".to_string(), Value::String(url));
    if let Some(ref key) = key {
        merged.insert("api_key".to_string(), Value::String(key.clone()));
    }
    for (name, value) in [("account", &args.account), ("user", &args.user)] {
        if let Some(value) = value {
            merged.insert(name.to_string(), Value::String(value.trim().to_string()));
        }
    }
    if merged
        .get("plugin")
        .and_then(|p| p.get("antigravity"))
        .and_then(|a| a.get("enabled"))
        == Some(&Value::Bool(false))
    {
        say("Note: plugin.antigravity.enabled is false in this file, so the plugin stays off until you set it to true.");
    }

    if args.dry_run {
        let mut shown = merged.clone();
        if let Some(key) = key {
            shown.insert(
                "api_key".to_string(),
                Value::String(config::mask_secret(&key)),
            );
        }
        let pretty = serde_json::to_string_pretty(&Value::Object(shown)).unwrap_or_default();
        crate::write_stdout_raw_line(&pretty);
        say(&format!("Dry run: {} was not changed.", path.display()));
        return Ok(());
    }

    let backup = backup(&path)?;
    config::write_json_atomic(&path, &Value::Object(merged))
        .map_err(|err| format!("cannot write {}: {err:#}", path.display()))?;
    say(&format!("Wrote {} (mode 0600).", path.display()));
    if let Some(backup) = backup {
        say(&format!("The previous file is at {}.", backup.display()));
    }
    say("Start a new agy session to use it. If memory does not show up, run `ov-agy doctor`.");
    Ok(())
}

/// `OPENVIKING_CLI_CONFIG_FILE`, or the `ovcli.conf` the plugin already reads.
fn target_path(cfg: &ResolvedConfig) -> PathBuf {
    std::env::var_os("OPENVIKING_CLI_CONFIG_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| cfg.cli_config_path.clone())
        .unwrap_or_else(|| cfg.ov_home_dir.join("ovcli.conf"))
}

/// The current file as a JSON object; refuses to overwrite one that does not parse.
fn read_existing(path: &Path) -> Result<Map<String, Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(format!(
            "{} is not a JSON object; fix or move it, then run setup again",
            path.display()
        )),
    }
}

fn backup(path: &Path) -> Result<Option<PathBuf>, String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(None);
    };
    let name = path
        .file_name()
        .map_or_else(|| "ovcli.conf".into(), |n| n.to_string_lossy().into_owned());
    let target = path.with_file_name(format!("{name}.bak-{}", config::now_epoch_ms()));
    let mut file = config::open_private_file(&target, PrivateOpenMode::TruncateWrite)
        .map_err(|err| format!("cannot create the backup {}: {err}", target.display()))?;
    file.write_all(&bytes)
        .map_err(|err| format!("cannot write the backup {}: {err}", target.display()))?;
    Ok(Some(target))
}

fn prompt(question: &str) -> Result<Option<String>, String> {
    let _ = write!(io::stderr(), "{question}");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| format!("cannot read the answer: {err}"))?;
    let answer = line.trim();
    Ok((!answer.is_empty()).then(|| answer.to_string()))
}

/// Reads a line with the terminal echo off, so the key does not appear on screen.
fn prompt_secret(question: &str) -> Result<Option<String>, String> {
    let echo = |on: bool| {
        #[cfg(unix)]
        {
            std::process::Command::new("stty")
                .arg(if on { "echo" } else { "-echo" })
                .stdin(std::process::Stdio::inherit())
                .status()
                .is_ok_and(|status| status.success())
        }
        #[cfg(not(unix))]
        {
            let _ = on;
            false
        }
    };
    let hidden = echo(false);
    let answer = prompt(question);
    if hidden {
        echo(true);
        say("");
    }
    answer
}

/// Checks the values against the server: reachable, the key accepted, not the root key, and
/// the tenant API open to it. Returns the identity the server resolved.
async fn verify(
    cfg: &ResolvedConfig,
    url: &str,
    key: Option<&str>,
    account: Option<&str>,
    user: Option<&str>,
) -> Result<String, String> {
    let https = url.to_ascii_lowercase().starts_with("https://");
    let http = crate::client::tls_client_builder(cfg, https)
        .timeout(VERIFY_TIMEOUT)
        .build()
        .map_err(|err| err.to_string())?;
    let mut headers = HeaderMap::new();
    if let Some(key) = key {
        let bearer = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| "the api key contains characters that cannot go in a header")?;
        headers.insert(AUTHORIZATION, bearer);
    }
    if let (Some(account), Some(user)) = (account, user) {
        for (name, value) in [
            ("x-openviking-account", account),
            ("x-openviking-user", user),
        ] {
            if let Ok(value) = HeaderValue::from_str(value.trim()) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
    }
    let unverified = "; pass --no-verify to write it anyway";
    let resp = http
        .get(format!("{url}/health"))
        .headers(headers.clone())
        .send()
        .await
        .map_err(|err| format!("cannot reach {url}: {err}{unverified}"))?;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    match status {
        200 => {}
        401 | 403 => {
            return Err(format!(
                "the server rejected the api key ({status}){unverified}"
            ))
        }
        _ => {
            return Err(format!(
            "GET {url}/health answered {status}; is this the OpenViking server root?{unverified}"
        ))
        }
    }
    let auth_mode = body["auth_mode"].as_str().unwrap_or_default();
    let role = body["role"].as_str().unwrap_or_default();
    if role == "root" {
        return Err("this is the server's ROOT key, which OpenViking rejects on /mcp and on tenant data; create a user key with it and use that one".to_string());
    }
    if auth_mode == "api_key" && key.is_none() {
        return Err("the server requires an api key (auth_mode=api_key)".to_string());
    }
    if auth_mode == "trusted" && (account.is_none() || user.is_none()) {
        return Err("the server is in trusted mode: pass --account and --user".to_string());
    }
    let tenant = http
        .get(format!("{url}/api/v1/system/status"))
        .headers(headers)
        .send()
        .await
        .map_err(|err| format!("GET /api/v1/system/status failed: {err}{unverified}"))?
        .status()
        .as_u16();
    if !(200..300).contains(&tenant) {
        return Err(format!(
            "GET /api/v1/system/status answered {tenant}{unverified}"
        ));
    }
    Ok(
        match (body["account_id"].as_str(), body["user_id"].as_str()) {
            (Some(account), Some(user)) => format!("account={account} user={user} role={role}"),
            _ => format!("auth_mode={auth_mode}"),
        },
    )
}
