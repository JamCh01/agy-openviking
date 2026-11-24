//! Git repository and linked Git worktree `peer` identity derivation.

use std::fs;
use std::path::{Component, Path, PathBuf};

use sha2::Sha256;
use url::Url;

const MAX_PEER_ID_LENGTH: usize = 100;

/// Resolved Git repository and normalized `peer` identity for a workspace path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerResolution {
    pub peer_id: String,
    pub repo_root: PathBuf,
    pub common_git_dir: PathBuf,
    pub remote_normalized: Option<String>,
    pub is_worktree: bool,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
struct GitDirInfo {
    common_dir: PathBuf,
    is_worktree: bool,
}

fn short_hash(value: &str) -> String {
    use sha2::Digest;

    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..12].to_string()
}

/// Sanitizes an arbitrary identifier into a canonical `OpenViking` `peer` ID.
#[must_use]
pub fn sanitize_peer_id(raw_input: &str) -> String {
    let raw = raw_input.trim();
    if raw.is_empty() {
        return String::new();
    }

    let mut cleaned = String::with_capacity(raw.len());
    let mut last_was_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '@' | '-') {
            if ch == '-' {
                if !last_was_dash {
                    cleaned.push('-');
                    last_was_dash = true;
                }
            } else {
                cleaned.push(ch);
                last_was_dash = false;
            }
        } else if !last_was_dash {
            cleaned.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = cleaned.trim_matches(['-', '.']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return String::new();
    }

    // OpenViking accepts at most one `@` in an identifier segment.
    let mut normalized = if let Some(first_at) = trimmed.find('@') {
        let (head, tail) = trimmed.split_at(first_at + 1);
        let sanitized_tail = tail.replace('@', "-");
        format!("{head}{sanitized_tail}")
    } else {
        trimmed.to_string()
    };

    if normalized.starts_with("ext-") {
        normalized = format!("x-{normalized}");
    } else if normalized == "__self" {
        normalized = "self".to_string();
    }

    if normalized.len() > MAX_PEER_ID_LENGTH {
        let prefix_len = MAX_PEER_ID_LENGTH - 13;
        let mut cut = prefix_len;
        while cut > 0 && !normalized.is_char_boundary(cut) {
            cut -= 1;
        }
        let prefix = normalized[..cut].trim_end_matches(['-', '.']);
        let hash_suffix = short_hash(raw);
        normalized = format!("{prefix}-{hash_suffix}");
    }

    normalized
}

/// Normalizes a Git remote URL (`git@host:Org/Repo.git`, `ssh://git@host/Org/Repo`,
/// `https://token@host/Org/Repo.git/`) into `host/path` (lowercased, without credentials,
/// port, or `.git` suffix).
#[must_use]
pub fn normalize_git_remote(raw_url: &str) -> Option<String> {
    let raw = raw_url.trim();
    if raw.is_empty() {
        return None;
    }

    // Reject Windows local drive paths such as `C:\repo` or `C:/repo`.
    let bytes = raw.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return None;
    }

    let (host, path) = if raw.contains("://") {
        let parsed = Url::parse(raw).ok()?;
        if parsed.scheme().eq_ignore_ascii_case("file") {
            return None;
        }
        let parsed_host = parsed.host_str()?.to_string();
        let parsed_path = parsed.path().to_string();
        (parsed_host, parsed_path)
    } else {
        // SCP-like syntax: `[user@]host:path` where `path` does not begin with `/`.
        let without_user = match raw.find('@') {
            Some(at_idx) => &raw[at_idx + 1..],
            None => raw,
        };
        let colon_idx = without_user.find(':')?;
        let scp_host = &without_user[..colon_idx];
        let scp_path = &without_user[colon_idx + 1..];
        if scp_host.is_empty()
            || scp_host.contains('/')
            || scp_host.contains('\\')
            || scp_path.is_empty()
            || scp_path.starts_with('/')
        {
            return None;
        }
        (scp_host.to_string(), scp_path.to_string())
    };

    let clean_host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    let mut clean_path = path.trim_matches('/').to_ascii_lowercase();
    if let Some(stripped) = clean_path.strip_suffix(".git") {
        clean_path = stripped.trim_end_matches('/').to_string();
    }

    if clean_host.is_empty() || clean_path.is_empty() {
        return None;
    }

    Some(format!("{clean_host}/{clean_path}"))
}

/// Reads `[remote "<remote_name>"]` `url = ...` from `<common_dir>/config`.
#[must_use]
pub fn read_git_remote_url(common_dir: &Path, remote_name: &str) -> Option<String> {
    let config_path = common_dir.join("config");
    let text = fs::read_to_string(config_path).ok()?;

    let mut in_target_remote = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(end_bracket) = rest.find(']') {
                let header = rest[..end_bracket].trim();
                in_target_remote =
                    parse_remote_section_name(header).is_some_and(|name| name == remote_name);
                continue;
            }
        }

        if !in_target_remote {
            continue;
        }

        if let Some((key, value_part)) = trimmed.split_once('=') {
            if key.trim().eq_ignore_ascii_case("url") {
                let uncommented = strip_git_inline_comment(value_part.trim());
                let unquoted = uncommented
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim();
                if !unquoted.is_empty() {
                    return Some(unquoted.to_string());
                }
            }
        }
    }

    None
}

fn parse_remote_section_name(header: &str) -> Option<&str> {
    let first_quote = header.find('"')?;
    let prefix = header[..first_quote].trim();
    if !prefix.eq_ignore_ascii_case("remote") {
        return None;
    }
    let after = &header[first_quote + 1..];
    let end_quote = after.rfind('"')?;
    Some(&after[..end_quote])
}

fn strip_git_inline_comment(value: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in value.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' | ';' if !in_quotes => return &value[..idx],
            _ => {}
        }
    }
    value
}

fn resolve_relative_or_abs(base: &Path, raw_target: &str) -> PathBuf {
    let candidate = Path::new(raw_target.trim());
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    fs::canonicalize(&joined).unwrap_or_else(|_| lexical_clean(&joined))
}

fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn inspect_dot_git(dir: &Path) -> Option<GitDirInfo> {
    let dot_git = dir.join(".git");
    let meta = fs::metadata(&dot_git).ok()?;

    if meta.is_dir() {
        let canonical = fs::canonicalize(&dot_git).unwrap_or(dot_git);
        return Some(GitDirInfo {
            common_dir: canonical,
            is_worktree: false,
        });
    }

    if !meta.is_file() {
        return None;
    }

    let content = fs::read_to_string(&dot_git).ok()?;
    let gitdir_raw = content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })?;

    let git_dir = resolve_relative_or_abs(dir, gitdir_raw);
    let commondir_file = git_dir.join("commondir");
    if let Ok(common_raw) = fs::read_to_string(&commondir_file) {
        let trimmed = common_raw.trim();
        if !trimmed.is_empty() {
            let common_dir = resolve_relative_or_abs(&git_dir, trimmed);
            let common_str = common_dir.to_string_lossy();
            let is_submodule =
                common_str.contains("/.git/modules/") || common_str.contains("\\.git\\modules\\");
            return Some(GitDirInfo {
                common_dir,
                is_worktree: !is_submodule,
            });
        }
    }

    Some(GitDirInfo {
        common_dir: git_dir,
        is_worktree: false,
    })
}

/// Walks upward from `start_dir` to locate `.git` (directory or linked worktree pointer file),
/// resolves `commondir` for Git worktrees, and derives the canonical normalized `peer` ID.
#[must_use]
pub fn resolve_git_peer(start_dir: &Path, home_dir: Option<&Path>) -> Option<PeerResolution> {
    let canonical_start = fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    let canonical_home = home_dir.map(|h| fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));

    let mut current: &Path = &canonical_start;
    loop {
        if current.parent().is_none() {
            break;
        }
        if let Some(ref home) = canonical_home {
            if current == home.as_path() {
                break;
            }
        }

        if let Some(git_info) = inspect_dot_git(current) {
            if let Some(remote_raw) = read_git_remote_url(&git_info.common_dir, "origin") {
                if let Some(norm_remote) = normalize_git_remote(&remote_raw) {
                    let peer_id = sanitize_peer_id(&norm_remote);
                    if !peer_id.is_empty() {
                        return Some(PeerResolution {
                            peer_id,
                            repo_root: current.to_path_buf(),
                            common_git_dir: git_info.common_dir,
                            remote_normalized: Some(norm_remote),
                            is_worktree: git_info.is_worktree,
                            source: "git_remote",
                        });
                    }
                }
            }

            // Without `origin`, upstream's `{git_root}` names the peer after the checkout path,
            // so two unrelated repositories that share a directory name never share memories.
            let peer_id = legacy_sanitize(&current.to_string_lossy());
            if !peer_id.trim_matches('-').is_empty() {
                return Some(PeerResolution {
                    peer_id,
                    repo_root: current.to_path_buf(),
                    common_git_dir: git_info.common_dir,
                    remote_normalized: None,
                    is_worktree: git_info.is_worktree,
                    source: "git_root",
                });
            }
        }

        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }

    None
}

/// Upstream's path sanitizer: every character that is not an ASCII letter or digit becomes `-`,
/// one for one, so `/Users/x/repo` becomes `-Users-x-repo`. `peer.source` templates
/// (`{git_root}`, `{cwd}`) depend on this exact mapping, shared with the Codex and Claude Code
/// plugins.
#[must_use]
pub fn legacy_sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Finds the nearest enclosing workspace root directory (`.git` or `.openviking/config*.json`).
#[must_use]
pub fn find_workspace_root(start_dir: &Path, home_dir: Option<&Path>) -> Option<PathBuf> {
    let canonical_start = fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    let canonical_home = home_dir.map(|h| fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));

    let mut current: &Path = &canonical_start;
    loop {
        if current.parent().is_none() {
            break;
        }
        if let Some(ref home) = canonical_home {
            if current == home.as_path() {
                break;
            }
        }

        if current.join(".git").exists()
            || current.join(".openviking").join("config.json").is_file()
            || current
                .join(".openviking")
                .join("config.local.json")
                .is_file()
        {
            return Some(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn remote_to_peer(raw_url: &str) -> String {
        let normalized = normalize_git_remote(raw_url).expect("remote URL should normalize");
        sanitize_peer_id(&normalized)
    }

    #[test]
    fn normalizes_ssh_scp_and_https_remotes_identically() {
        let scp = remote_to_peer("git@github.com:Org/Repo.git");
        let ssh = remote_to_peer("ssh://git@github.com:22/Org/Repo");
        let https = remote_to_peer("https://oauth2:secret_token@GitHub.com/Org/Repo.git/");

        assert_eq!(
            scp, "github.com-org-repo",
            "SCP remote must normalize cleanly"
        );
        assert_eq!(
            ssh, "github.com-org-repo",
            "ssh:// remote must match SCP remote"
        );
        assert_eq!(
            https, "github.com-org-repo",
            "HTTPS remote with embedded credentials must match SSH remote"
        );
    }

    #[test]
    fn resolves_linked_git_worktree_via_commondir() {
        let temp_dir = TempDir::new().expect("tempdir should be created");
        let main_repo = temp_dir.path().join("main-repo");
        let main_git = main_repo.join(".git");
        let worktree_admin = main_git.join("worktrees").join("feature-1");
        let worktree_dir = temp_dir.path().join("wt-feature-1");
        let nested_src = worktree_dir.join("crates").join("core");

        fs::create_dir_all(&worktree_admin).expect("worktree admin dir should be created");
        fs::create_dir_all(&nested_src).expect("nested worktree dir should be created");

        fs::write(
            main_git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:VolcEngine/OpenViking.git # inline comment\n",
        )
        .expect("git config should be written");

        fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", worktree_admin.display()),
        )
        .expect("worktree .git pointer file should be written");
        fs::write(worktree_admin.join("commondir"), "../..\n")
            .expect("commondir pointer should be written");

        let main_res = resolve_git_peer(&main_repo, None).expect("main repo peer should resolve");
        let wt_res = resolve_git_peer(&nested_src, None).expect("worktree peer should resolve");

        assert_eq!(
            main_res.peer_id, "github.com-volcengine-openviking",
            "main repo should derive canonical peer ID"
        );
        assert!(
            !main_res.is_worktree,
            "main repo should not be flagged as worktree"
        );
        assert_eq!(
            wt_res.peer_id, "github.com-volcengine-openviking",
            "linked worktree must resolve to the identical peer ID"
        );
        assert!(
            wt_res.is_worktree,
            "linked worktree must be detected via commondir"
        );
    }

    #[test]
    fn falls_back_to_repo_directory_name_without_origin() {
        let temp_dir = TempDir::new().expect("tempdir should be created");
        let main_repo = temp_dir.path().join("My_Local_Repo");
        let main_git = main_repo.join(".git");
        let worktree_admin = main_git.join("worktrees").join("agent-a");
        let worktree_dir = temp_dir.path().join("random-worktree-folder");

        fs::create_dir_all(&worktree_admin).expect("worktree admin dir should be created");
        fs::create_dir_all(&worktree_dir).expect("worktree dir should be created");
        fs::write(main_git.join("config"), "[core]\n\tbare = false\n")
            .expect("git config without origin should be written");
        fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", worktree_admin.display()),
        )
        .expect("worktree .git file should be written");
        fs::write(worktree_admin.join("commondir"), "../..\n")
            .expect("commondir file should be written");

        let main_peer =
            resolve_git_peer(&main_repo, None).expect("main fallback peer should exist");
        let wt_peer =
            resolve_git_peer(&worktree_dir, None).expect("worktree fallback peer should exist");

        let canonical_main = fs::canonicalize(&main_repo).unwrap();
        assert_eq!(
            main_peer.peer_id,
            legacy_sanitize(&canonical_main.to_string_lossy()),
            "fallback peer should be the sanitized checkout path, as upstream's {{git_root}}"
        );
        assert!(main_peer.peer_id.ends_with("-My-Local-Repo"));
        assert_eq!(main_peer.source, "git_root");
        let canonical_wt = fs::canonicalize(&worktree_dir).unwrap();
        assert_eq!(
            wt_peer.peer_id,
            legacy_sanitize(&canonical_wt.to_string_lossy()),
            "each checkout without origin is its own peer"
        );
        assert_ne!(main_peer.peer_id, wt_peer.peer_id);
    }

    #[test]
    fn legacy_sanitize_maps_each_character_one_for_one() {
        assert_eq!(legacy_sanitize("/Users/x/My Repo"), "-Users-x-My-Repo");
        assert_eq!(legacy_sanitize("a..b"), "a--b");
    }
}
