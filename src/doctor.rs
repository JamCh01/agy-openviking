//! `ov-agy doctor`: install, configuration, server and recent-activity checks, reported as
//! ok/info/warn/fail findings with fixes, in the shape of the Codex and Claude Code doctors.
//! The human report exits 1 when a check fails; `--json` always exits 0 and carries `ok`.

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Client, Method};
use serde_json::{json, Value};

use crate::cli::DoctorArgs;
use crate::client::{build_ov_headers, ActorPeer};
use crate::config::{self, ResolvedConfig};
use crate::wal::{self, WalStore};
use crate::wire::{
    DataDirReport, DoctorFinding, DoctorLevel, DoctorReport, DoctorSection, LastCaptureSnapshot,
    LastRecallSnapshot, WalScopeReport,
};

/// Hook events the plugin registers; a missing one silently disables a feature.
const HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "PreInvocation",
    "PostInvocation",
    "PreToolUse",
    "Stop",
];
/// Tools the skills and rules tell the model to use.
const CORE_MCP_TOOLS: [&str; 7] = ["search", "find", "read", "list", "tree", "grep", "glob"];
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
/// The debug log tail scanned for recent errors.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Runs the doctor and returns the process exit code.
pub async fn run(args: DoctorArgs) -> i32 {
    let report = build_report(&args).await;
    if args.json {
        if let Ok(pretty) = serde_json::to_string_pretty(&report) {
            crate::write_stdout_raw_line(&pretty);
        }
        return 0;
    }
    crate::write_stdout_raw_line(&render(&report));
    i32::from(!report.ok)
}

#[derive(Default)]
struct Findings {
    sections: Vec<DoctorSection>,
}

impl Findings {
    fn section(&mut self, title: &str) {
        self.sections.push(DoctorSection {
            title: title.to_string(),
            findings: Vec::new(),
        });
    }

    fn push(&mut self, level: DoctorLevel, title: impl Into<String>, detail: &str, fix: &str) {
        if self.sections.is_empty() {
            self.section("General");
        }
        let finding = DoctorFinding {
            level,
            title: title.into(),
            detail: (!detail.is_empty()).then(|| detail.to_string()),
            fix: (!fix.is_empty()).then(|| fix.to_string()),
        };
        if let Some(section) = self.sections.last_mut() {
            section.findings.push(finding);
        }
    }

    fn ok(&mut self, title: impl Into<String>) {
        self.push(DoctorLevel::Ok, title, "", "");
    }

    fn info(&mut self, title: impl Into<String>, detail: &str) {
        self.push(DoctorLevel::Info, title, detail, "");
    }

    fn warn(&mut self, title: impl Into<String>, detail: &str, fix: &str) {
        self.push(DoctorLevel::Warn, title, detail, fix);
    }

    fn fail(&mut self, title: impl Into<String>, detail: &str, fix: &str) {
        self.push(DoctorLevel::Fail, title, detail, fix);
    }

    fn all(&self) -> impl Iterator<Item = &DoctorFinding> {
        self.sections.iter().flat_map(|s| &s.findings)
    }
}

/// What the server probes found, for the flat fields of the JSON report.
#[derive(Default)]
struct ServerFacts {
    health_ok: Option<bool>,
    latency_ms: Option<u128>,
    version: Option<String>,
    auth_mode: Option<String>,
    account: Option<String>,
    user: Option<String>,
    role: Option<String>,
    ready: Option<bool>,
    mcp_ok: Option<bool>,
    mcp_status: Option<u16>,
    mcp_session: Option<bool>,
    mcp_tools: Option<usize>,
    root_key_warning: Option<String>,
}

async fn build_report(args: &DoctorArgs) -> DoctorReport {
    let cfg = ResolvedConfig::load(args.cwd.as_deref());
    let mut f = Findings::default();

    let install = check_install(&mut f);
    check_config(&mut f, &cfg, args);

    let mut facts = ServerFacts {
        root_key_warning: cfg.root_key_warning.clone(),
        ..ServerFacts::default()
    };
    if args.offline {
        f.section("Server");
        f.info("server checks skipped (--offline)", "");
    } else {
        let timeout = Duration::from_millis(args.timeout_ms.clamp(250, 30_000));
        check_server(&mut f, &cfg, timeout, &mut facts).await;
    }

    let wal = WalStore::new(&cfg);
    let wal_stats = wal.stats();
    let data_dirs: Vec<DataDirReport> = candidate_data_dirs(&cfg)
        .into_iter()
        .filter(|(dir, active)| *active || dir.is_dir())
        .map(|(dir, active)| survey_data_dir(&dir, active, wal.scope()))
        .collect();
    check_activity(&mut f, &cfg, &data_dirs, wal_stats.entry_count);

    let warnings: Vec<String> = f
        .all()
        .filter(|finding| matches!(finding.level, DoctorLevel::Warn | DoctorLevel::Fail))
        .map(|finding| finding.title.clone())
        .collect();
    let ok = !f.all().any(|finding| finding.level == DoctorLevel::Fail);
    let non_empty = |value: &str| (!value.is_empty()).then(|| value.to_string());

    DoctorReport {
        plugin_version: env!("CARGO_PKG_VERSION").to_string(),
        enabled: cfg.enabled,
        enabled_reason: cfg.enabled_reason.clone(),
        credential_source: cfg.credential_source.clone(),
        base_url: cfg.base_url.clone(),
        mcp_url: cfg.mcp_url.clone(),
        api_key_masked: cfg.mask_api_key(),
        has_api_key: !cfg.api_key.is_empty(),
        root_key_warning: facts.root_key_warning,
        account: non_empty(&cfg.account),
        user: non_empty(&cfg.user),
        peer_id: non_empty(&cfg.peer_id),
        peer_source: Some(cfg.peer_origin.clone()),
        git_is_worktree: cfg.peer_resolution.as_ref().is_some_and(|r| r.is_worktree),
        cli_config_path: cfg
            .cli_config_path
            .as_ref()
            .map(|p| p.display().to_string()),
        ov_config_path: cfg.ov_config_path.as_ref().map(|p| p.display().to_string()),
        workspace_config_paths: cfg
            .workspace_config_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        state_dir: cfg.state_dir.display().to_string(),
        pending_wal_entries: wal_stats.entry_count,
        pending_wal_bytes: wal_stats.byte_size,
        data_dirs,
        recall_timeout_ms: cfg.recall_timeout_ms,
        recall_query_expansion: cfg.effective_query_expansion().map(str::to_string),
        commit_token_threshold: cfg.commit_token_threshold,
        capture_tool_calls: cfg.capture_tool_calls,
        idle_sweep: cfg.idle_sweep,
        health_ok: facts.health_ok,
        health_latency_ms: facts.latency_ms,
        server_version: facts.version,
        server_auth_mode: facts.auth_mode,
        mcp_handshake_ok: facts.mcp_ok,
        mcp_status_code: facts.mcp_status,
        mcp_session_id_present: facts.mcp_session,
        mcp_tool_count: facts.mcp_tools,
        server_account: facts.account,
        server_user: facts.user,
        server_role: facts.role,
        server_ready: facts.ready,
        installed_plugin_version: install.version,
        installed_plugin_path: install.root.map(|p| p.display().to_string()),
        ok,
        sections: f.sections,
        warnings,
    }
}

// ── Plugin install ─────────────────────────────────────────────────────────

#[derive(Default)]
struct Install {
    root: Option<PathBuf>,
    version: Option<String>,
}

fn check_install(f: &mut Findings) -> Install {
    f.section("Plugin install");
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    f.ok(format!("ov-agy {} ({exe})", env!("CARGO_PKG_VERSION")));

    let home = dirs::home_dir();
    let root = std::env::var_os("PLUGIN_ROOT")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            home.as_ref()
                .map(|h| h.join(".gemini/config/plugins/openviking"))
                .filter(|p| p.exists())
        });
    let Some(root) = root else {
        f.warn(
            "the plugin is not installed for agy",
            "no ~/.gemini/config/plugins/openviking",
            "agy plugin install https://github.com/JamCh01/agy-openviking",
        );
        return Install::default();
    };
    let resolved = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let shown = if resolved == root {
        root.display().to_string()
    } else {
        format!("{} -> {}", root.display(), resolved.display())
    };
    let version = [".antigravity-plugin/plugin.json", "plugin.json"]
        .iter()
        .map(|name| resolved.join(name))
        .find(|path| path.is_file())
        .and_then(|path| config::read_json_optional::<Value>(&path))
        .and_then(|manifest| manifest["version"].as_str().map(str::to_string));
    match version.as_deref() {
        Some(v) if v == env!("CARGO_PKG_VERSION") => f.ok(format!("plugin v{v} at {shown}")),
        Some(v) => f.warn(
            format!(
                "plugin v{v} at {shown}, but this binary is v{}",
                env!("CARGO_PKG_VERSION")
            ),
            "the hooks run the binary that matches the manifest, not this one",
            "run the doctor through the plugin's scripts/ov-agy, or reinstall the plugin",
        ),
        None => f.fail(
            format!("no readable plugin manifest under {shown}"),
            "",
            "reinstall the plugin",
        ),
    }

    if let Some(agy) = home
        .as_ref()
        .and_then(|h| config::read_json_optional::<Value>(&h.join(".gemini/config/config.json")))
    {
        match agy["plugins"]["openviking"]["enabled"].as_bool() {
            Some(true) => f.ok("enabled in agy"),
            Some(false) => f.fail(
                "agy has the plugin disabled",
                "~/.gemini/config/config.json: plugins.openviking.enabled = false",
                "agy plugin enable openviking (or turn it on in agy's Customizations page), then start a new session",
            ),
            None => f.info("agy has no enablement entry for the plugin", ""),
        }
    }
    check_hooks(f, &resolved);
    check_launcher(f, &resolved);
    check_mcp_config(f, &resolved);
    Install {
        root: Some(root),
        version,
    }
}

fn check_hooks(f: &mut Findings, root: &Path) {
    let path = root.join("hooks.json");
    let Some(hooks) = config::read_json_optional::<Value>(&path) else {
        f.fail(
            "hooks.json is missing or not JSON",
            &path.display().to_string(),
            "reinstall the plugin",
        );
        return;
    };
    let group = &hooks["openviking"];
    if group["enabled"] == false {
        f.fail(
            "hooks.json switches the plugin's hooks off",
            "openviking.enabled = false",
            "set it to true",
        );
    }
    let missing: Vec<&str> = HOOK_EVENTS
        .into_iter()
        .filter(|event| !group[*event].to_string().contains("ov-agy"))
        .collect();
    if missing.is_empty() {
        f.ok(format!("hooks registered: {}", HOOK_EVENTS.join(", ")));
    } else {
        f.warn(
            format!("hooks missing: {}", missing.join(", ")),
            "",
            "reinstall the plugin so hooks.json matches this version",
        );
    }
}

fn check_launcher(f: &mut Findings, root: &Path) {
    let launcher = root.join("scripts/ov-agy");
    let Ok(meta) = fs::metadata(&launcher) else {
        f.fail("scripts/ov-agy is missing", "", "reinstall the plugin");
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            f.warn(
                "scripts/ov-agy is not executable",
                "the hooks start it through sh, but running it by hand fails",
                &format!("chmod +x {}", launcher.display()),
            );
            return;
        }
    }
    let _ = meta;
    f.ok("launcher scripts/ov-agy present");
}

fn check_mcp_config(f: &mut Findings, root: &Path) {
    let server = config::read_json_optional::<Value>(&root.join("mcp_config.json"))
        .map_or(Value::Null, |cfg| cfg["mcpServers"]["openviking"].clone());
    if server["args"].to_string().contains("mcp-proxy") {
        f.ok("MCP server `openviking` registered (scripts/ov-agy mcp-proxy)");
    } else {
        f.warn(
            "mcp_config.json does not register the `openviking` MCP server",
            "",
            "reinstall the plugin",
        );
    }
}

// ── Configuration ──────────────────────────────────────────────────────────

fn check_config(f: &mut Findings, cfg: &ResolvedConfig, args: &DoctorArgs) {
    f.section("Configuration");
    if cfg.enabled {
        f.ok(format!("plugin enabled ({})", cfg.enabled_reason));
    } else {
        f.fail(
            format!(
                "plugin disabled; every hook exits at once ({})",
                cfg.enabled_reason
            ),
            "",
            "run `ov-agy setup`, or set OPENVIKING_MEMORY_ENABLED=1 with OPENVIKING_URL and OPENVIKING_API_KEY",
        );
    }
    let shown = |path: Option<&PathBuf>| {
        path.map_or_else(|| "not found".to_string(), |p| p.display().to_string())
    };
    let mut files = vec![
        format!("ovcli.conf: {}", shown(cfg.cli_config_path.as_ref())),
        format!("ov.conf: {}", shown(cfg.ov_config_path.as_ref())),
    ];
    files.extend(
        cfg.workspace_config_paths
            .iter()
            .map(|p| format!("workspace: {}", p.display())),
    );
    f.info(
        format!("credentials from {}", cfg.credential_source),
        &files.join("\n"),
    );
    for warning in &cfg.warnings {
        f.warn(warning.clone(), "", "");
    }
    for (level, message, fix) in lint_base_url(&cfg.base_url) {
        f.push(level, message, "", fix);
    }
    if cfg.mcp_url != format!("{}/mcp", cfg.base_url.trim_end_matches('/')) {
        f.info(format!("MCP url overridden: {}", cfg.mcp_url), "");
    }

    let key = describe_api_key(&cfg.api_key);
    if cfg.api_key.is_empty() {
        if cfg.auth_mode == "trusted" {
            f.info(
                "no api key: trusted mode sends account and user headers instead",
                "",
            );
        } else {
            f.warn(
                "no api key configured",
                "servers in api_key mode reject every request",
                "run `ov-agy setup`, or set api_key in ~/.openviking/ovcli.conf",
            );
        }
    } else {
        f.ok(format!("api key {}", key.display));
        for problem in &key.problems {
            f.warn(
                format!("api key {problem}"),
                "",
                "store only the key itself in ovcli.conf or OPENVIKING_API_KEY",
            );
        }
    }
    if let Some(ref warning) = cfg.root_key_warning {
        f.fail(
            "the configured key is the server's root key",
            warning,
            "create a user key with `ov admin` and switch to it with `ov config`",
        );
    }
    if cfg.auth_mode == "trusted" && (cfg.account.is_empty() || cfg.user.is_empty()) {
        f.warn(
            "authMode is trusted but account or user is empty",
            "a trusted-mode server rejects requests without X-OpenViking-Account and -User",
            "set account and user in ovcli.conf, or OPENVIKING_ACCOUNT and OPENVIKING_USER",
        );
    }

    let peer = if cfg.peer_id.is_empty() {
        "none"
    } else {
        cfg.peer_id.as_str()
    };
    f.info(format!("peer {peer} (source={})", cfg.peer_origin), "");
    f.info(toggles(cfg), "");
    if cfg.bypass_session {
        f.warn(
            "OPENVIKING_BYPASS_SESSION is on: every hook skips the server",
            "",
            "unset it",
        );
    }
    if !cfg.bypass_session_patterns.is_empty() {
        let cwd = args
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| p.display().to_string());
        let patterns = cfg.bypass_session_patterns.join(", ");
        if cfg.is_bypassed(None, cwd.as_deref()) {
            f.warn(
                format!("bypass patterns {patterns} match this directory"),
                "recall and capture are skipped here",
                "narrow bypass.session_patterns or OPENVIKING_BYPASS_SESSION_PATTERNS",
            );
        } else {
            f.info(format!("bypass patterns: {patterns}"), "");
        }
    }
    sweep_env(f, cfg);
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "OFF"
    }
}

fn toggles(cfg: &ResolvedConfig) -> String {
    format!(
        "recall {} (limit {}, {} ms, query_expansion={}, peer scope {}) · capture {} (tool calls {}, commit at {} tokens) · usage {}/{} · compaction re-inject, commit {} · idle sweep {}",
        on_off(cfg.auto_recall),
        cfg.recall_limit,
        cfg.recall_timeout_ms,
        cfg.effective_query_expansion().unwrap_or("server default"),
        cfg.recall_peer_scope,
        on_off(cfg.auto_capture),
        on_off(cfg.capture_tool_calls),
        cfg.commit_token_threshold,
        cfg.usage_view,
        cfg.usage_output,
        on_off(cfg.auto_commit_on_compact),
        on_off(cfg.idle_sweep),
    )
}

/// Upstream's base URL lint: the mistakes that turn every request into a 404 or a parse error.
pub(crate) fn lint_base_url(url: &str) -> Vec<(DoctorLevel, String, &'static str)> {
    let mut problems = Vec::new();
    let value = url.trim();
    if value.is_empty() {
        problems.push((
            DoctorLevel::Fail,
            "base URL is empty".to_string(),
            "set url in ovcli.conf or OPENVIKING_URL",
        ));
        return problems;
    }
    if value != url || value.chars().any(char::is_whitespace) {
        problems.push((
            DoctorLevel::Fail,
            "base URL contains whitespace".to_string(),
            "remove the spaces or line breaks around the url",
        ));
    }
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        problems.push((
            DoctorLevel::Fail,
            format!("base URL {value} has no http:// or https:// scheme"),
            "prefix the url with http:// or https://",
        ));
        return problems;
    }
    let Ok(parsed) = url::Url::parse(value) else {
        problems.push((
            DoctorLevel::Fail,
            format!("base URL {value} does not parse"),
            "use the server root, e.g. http://127.0.0.1:1933",
        ));
        return problems;
    };
    let path = parsed.path().trim_end_matches('/').to_ascii_lowercase();
    if path.ends_with("/api") || path.ends_with("/api/v1") {
        problems.push((
            DoctorLevel::Fail,
            format!("base URL ends with {path}; every request becomes {path}/api/v1/… and 404s"),
            "drop the /api/v1 suffix; the plugin appends API paths itself",
        ));
    }
    if path.ends_with("/mcp") {
        problems.push((
            DoctorLevel::Fail,
            "base URL ends with /mcp; the plugin appends /mcp itself".to_string(),
            "use the server root URL, not the MCP endpoint",
        ));
    }
    if parsed.host_str() == Some("0.0.0.0") {
        problems.push((
            DoctorLevel::Warn,
            "host 0.0.0.0 is a bind address, not a destination".to_string(),
            "use 127.0.0.1 or the real hostname",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        problems.push((
            DoctorLevel::Warn,
            "base URL carries a query string or fragment".to_string(),
            "remove everything after the path",
        ));
    }
    problems
}

struct KeyInfo {
    display: String,
    problems: Vec<String>,
}

/// Masked key plus the account and user a v2 key (`account.user.secret`) names.
fn describe_api_key(raw: &str) -> KeyInfo {
    let mut problems = Vec::new();
    if raw != raw.trim() {
        problems.push("has leading or trailing whitespace".to_string());
    }
    if raw.contains(['\r', '\n']) {
        problems.push("contains a line break".to_string());
    }
    let trimmed = raw.trim();
    if trimmed.starts_with(['"', '\'']) || trimmed.ends_with(['"', '\'']) {
        problems.push("is wrapped in quotes".to_string());
    }
    if trimmed
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("bearer "))
    {
        problems.push("starts with 'Bearer '".to_string());
    }
    let masked = config::mask_secret(raw);
    let parts: Vec<&str> = trimmed.split('.').collect();
    let display = if let [account, user, secret] = parts.as_slice() {
        match (base64url_text(account), base64url_text(user)) {
            (Some(account), Some(user)) => {
                if secret.is_empty() {
                    problems.push("has an empty secret segment".to_string());
                }
                format!("{masked}, names account={account} user={user}")
            }
            _ => masked,
        }
    } else {
        masked
    };
    KeyInfo { display, problems }
}

/// Decodes unpadded base64url into UTF-8 text, for the readable segments of a v2 key.
fn base64url_text(segment: &str) -> Option<String> {
    let mut bits = 0_u32;
    let mut width = 0;
    let mut out = Vec::new();
    for byte in segment.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        width += 6;
        if width >= 8 {
            width -= 8;
            out.push(u8::try_from((bits >> width) & 0xff).ok()?);
        }
    }
    let text = String::from_utf8(out).ok()?;
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

/// `OPENVIKING_*`, proxy and CA variables that change what the hooks reach.
fn sweep_env(f: &mut Findings, cfg: &ResolvedConfig) {
    let mut ov: Vec<String> = std::env::vars()
        .filter(|(name, _)| name.starts_with("OPENVIKING_"))
        .map(|(name, value)| {
            let secret = ["KEY", "TOKEN", "SECRET", "PASSWORD", "HEADERS"]
                .iter()
                .any(|marker| name.contains(marker));
            let shown = if secret {
                config::mask_secret(&value)
            } else {
                value
            };
            format!("{name}={shown}")
        })
        .collect();
    ov.sort();
    if ov.is_empty() {
        f.info("no OPENVIKING_* variables in this environment", "");
    } else {
        f.info("OPENVIKING_* in this environment", &ov.join("\n"));
    }

    let host = url::Url::parse(cfg.base_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let proxies: Vec<&str> = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .filter(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()))
    .collect();
    if !proxies.is_empty() {
        let no_proxy = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .unwrap_or_default();
        let names = proxies.join(", ");
        if no_proxy_matches(&no_proxy, &host) {
            f.info(
                format!("proxy variables set ({names}); NO_PROXY covers {host}"),
                "",
            );
        } else {
            let fix = if is_loopback(&host) {
                "add 127.0.0.1,localhost to NO_PROXY in the environment that starts agy"
            } else {
                "add the server host to NO_PROXY, or make sure the proxy reaches it"
            };
            f.warn(
                format!("proxy variables set ({names}): requests to {host} go through the proxy"),
                "ov-agy honours HTTP(S)_PROXY and ALL_PROXY; a proxy that cannot reach the server makes every hook fail open",
                fix,
            );
        }
    }
    check_tls(f, cfg);
    if cfg.debug {
        f.info(
            format!("debug log on: {}", cfg.debug_log_path.display()),
            "",
        );
    } else {
        f.info(
            "debug log off",
            "set OPENVIKING_DEBUG=1 in the environment that starts agy to record hook errors",
        );
    }
}

/// The roots TLS is checked against: bundled Mozilla roots, the system store (or what
/// `SSL_CERT_FILE` / `SSL_CERT_DIR` name instead), and the extra CA bundle.
fn check_tls(f: &mut Findings, cfg: &ResolvedConfig) {
    let extra = crate::client::extra_root_certificates(cfg);
    if !cfg.uses_https() {
        if cfg.ca_certs.is_some() {
            f.info(
                "plain HTTP: the extra CA bundle is not used",
                &cfg.ca_certs
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
        }
        return;
    }
    let replaced = ["SSL_CERT_FILE", "SSL_CERT_DIR"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        .collect::<Vec<_>>();
    let store = if replaced.is_empty() {
        "the system store".to_string()
    } else {
        replaced.join(" and ")
    };
    let native = rustls_native_certs::load_native_certs();
    let errors = native
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut roots = format!(
        "TLS roots: bundled Mozilla roots + {} from {store}",
        native.certs.len()
    );
    if let Ok(ref certs) = extra {
        if !certs.is_empty() {
            let source = cfg
                .ca_certs
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let _ = write!(roots, " + {} from {source}", certs.len());
        }
    }
    if native.certs.is_empty() && !replaced.is_empty() {
        f.warn(
            format!("{roots}: {store} yielded no certificate"),
            &errors,
            "point SSL_CERT_FILE at a PEM bundle (or SSL_CERT_DIR at a directory of them), or unset it to use the system store",
        );
    } else {
        f.ok(roots);
    }
    if let Err(err) = extra {
        f.fail(
            "the extra CA bundle cannot be used",
            &err,
            "point OPENVIKING_CA_CERTS (or caCerts in ovcli.conf) at a PEM file holding the CA certificate(s)",
        );
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "::1" | "[::1]")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// `NO_PROXY` as reqwest reads it: `*`, host suffixes (with or without a leading dot), exact
/// IPs and IPv4 CIDR ranges.
fn no_proxy_matches(no_proxy: &str, host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    no_proxy
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Some((net, bits)) = entry.split_once('/') {
                let (Ok(net), Ok(bits), Ok(ip)) = (
                    net.parse::<Ipv4Addr>(),
                    bits.parse::<u32>(),
                    host.parse::<Ipv4Addr>(),
                ) else {
                    return false;
                };
                let mask = u32::MAX.checked_shl(32 - bits.min(32)).unwrap_or(0);
                return u32::from(net) & mask == u32::from(ip) & mask;
            }
            let bare = entry.trim_start_matches("*.").trim_start_matches('.');
            host == bare || host.ends_with(&format!(".{bare}"))
        })
}

// ── Server ─────────────────────────────────────────────────────────────────

struct Probe {
    status: Option<u16>,
    json: Option<Value>,
    text: String,
    latency_ms: u64,
    error: Option<String>,
    error_kind: &'static str,
    hint: &'static str,
    session_id: Option<String>,
    location: Option<String>,
}

impl Probe {
    fn ok(&self) -> bool {
        self.status.is_some_and(|s| (200..300).contains(&s))
    }

    /// The server's error message, from the `OpenViking` envelope or a `FastAPI` detail.
    fn server_message(&self) -> String {
        if let Some(json) = &self.json {
            for candidate in [
                &json["error"]["message"],
                &json["detail"],
                &json["error"],
                &json["message"],
            ] {
                if let Some(text) = candidate.as_str() {
                    return text.to_string();
                }
            }
        }
        let flat = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        flat.chars().take(200).collect()
    }
}

async fn probe(
    http: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Option<&Value>,
    timeout: Duration,
) -> Probe {
    let start = Instant::now();
    let mut request = http.request(method, url).headers(headers).timeout(timeout);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await;
    let elapsed = || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    match response {
        Err(err) => {
            let (error_kind, hint) = classify_error(&err);
            Probe {
                status: None,
                json: None,
                text: String::new(),
                latency_ms: elapsed(),
                error: Some(error_chain(&err)),
                error_kind,
                hint,
                session_id: None,
                location: None,
            }
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let header = |name: &str| {
                resp.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let session_id = header("mcp-session-id");
            let location = header(LOCATION.as_str());
            let sse = header(CONTENT_TYPE.as_str())
                .is_some_and(|content_type| content_type.contains("text/event-stream"));
            let text = resp.text().await.unwrap_or_default();
            let json = if sse {
                sse_json(&text)
            } else {
                serde_json::from_str(&text).ok()
            };
            Probe {
                status: Some(status),
                json,
                text,
                latency_ms: elapsed(),
                error: None,
                error_kind: "",
                hint: "",
                session_id,
                location,
            }
        }
    }
}

fn sse_json(text: &str) -> Option<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|frame| frame.get("result").is_some() || frame.get("error").is_some())
}

fn error_chain(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !parts.iter().any(|seen| seen.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

fn classify_error(err: &reqwest::Error) -> (&'static str, &'static str) {
    let chain = error_chain(err).to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| chain.contains(needle));
    if err.is_timeout() {
        (
            "timeout",
            "no response within the timeout: wrong host or port, a firewall, or a very slow server",
        )
    } else if has(&["connection refused"]) {
        (
            "refused",
            "nothing listens on that host and port: the server is down or the port is wrong",
        )
    } else if has(&[
        "dns error",
        "failed to lookup address",
        "nodename nor servname",
    ]) {
        (
            "dns",
            "the hostname does not resolve: a typo in the url, or DNS or the VPN is unavailable",
        )
    } else if has(&["certificate", "unknownissuer", "invalid peer certificate"]) {
        (
            "tls",
            "TLS verification failed: the server's certificate does not chain to a trusted root (the bundled Mozilla roots, the system store, or OPENVIKING_CA_CERTS)",
        )
    } else if has(&[
        "corrupt message",
        "wrong version number",
        "invalid http version",
    ]) {
        (
            "tls",
            "protocol mismatch: https:// against a plain-HTTP port, or the reverse",
        )
    } else if has(&["connection reset", "broken pipe"]) {
        (
            "reset",
            "the connection was reset: a proxy or TLS terminator closed it",
        )
    } else if err.is_builder() {
        ("url", "the url does not parse; check the scheme and host")
    } else {
        ("other", "")
    }
}

async fn check_server(
    f: &mut Findings,
    cfg: &ResolvedConfig,
    timeout: Duration,
    facts: &mut ServerFacts,
) {
    f.section("Server");
    let http = match crate::client::http_client_builder(cfg)
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .build()
    {
        Ok(http) => http,
        Err(err) => {
            f.fail("cannot build the HTTP client", &err.to_string(), "");
            return;
        }
    };
    let base = cfg.base_url.trim().trim_end_matches('/').to_string();
    let auth = build_ov_headers(cfg, ActorPeer::Always);

    let health = probe(
        &http,
        Method::GET,
        &format!("{base}/health"),
        auth.clone(),
        None,
        timeout,
    )
    .await;
    if !assess_health(f, cfg, &http, &base, &health, timeout, facts).await {
        return;
    }

    let ready = probe(
        &http,
        Method::GET,
        &format!("{base}/ready"),
        HeaderMap::new(),
        None,
        timeout.max(Duration::from_secs(10)),
    )
    .await;
    facts.ready = assess_ready(f, &ready);

    let status = probe(
        &http,
        Method::GET,
        &format!("{base}/api/v1/system/status"),
        auth.clone(),
        None,
        timeout,
    )
    .await;
    match status.status {
        Some(200) => {
            let user = status
                .json
                .as_ref()
                .and_then(|j| j["result"]["user"].as_str());
            f.ok(format!(
                "GET /api/v1/system/status: 200{}",
                user.map_or_else(String::new, |u| format!(" (user space {u})"))
            ));
        }
        Some(code @ (401 | 403)) => f.fail(
            format!("GET /api/v1/system/status: {code}"),
            &status.server_message(),
            if code == 401 {
                "the server rejects the credentials; every hook hits this after /health passes"
            } else {
                "the key is valid but not allowed here: a root key, or the wrong role"
            },
        ),
        Some(code) => f.warn(
            format!("GET /api/v1/system/status: {code}"),
            &status.server_message(),
            "",
        ),
        None => f.fail(
            "GET /api/v1/system/status failed",
            status.error.as_deref().unwrap_or(""),
            "",
        ),
    }

    let ls = probe(
        &http,
        Method::GET,
        &format!("{base}/api/v1/fs/ls?uri=viking%3A%2F%2F~%2Fmemories&node_limit=1"),
        auth.clone(),
        None,
        timeout,
    )
    .await;
    match ls.status {
        Some(200) => f.ok("GET /api/v1/fs/ls viking://~/memories: 200"),
        Some(403) => {
            let msg = ls.server_message();
            if msg.to_ascii_uppercase().contains("ROOT") {
                f.fail(
                    "the tenant data API rejects this key as a root key",
                    &msg,
                    "use a user or admin key, not server.root_api_key",
                );
            } else {
                f.fail(
                    "GET /api/v1/fs/ls viking://~/memories: 403",
                    &msg,
                    "the key may not read this user's memories",
                );
            }
        }
        Some(404) => f.info(
            "viking://~/memories does not exist yet (nothing captured for this user so far)",
            "",
        ),
        Some(400) => f.warn(
            "GET /api/v1/fs/ls viking://~/memories: 400",
            &ls.server_message(),
            "trusted mode needs account and user; otherwise the server may predate the viking://~ alias",
        ),
        Some(401) | None => {}
        Some(code) => f.warn(
            format!("GET /api/v1/fs/ls viking://~/memories: {code}"),
            &ls.server_message(),
            "",
        ),
    }

    check_mcp(f, cfg, &http, timeout, facts).await;
}

/// `false` when the server cannot be used, so the rest of the ladder would fail the same way.
async fn assess_health(
    f: &mut Findings,
    cfg: &ResolvedConfig,
    http: &Client,
    base: &str,
    health: &Probe,
    timeout: Duration,
    facts: &mut ServerFacts,
) -> bool {
    if let Some(ref error) = health.error {
        facts.health_ok = Some(false);
        let fix = if health.error_kind == "tls" && health.hint.starts_with("TLS verification") {
            "add the server's CA to the system trust store, or set OPENVIKING_CA_CERTS=/path/to/ca.pem"
        } else {
            "check that the server runs and that the url and port are right; compare with `curl -sS <url>/health`"
        };
        f.fail(
            format!("server unreachable: {base}"),
            &format!("{}: {error}\n{}", health.error_kind, health.hint),
            fix,
        );
        return false;
    }
    let status = health.status.unwrap_or(0);
    facts.latency_ms = Some(u128::from(health.latency_ms));
    if status == 401 || status == 403 {
        facts.health_ok = Some(false);
        let anonymous = probe(
            http,
            Method::GET,
            &format!("{base}/health"),
            HeaderMap::new(),
            None,
            timeout,
        )
        .await;
        if anonymous.ok() {
            f.fail(
                format!("the server rejects this api key ({status} on /health)"),
                &format!("{} (key {})", health.server_message(), cfg.mask_api_key()),
                "check api_key in ~/.openviking/ovcli.conf or OPENVIKING_API_KEY against the key issued for this server",
            );
        } else {
            f.fail(
                format!("GET /health: {status} with and without the key"),
                &health.server_message(),
                "a gateway in front of OpenViking wants its own credentials: send them with OPENVIKING_EXTRA_HEADERS, or use the server's direct url",
            );
        }
        return false;
    }
    if status == 404 {
        facts.health_ok = Some(false);
        f.fail(
            format!("GET /health: 404 at {base}"),
            "the url reaches a web server, but not an OpenViking API root",
            "check for a missing path prefix, or a reverse proxy that does not forward /health",
        );
        return false;
    }
    if (300..400).contains(&status) {
        facts.health_ok = Some(false);
        f.fail(
            format!("GET /health: {status} redirect"),
            &format!("location: {}", health.location.as_deref().unwrap_or("?")),
            "the url is redirected (http to https, or an SSO gateway); use the final url",
        );
        return false;
    }
    let Some(body) = health.json.as_ref().filter(|_| health.ok()) else {
        facts.health_ok = Some(false);
        f.fail(
            format!("GET /health: {status}"),
            &health.server_message(),
            "the server answered, but not like OpenViking: make sure the url reaches the OpenViking API",
        );
        return false;
    };
    let text = |key: &str| body[key].as_str().map(str::to_string);
    facts.health_ok = Some(body["healthy"].as_bool().unwrap_or(true));
    facts.version = text("version");
    facts.auth_mode = text("auth_mode");
    let auth_mode = facts.auth_mode.clone().unwrap_or_default();
    f.ok(format!(
        "server reachable: {} ms, version {}, auth_mode={}",
        health.latency_ms,
        facts.version.as_deref().unwrap_or("?"),
        if auth_mode.is_empty() {
            "?"
        } else {
            &auth_mode
        }
    ));
    if health.latency_ms > 1000 {
        f.warn(
            format!("/health took {} ms", health.latency_ms),
            "SessionStart has 4 s for all of its reads, and recall has OPENVIKING_RECALL_TIMEOUT_MS",
            "check the network path to the server",
        );
    }

    if let Some(role) = text("role") {
        let account = text("account_id").unwrap_or_default();
        let user = text("user_id").unwrap_or_default();
        f.ok(format!(
            "credentials accepted: account={account} user={user} role={role}"
        ));
        if role == "root" {
            let warning = "The configured key is the ROOT api key. OpenViking rejects it on `/mcp` and on tenant data APIs; configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string();
            f.fail(
                "this is the ROOT api key",
                "root keys are rejected on /mcp and on tenant data APIs (memories, sessions, search)",
                "create a user key with the root key, then switch to it with `ov config`",
            );
            facts.root_key_warning.get_or_insert(warning);
        }
        if auth_mode == "api_key" {
            let differs = (!cfg.account.is_empty() && cfg.account != account)
                || (!cfg.user.is_empty() && cfg.user != user);
            if differs {
                f.warn(
                    format!(
                        "configured account/user ({}/{}) differ from the key's identity ({account}/{user})",
                        if cfg.account.is_empty() { "-" } else { &cfg.account },
                        if cfg.user.is_empty() { "-" } else { &cfg.user }
                    ),
                    "in api_key mode the key decides where data lands; the account and user settings are ignored",
                    "remove account and user from the configuration, or use a key issued for them",
                );
            }
        }
        facts.account = Some(account);
        facts.user = Some(user);
        facts.role = Some(role);
    } else if auth_mode == "dev" {
        f.info(
            "the server runs in dev auth mode: any key is accepted",
            "identity comes from X-OpenViking-Account and -User (default `default`)",
        );
    } else if auth_mode == "trusted" {
        if cfg.account.is_empty() || cfg.user.is_empty() {
            f.fail(
                "the server is in trusted mode and needs account and user",
                "requests without X-OpenViking-Account and -User are rejected with 400",
                "set account and user in ovcli.conf, or OPENVIKING_ACCOUNT and OPENVIKING_USER",
            );
        } else {
            f.ok(format!(
                "trusted mode: identity from headers account={} user={}",
                cfg.account, cfg.user
            ));
        }
    } else if cfg.api_key.is_empty() && auth_mode == "api_key" {
        f.fail(
            "the server requires an api key (auth_mode=api_key) and none is configured",
            "",
            "run `ov-agy setup`, or set api_key in ~/.openviking/ovcli.conf",
        );
    } else if !cfg.api_key.is_empty() {
        f.info(
            "the server did not report the key's identity",
            "older servers do not; the system/status check below still validates the key",
        );
    }
    true
}

/// `/ready` checks storage, the vector index and the embedder; `None` when unknown.
fn assess_ready(f: &mut Findings, probe: &Probe) -> Option<bool> {
    if let Some(ref error) = probe.error {
        if probe.error_kind == "timeout" {
            f.warn(
                format!("GET /ready timed out after {} ms", probe.latency_ms),
                "a hanging embedding provider is the usual cause",
                "run `openviking-server doctor` where the server runs",
            );
        } else {
            f.info(format!("GET /ready failed: {error}"), "");
        }
        return None;
    }
    let status = probe.status.unwrap_or(0);
    if status == 404 {
        f.info(
            "the server has no /ready endpoint (older version)",
            "`openviking-server doctor` covers storage and the embedder there",
        );
        return None;
    }
    let Some(json) = probe.json.as_ref() else {
        f.info(format!("GET /ready: {status} without a JSON body"), "");
        return None;
    };
    if json["reason"] == "initializing" {
        f.warn(
            "the server is still initializing (503 on /ready)",
            "storage and the embedder are being set up; a local embedding model downloads on first start",
            "wait and run the doctor again; if it never finishes, read the server log",
        );
        return Some(false);
    }
    let Some(checks) = json["checks"].as_object() else {
        f.info(format!("GET /ready: {status}"), &probe.server_message());
        return None;
    };
    let healthy = |value: &Value| match value {
        Value::String(s) => matches!(s.as_str(), "ok" | "not_configured" | "healthy"),
        Value::Object(obj) => obj
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "ok" | "healthy")),
        _ => false,
    };
    let summary: Vec<String> = checks
        .iter()
        .map(|(name, value)| {
            let text = value
                .as_str()
                .map_or_else(|| value["status"].to_string(), str::to_string);
            format!("{name} {}", text.trim_matches('"'))
        })
        .collect();
    let failed: Vec<&String> = checks
        .iter()
        .filter(|(_, value)| !healthy(value))
        .map(|(name, _)| name)
        .collect();
    if failed.is_empty() && probe.ok() {
        f.ok(format!("/ready: {}", summary.join(", ")));
        Some(true)
    } else {
        f.fail(
            format!(
                "/ready: {} not ready",
                failed
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            &summary.join("\n"),
            "read the server log; `openviking-server doctor` shows the provider's reply",
        );
        Some(false)
    }
}

async fn check_mcp(
    f: &mut Findings,
    cfg: &ResolvedConfig,
    http: &Client,
    timeout: Duration,
    facts: &mut ServerFacts,
) {
    let mut headers = build_ov_headers(cfg, ActorPeer::ActorScopeOnly);
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        HeaderName::from_static("mcp-protocol-version"),
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    let mcp_timeout = timeout.max(Duration::from_secs(8));
    let init = probe(
        http,
        Method::POST,
        &cfg.mcp_url,
        headers.clone(),
        Some(&json!({
            "jsonrpc": "2.0",
            "id": "ov-agy-doctor-init",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ov-agy-doctor", "version": env!("CARGO_PKG_VERSION")}
            }
        })),
        mcp_timeout,
    )
    .await;
    facts.mcp_status = init.status;
    facts.mcp_session = Some(init.session_id.is_some());
    facts.mcp_ok = Some(init.ok());
    if !init.ok() {
        mcp_failure(f, &init, "initialize", facts);
        return;
    }
    if let Some(ref sid) = init.session_id {
        if let Ok(value) = HeaderValue::from_str(sid) {
            headers.insert(HeaderName::from_static("mcp-session-id"), value);
        }
        let _ = probe(
            http,
            Method::POST,
            &cfg.mcp_url,
            headers.clone(),
            Some(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            mcp_timeout,
        )
        .await;
    }
    let list = probe(
        http,
        Method::POST,
        &cfg.mcp_url,
        headers.clone(),
        Some(&json!({"jsonrpc": "2.0", "id": "ov-agy-doctor-tools", "method": "tools/list", "params": {}})),
        mcp_timeout,
    )
    .await;
    if init.session_id.is_some() {
        let _ = http
            .delete(&cfg.mcp_url)
            .headers(headers)
            .timeout(Duration::from_secs(1))
            .send()
            .await;
    }
    let tools: Option<Vec<String>> = list
        .json
        .as_ref()
        .and_then(|j| j["result"]["tools"].as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        });
    let Some(tools) = tools.filter(|_| list.ok()) else {
        facts.mcp_ok = Some(false);
        mcp_failure(f, &list, "tools/list", facts);
        return;
    };
    facts.mcp_tools = Some(tools.len());
    f.ok(format!("POST /mcp tools/list: {} tools", tools.len()));
    let missing: Vec<&str> = CORE_MCP_TOOLS
        .into_iter()
        .filter(|name| !tools.iter().any(|t| t == name))
        .collect();
    if !missing.is_empty() {
        f.warn(
            format!("MCP tools missing: {}", missing.join(", ")),
            "the plugin's skills and rules tell the model to use them",
            "upgrade the OpenViking server",
        );
    }
}

fn mcp_failure(f: &mut Findings, probe: &Probe, step: &str, facts: &mut ServerFacts) {
    if let Some(ref error) = probe.error {
        f.fail(
            format!("POST /mcp {step} failed"),
            &format!("{}: {error}", probe.error_kind),
            probe.hint,
        );
        return;
    }
    let status = probe.status.unwrap_or(0);
    let hint = match status {
        401 => "the credentials are rejected on /mcp",
        403 => "the key is valid but may not use /mcp: a root key in api_key mode, or a gateway rule",
        404 => "no MCP endpoint at <url>/mcp: a missing path prefix, an old server, or a reverse proxy that does not forward /mcp",
        406 => "the server refused the Accept header: something between client and server rewrites headers",
        502 => "the reverse proxy cannot reach the server",
        504 => "the reverse proxy timed out (SSE needs proxy_buffering off)",
        _ => "",
    };
    if status == 403 && facts.root_key_warning.is_none() {
        facts.root_key_warning = Some("POST /mcp returned 403 Forbidden. OpenViking rejects `server.root_api_key` on `/mcp` and tenant memory endpoints; configure a User/Admin Key via `ov config` in `~/.openviking/ovcli.conf`.".to_string());
    }
    f.fail(
        format!("POST /mcp {step}: {status}"),
        &probe.server_message(),
        hint,
    );
}

// ── Recent activity ────────────────────────────────────────────────────────

fn check_activity(f: &mut Findings, cfg: &ResolvedConfig, dirs: &[DataDirReport], pending: usize) {
    f.section("Recent activity");
    let inject = cfg.state_dir.join("last-inject.md");
    match fs::metadata(&inject) {
        Ok(meta) => f.info(
            format!(
                "last context injection {}, {} bytes ({})",
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or_else(
                        || "at an unknown time".to_string(),
                        |d| describe_age(u64::try_from(d.as_millis()).unwrap_or(0))
                    ),
                meta.len(),
                inject.display()
            ),
            "",
        ),
        Err(_) => f.info(
            "no context injection recorded yet (SessionStart has not run with this data directory)",
            "",
        ),
    }

    let active = dirs.iter().find(|dir| dir.active);
    match active.and_then(|dir| dir.last_recall.as_ref()) {
        Some(recall) => {
            let line = format!(
                "last recall {}: {} ({} ms, {} chars injected)",
                describe_age(recall.timestamp_ms),
                if recall.outcome.is_empty() {
                    "recorded"
                } else {
                    &recall.outcome
                },
                recall.latency_ms,
                recall.injected_chars
            );
            if recall.outcome == "failed" {
                f.warn(
                    line,
                    recall.error.as_deref().unwrap_or(""),
                    "check the server with the Server section above; a timeout means OPENVIKING_RECALL_TIMEOUT_MS is too short for this server",
                );
            } else {
                f.info(line, "");
            }
        }
        None => f.info("no recall recorded yet", ""),
    }
    match active.and_then(|dir| dir.last_capture.as_ref()) {
        Some(capture) => {
            let line = format!(
                "last capture {}: {} captured, {} queued, {} rejected, commit {}{} (session {})",
                describe_age(capture.timestamp_ms),
                capture.turns_captured,
                capture.turns_queued,
                capture.turns_failed,
                capture.commit,
                capture.pending_tokens.map_or_else(String::new, |p| format!(
                    " ({p}/{} tokens)",
                    capture.commit_threshold
                )),
                capture.ov_session_id
            );
            if capture.turns_failed > 0 || capture.commit == "failed" {
                f.warn(
                    line,
                    capture.error.as_deref().unwrap_or(""),
                    "the server rejected those turns for good (401/403/404/422); fix the credentials, the turns after that are captured again",
                );
            } else if capture.turns_queued > 0 {
                f.warn(
                    line,
                    capture.error.as_deref().unwrap_or(""),
                    "the turns wait in the offline queue and replay with the next capture or session start",
                );
            } else {
                f.info(line, "");
            }
        }
        None => f.info(
            "no capture recorded yet (the Stop hook has not finished a turn)",
            "",
        ),
    }

    if pending > 0 {
        f.warn(
            format!("{pending} operation(s) in this identity's offline queue"),
            "they replay once the server accepts writes again",
            "start a new agy session after the server is back; SessionStart replays the queue",
        );
    } else {
        f.ok("offline queue empty");
    }
    for dir in dirs {
        let queued: usize = dir.wal_scopes.iter().map(|q| q.entries).sum();
        f.info(
            format!(
                "{} {}: {} conversation(s), last activity {}, {queued} queued",
                if dir.active { "*" } else { "-" },
                dir.path,
                dir.conversations,
                dir.last_activity_ms
                    .map_or_else(|| "never".to_string(), describe_age),
            ),
            "",
        );
    }
    for warning in data_dir_warnings(dirs) {
        f.warn(warning, "", "");
    }
    scan_debug_log(f, cfg);
}

fn scan_debug_log(f: &mut Findings, cfg: &ResolvedConfig) {
    let path = &cfg.debug_log_path;
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    let mut tail = String::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_string(&mut tail).is_err() {
        return;
    }
    let errors: Vec<String> = tail
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            entry["stage"]
                .as_str()
                .is_some_and(|stage| stage.contains("error") || stage.contains("fatal"))
        })
        .map(|entry| {
            format!(
                "{} {}/{}: {}",
                entry["ts_ms"]
                    .as_u64()
                    .map_or_else(|| "?".to_string(), describe_age),
                entry["component"].as_str().unwrap_or("?"),
                entry["stage"].as_str().unwrap_or("?"),
                entry["detail"]["error"]
                    .as_str()
                    .map_or_else(|| entry["detail"].to_string(), str::to_string)
            )
        })
        .collect();
    let recent: Vec<&String> = errors.iter().rev().take(5).collect();
    if recent.is_empty() {
        f.info(
            format!(
                "debug log {} ({len} bytes): no recent errors",
                path.display()
            ),
            "",
        );
    } else {
        f.warn(
            format!(
                "debug log {}: {} error(s) recorded, the latest {} below",
                path.display(),
                errors.len(),
                recent.len()
            ),
            &recent
                .iter()
                .map(|line| line.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            "",
        );
    }
}

// ── Data directories (moved from `main.rs`) ────────────────────────────────

/// Plugin data directories worth reporting: the one this process uses, the default used
/// outside agy, and every agy product directory that holds this plugin's files.
pub fn candidate_data_dirs(cfg: &ResolvedConfig) -> Vec<(PathBuf, bool)> {
    let mut found = vec![
        (cfg.state_dir.clone(), true),
        (cfg.ov_home_dir.join("agy-plugin"), false),
    ];
    if let Some(home) = dirs::home_dir() {
        let products = fs::read_dir(home.join(".gemini")).into_iter().flatten();
        for product in products.filter_map(Result::ok) {
            let plugins = fs::read_dir(product.path().join("plugin_data"))
                .into_iter()
                .flatten();
            for plugin in plugins.filter_map(Result::ok) {
                let dir = plugin.path();
                if holds_ov_agy_data(&dir) {
                    found.push((dir, false));
                }
            }
        }
    }
    let mut seen = Vec::new();
    found.retain(|(dir, _)| {
        let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    found
}

fn holds_ov_agy_data(dir: &Path) -> bool {
    dir.join("wal").is_dir()
        || dir.join("last-recall.json").is_file()
        || dir.join("last-capture.json").is_file()
        || fs::read_dir(dir.join("bin"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("ov-agy-v"))
}

pub fn survey_data_dir(dir: &Path, active: bool, current_scope: &str) -> DataDirReport {
    let state_files: Vec<fs::Metadata> = fs::read_dir(dir.join("state"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| entry.metadata().ok())
        .collect();
    let last_activity_ms = state_files
        .iter()
        .filter_map(|meta| meta.modified().ok())
        .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX))
        .max();
    let (scopes, legacy) = wal::survey_queues(dir);
    DataDirReport {
        path: dir.display().to_string(),
        active,
        conversations: state_files.len(),
        last_activity_ms,
        wal_scopes: scopes
            .into_iter()
            .map(|queue| WalScopeReport {
                current_identity: queue.scope == current_scope,
                scope: queue.scope,
                entries: queue.stats.entry_count,
                bytes: queue.stats.byte_size,
                oldest_created_at_ms: queue.oldest_created_at_ms,
            })
            .collect(),
        legacy_wal_entries: legacy.entry_count,
        last_recall: config::read_json_optional::<LastRecallSnapshot>(
            &dir.join("last-recall.json"),
        ),
        last_capture: config::read_json_optional::<LastCaptureSnapshot>(
            &dir.join("last-capture.json"),
        ),
    }
}

fn data_dir_warnings(dirs: &[DataDirReport]) -> Vec<String> {
    let mut warnings = Vec::new();
    for dir in dirs {
        for queue in dir
            .wal_scopes
            .iter()
            .filter(|queue| !queue.current_identity)
        {
            warnings.push(format!(
                "{} queued turn(s) in {}/wal/{} belong to a different URL, account, key or peer; they replay only when agy runs with that identity.",
                queue.entries, dir.path, queue.scope
            ));
        }
        if dir.legacy_wal_entries > 0 {
            warnings.push(format!(
                "Unscoped legacy WAL with {} entr(ies) retained at {}/wal/pending.jsonl. Verify its original destination before manual recovery.",
                dir.legacy_wal_entries, dir.path
            ));
        }
    }
    let active_activity = dirs
        .iter()
        .find(|dir| dir.active)
        .and_then(|dir| dir.last_activity_ms)
        .unwrap_or(0);
    if let Some(busiest) = dirs
        .iter()
        .filter(|dir| !dir.active)
        .filter(|dir| dir.last_activity_ms.unwrap_or(0) > active_activity)
        .max_by_key(|dir| dir.last_activity_ms)
    {
        warnings.push(format!(
            "agy last wrote plugin state to {}, not to the directory this run uses. Run doctor with OPENVIKING_PLUGIN_DATA={} to inspect that queue as the hooks see it.",
            busiest.path, busiest.path
        ));
    }
    warnings
}

pub fn describe_age(timestamp_ms: u64) -> String {
    let secs = config::now_epoch_ms().saturating_sub(timestamp_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

fn mark(level: DoctorLevel) -> &'static str {
    match level {
        DoctorLevel::Ok => "✓",
        DoctorLevel::Info => "·",
        DoctorLevel::Warn => "!",
        DoctorLevel::Fail => "✗",
    }
}

fn render(report: &DoctorReport) -> String {
    let mut lines = vec![format!(
        "OpenViking for agy, doctor v{}",
        report.plugin_version
    )];
    for section in &report.sections {
        lines.push(String::new());
        lines.push(format!("== {} ==", section.title));
        for finding in &section.findings {
            lines.push(format!("  {} {}", mark(finding.level), finding.title));
            if let Some(ref detail) = finding.detail {
                lines.extend(detail.lines().map(|l| format!("      {l}")));
            }
            if let Some(ref fix) = finding.fix {
                if matches!(finding.level, DoctorLevel::Warn | DoctorLevel::Fail) {
                    lines.push(format!("      fix: {fix}"));
                }
            }
        }
    }
    let problems: Vec<&DoctorFinding> = report
        .sections
        .iter()
        .flat_map(|s| &s.findings)
        .filter(|f| matches!(f.level, DoctorLevel::Warn | DoctorLevel::Fail))
        .collect();
    lines.push(String::new());
    lines.push("== Summary ==".to_string());
    if problems.is_empty() {
        lines.push("  ✓ no problems found".to_string());
    } else {
        let fails = problems
            .iter()
            .filter(|p| p.level == DoctorLevel::Fail)
            .count();
        lines.push(format!(
            "  {fails} failure(s), {} warning(s)",
            problems.len() - fails
        ));
        let ordered = problems
            .iter()
            .filter(|p| p.level == DoctorLevel::Fail)
            .chain(problems.iter().filter(|p| p.level == DoctorLevel::Warn));
        for problem in ordered {
            lines.push(format!(
                "  {} {}{}",
                mark(problem.level),
                problem.title,
                problem
                    .fix
                    .as_deref()
                    .map_or_else(String::new, |fix| format!(" -> {fix}"))
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_lint_catches_api_and_mcp_suffixes() {
        let titles = |url: &str| {
            lint_base_url(url)
                .into_iter()
                .map(|(_, title, _)| title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles("http://127.0.0.1:1933"), Vec::<String>::new());
        assert!(titles("http://h/api/v1")[0].contains("/api/v1"));
        assert!(titles("https://h/mcp/")[0].contains("/mcp"));
        assert!(titles("127.0.0.1:1933")[0].contains("no http:// or https:// scheme"));
        assert!(titles(" http://h ")[0].contains("whitespace"));
    }

    #[test]
    fn api_key_description_never_shows_the_secret() {
        let account = "YWNtZQ"; // "acme"
        let user = "YWxpY2U"; // "alice"
        let key = format!("{account}.{user}.s3cr3t-value-that-is-long");
        let info = describe_api_key(&key);
        assert!(
            info.display.contains("account=acme user=alice"),
            "{}",
            info.display
        );
        assert!(!info.display.contains("s3cr3t-value"));
        assert_eq!(info.problems, Vec::<String>::new());
        let quoted = describe_api_key("\"Bearer abc\" ");
        assert_eq!(quoted.problems.len(), 2, "{:?}", quoted.problems);
    }

    #[test]
    fn no_proxy_matching_follows_suffixes_and_cidr() {
        assert!(no_proxy_matches("localhost,127.0.0.1", "127.0.0.1"));
        assert!(no_proxy_matches(".example.com", "ov.example.com"));
        assert!(no_proxy_matches("example.com", "ov.example.com"));
        assert!(no_proxy_matches("10.0.0.0/8", "10.1.2.3"));
        assert!(no_proxy_matches("*", "anything"));
        assert!(!no_proxy_matches("example.com", "notexample.com"));
        assert!(!no_proxy_matches("", "127.0.0.1"));
    }
}
