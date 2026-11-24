//! Startup context: profiles, the memory index and the skills catalog, built for `SessionStart`
//! and again after agy compacts a conversation. Budgets and layout follow the Codex and Claude
//! Code plugins, so one shared `plugin` block tunes them all alike.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::client::OvClient;
use crate::config::ResolvedConfig;
use crate::wire::{OvFsEntry, OvSkillItem};

/// Every read is independent and the hook pays for each round trip, so they run in parallel
/// under one per-request deadline.
pub const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

const MEMORY_MORE_HINT: &str = "search OpenViking memories to find the rest";
const SKILL_MORE_HINT: &str = "search OpenViking skills to find the rest";
const SKILL_USAGE_LINE: &str = "  OpenViking skills (stored in OpenViking, not local files). Before following one, read <dir>/<name>/SKILL.md with the OpenViking read tool.";
const SHARED_SKILLS_ROOT: &str = "viking://agent/skills";
const HEAD_LINES: usize = 8;
const ELLIPSIS: &str = "\n... [profile middle elided] ...\n";
const SKILLS_OPEN: &str = "<available-skills>";
const SKILLS_CLOSE: &str = "</available-skills>";
const PEER_PROFILE_MIN_TOKENS: usize = 200;
const PEER_PROFILE_POINTER: &str = "Omitted for length; read this uri with the read tool.";
const SKILL_DESCRIPTION_TOKENS: usize = 40;
const MEMORY_NODE_LIMIT: usize = 512;
const SKILL_NODE_LIMIT: usize = 200;
const LEGACY_SKILL_LIMIT: usize = 30;

/// The assembled `<user-profile>` / `<available-memories>` / `<available-skills>` payload.
pub struct StartupBlock {
    pub text: String,
    pub stats: Value,
}

/// Upstream's estimate: 1.5 tokens per CJK character, four other characters per token.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    let (cjk, other) = char_mix(text);
    (cjk * 6 + other).div_ceil(4)
}

fn char_mix(text: &str) -> (usize, usize) {
    text.chars().fold((0, 0), |(cjk, other), c| {
        if u32::from(c) >= 0x3000 {
            (cjk + 1, other)
        } else {
            (cjk, other + 1)
        }
    })
}

/// A token budget in characters at this text's own CJK density, so a CJK-heavy profile does not
/// get four characters per token.
fn tokens_to_chars(text: &str, max_tokens: usize) -> usize {
    let (cjk, other) = char_mix(text);
    let len = cjk + other;
    if len == 0 {
        return 0;
    }
    max_tokens * 4 * len / (cjk * 6 + other)
}

fn take_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Keeps the head (identity and first events) and the tail (most recent events) of an
/// oversized profile and drops the noisy middle.
#[must_use]
pub fn elide_profile(content: &str, max_tokens: usize) -> String {
    if estimate_tokens(content) <= max_tokens {
        return content.to_string();
    }
    let max_chars = tokens_to_chars(content, max_tokens).max(400);
    let head_truncate = || {
        format!(
            "{}\n... [profile truncated]",
            take_chars(content, max_chars).trim_end()
        )
    };
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() <= HEAD_LINES + 4 {
        return head_truncate();
    }
    let head = lines[..HEAD_LINES].join("\n");
    let Some(reserve) = max_chars
        .checked_sub(head.chars().count() + ELLIPSIS.chars().count())
        .filter(|reserve| *reserve >= 200)
    else {
        return head_truncate();
    };
    let mut tail_chars = 0;
    let mut tail_start = lines.len();
    for i in (HEAD_LINES + 1..lines.len()).rev() {
        let cost = lines[i].chars().count() + 1;
        if tail_chars + cost > reserve {
            break;
        }
        tail_chars += cost;
        tail_start = i;
    }
    if tail_start >= lines.len() - 1 {
        return head_truncate();
    }
    format!("{head}{ELLIPSIS}{}", lines[tail_start..].join("\n"))
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    root: String,
    abstract_text: String,
}

#[derive(Debug, Default)]
struct Listing {
    lines: Vec<String>,
    used: usize,
    dropped: usize,
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One directory listing within `budget` tokens. A listing that has to stop says how many
/// entries it left out, so the model knows to search for the rest.
fn format_listing(header_uri: &str, entries: &[Entry], budget: usize, more_hint: &str) -> Listing {
    if entries.is_empty() {
        return Listing::default();
    }
    let stub = || {
        let line = format!(
            "  {header_uri}/  ({} entries, budget too tight; {more_hint})",
            entries.len()
        );
        Listing {
            used: estimate_tokens(&line),
            lines: vec![line],
            dropped: entries.len(),
        }
    };
    let header = format!("  {header_uri}/");
    let header_tokens = estimate_tokens(&header);
    if header_tokens > budget {
        return stub();
    }
    let mut lines = vec![header];
    let mut used = header_tokens;
    for (i, entry) in entries.iter().enumerate() {
        let desc = if entry.abstract_text.is_empty() {
            String::new()
        } else {
            format!(
                " — {}",
                take_chars(&collapse_whitespace(&entry.abstract_text), 200)
            )
        };
        let line = format!("    - {}{desc}", entry.name);
        let tokens = estimate_tokens(&line);
        if used + tokens > budget {
            let mut remaining = entries.len() - i;
            let tail_for = |n: usize| format!("    ... +{n} more, {more_hint}");
            while used + estimate_tokens(&tail_for(remaining)) > budget && lines.len() > 1 {
                if let Some(last) = lines.pop() {
                    used -= estimate_tokens(&last);
                }
                remaining += 1;
            }
            if lines.len() == 1 {
                return stub();
            }
            let tail = tail_for(remaining);
            let tail_tokens = estimate_tokens(&tail);
            if used + tail_tokens <= budget {
                lines.push(tail);
                used += tail_tokens;
            }
            return Listing {
                lines,
                used,
                dropped: remaining,
            };
        }
        lines.push(line);
        used += tokens;
    }
    Listing {
        lines,
        used,
        dropped: 0,
    }
}

struct Scope {
    profile_uri: String,
    pref_uri: String,
    ent_uri: String,
    profile: Option<String>,
    prefs: Vec<Entry>,
    ents: Vec<Entry>,
}

impl Scope {
    fn new(root: &str) -> Self {
        Self {
            profile_uri: format!("{root}/profile.md"),
            pref_uri: format!("{root}/preferences"),
            ent_uri: format!("{root}/entities"),
            profile: None,
            prefs: Vec::new(),
            ents: Vec::new(),
        }
    }
}

/// Each populated root takes its fair share of what is left; within a root the preferences get
/// half and the entities the rest.
fn format_listing_groups(scopes: &[Scope], budget: usize) -> Listing {
    let populated: Vec<&Scope> = scopes
        .iter()
        .filter(|s| !s.prefs.is_empty() || !s.ents.is_empty())
        .collect();
    let mut out = Listing::default();
    for (i, scope) in populated.iter().enumerate() {
        let share = budget.saturating_sub(out.used) / (populated.len() - i);
        let prefs = format_listing(&scope.pref_uri, &scope.prefs, share / 2, MEMORY_MORE_HINT);
        let ents = format_listing(
            &scope.ent_uri,
            &scope.ents,
            share.saturating_sub(prefs.used),
            MEMORY_MORE_HINT,
        );
        out.used += prefs.used + ents.used;
        out.dropped += prefs.dropped + ents.dropped;
        out.lines.extend(prefs.lines);
        out.lines.extend(ents.lines);
    }
    out
}

/// Drops peer-profile lines already in the user profile, so the budget is not spent twice on
/// one fact. `None` when nothing but headings would be left.
fn dedupe_profile_lines(peer: &str, user: Option<&str>) -> Option<String> {
    let seen: std::collections::HashSet<&str> = user
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut kept: Vec<&str> = Vec::new();
    for line in peer.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && seen.contains(trimmed) {
            continue;
        }
        if trimmed.is_empty() && kept.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        kept.push(line);
    }
    let has_content = kept
        .iter()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
    has_content.then(|| kept.join("\n").trim().to_string())
}

fn envelope_tag() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)</?(?:openviking-context|openviking-session|available-skills|available-memories|user-profile|memory)\b[^>]*>",
        )
        .ok()
    })
    .as_ref()
}

/// Skill descriptions are user-written and the shared root is account-wide: one must never
/// close or open the envelope the catalog sits in.
fn sanitize_inline(text: &str) -> String {
    let collapsed = collapse_whitespace(text);
    match envelope_tag() {
        Some(re) => re
            .replace_all(&collapsed, |caps: &regex::Captures<'_>| {
                caps[0].replace('<', "&lt;").replace('>', "&gt;")
            })
            .into_owned(),
        None => collapsed,
    }
}

fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    if estimate_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let keep = tokens_to_chars(text, max_tokens).saturating_sub(1).max(1);
    format!("{}…", take_chars(text, keep).trim_end())
}

/// The user's own skills first; a shared skill with the same name is dropped, since the private
/// one is the one to load.
fn skill_groups(items: Vec<OvSkillItem>) -> Vec<Vec<Entry>> {
    let mut own = Vec::new();
    let mut shared = Vec::new();
    for item in items {
        let name = item.name.unwrap_or_default().trim().to_string();
        let uri = item
            .uri
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let Some(slash) = uri.rfind('/') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let entry = Entry {
            name,
            root: uri[..slash].to_string(),
            abstract_text: truncate_to_tokens(
                &sanitize_inline(item.description.as_deref().unwrap_or_default()),
                SKILL_DESCRIPTION_TOKENS,
            ),
        };
        if uri.starts_with(&format!("{SHARED_SKILLS_ROOT}/")) {
            shared.push(entry);
        } else {
            own.push(entry);
        }
    }
    let own_names: std::collections::HashSet<String> = own.iter().map(|e| e.name.clone()).collect();
    shared.retain(|e| !own_names.contains(&e.name));
    own.sort_by(|a, b| a.name.cmp(&b.name));
    shared.sort_by(|a, b| a.name.cmp(&b.name));
    [own, shared]
        .into_iter()
        .filter(|g| !g.is_empty())
        .collect()
}

fn render_skill_groups(groups: &[Vec<Entry>], budget: usize, with_descriptions: bool) -> Listing {
    let lists: Vec<Vec<Entry>> = groups
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|e| Entry {
                    abstract_text: if with_descriptions {
                        e.abstract_text.clone()
                    } else {
                        String::new()
                    },
                    ..e.clone()
                })
                .collect()
        })
        .collect();
    let full_cost: Vec<usize> = lists
        .iter()
        .map(|entries| format_listing(&entries[0].root, entries, usize::MAX, SKILL_MORE_HINT).used)
        .collect();
    let mut out = Listing::default();
    for (i, entries) in lists.iter().enumerate() {
        // A group takes its fair share, or everything the later groups leave when listed in
        // full, whichever is larger.
        let remaining = budget.saturating_sub(out.used);
        let later: usize = full_cost[i + 1..].iter().sum();
        let share = (remaining / (lists.len() - i)).max(remaining.saturating_sub(later));
        let block = format_listing(&entries[0].root, entries, share, SKILL_MORE_HINT);
        out.used += block.used;
        out.dropped += block.dropped;
        out.lines.extend(block.lines);
    }
    out
}

/// `<available-skills>` within its own budget: descriptions when they fit, otherwise names only,
/// and a one-line count when not even one name fits.
fn format_skill_catalog(groups: &[Vec<Entry>], budget: usize) -> Listing {
    let count: usize = groups.iter().map(Vec::len).sum();
    if count == 0 {
        return Listing::default();
    }
    let frame = estimate_tokens(SKILLS_OPEN)
        + estimate_tokens(SKILL_USAGE_LINE)
        + estimate_tokens(SKILLS_CLOSE);
    let listing_budget = budget.saturating_sub(frame);
    let mut body = render_skill_groups(groups, listing_budget, true);
    if body.dropped > 0 {
        body = render_skill_groups(groups, listing_budget, false);
    }
    if body.dropped >= count {
        let stub =
            format!("{SKILLS_OPEN}{count} OpenViking skills; {SKILL_MORE_HINT}.{SKILLS_CLOSE}");
        let tokens = estimate_tokens(&stub);
        if tokens > budget {
            return Listing {
                dropped: count,
                ..Listing::default()
            };
        }
        return Listing {
            lines: vec![stub],
            used: tokens,
            dropped: count,
        };
    }
    let mut lines = vec![SKILLS_OPEN.to_string(), SKILL_USAGE_LINE.to_string()];
    lines.extend(body.lines);
    lines.push(SKILLS_CLOSE.to_string());
    Listing {
        lines,
        used: frame + body.used,
        dropped: body.dropped,
    }
}

fn leaf_entries(parent: &str, entries: Vec<OvFsEntry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = entries
        .into_iter()
        .filter(|e| !e.is_dir && e.access.as_deref() != Some("denied"))
        .filter_map(|e| {
            let name = e.rel_path.or(e.name).or_else(|| {
                e.uri.as_deref().and_then(|uri| {
                    uri.strip_prefix(parent)
                        .map(|rest| rest.trim_start_matches('/').to_string())
                })
            })?;
            let markdown = std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
            (markdown && !name.split('/').any(|seg| seg.starts_with('.'))).then(|| Entry {
                name,
                root: parent.to_string(),
                abstract_text: e.abstract_text.unwrap_or_default().trim().to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Servers without `GET /api/v1/skills` still answer a listing of both skill roots.
async fn fetch_skills(client: &OvClient) -> Vec<OvSkillItem> {
    if let Ok(skills) = client
        .list_skills(SKILL_NODE_LIMIT, PER_REQUEST_TIMEOUT)
        .await
    {
        return skills;
    }
    let (own, shared) = tokio::join!(
        client.list_entries(
            "viking://~/skills",
            false,
            LEGACY_SKILL_LIMIT,
            PER_REQUEST_TIMEOUT
        ),
        client.list_entries(
            SHARED_SKILLS_ROOT,
            false,
            LEGACY_SKILL_LIMIT,
            PER_REQUEST_TIMEOUT
        ),
    );
    [("viking://~/skills", own), (SHARED_SKILLS_ROOT, shared)]
        .into_iter()
        .flat_map(|(root, listing)| {
            listing
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e.access.as_deref() != Some("denied"))
                .filter_map(move |e| {
                    let name = e.name.or(e.rel_path)?.trim_end_matches('/').to_string();
                    let uri = e.uri.unwrap_or_else(|| format!("{root}/{name}"));
                    (!name.starts_with('.')).then_some(OvSkillItem {
                        name: Some(name),
                        uri: Some(uri),
                        description: e.abstract_text,
                    })
                })
        })
        .collect()
}

/// Profiles, memory index and skills catalog for the current user and workspace peer, or
/// `None` when the server holds none of them.
pub async fn build(client: &OvClient, cfg: &ResolvedConfig) -> Option<StartupBlock> {
    if !cfg.inject_startup_memories {
        return None;
    }
    let mut scopes = vec![Scope::new("viking://~/memories")];
    if !cfg.peer_id.is_empty() {
        scopes.push(Scope::new(&format!(
            "viking://~/peers/{}/memories",
            cfg.peer_id
        )));
    }
    let skill_budget = budgets(cfg).skills;
    let read_scope = |scope: &Scope| {
        let (profile, pref, ent) = (
            scope.profile_uri.clone(),
            scope.pref_uri.clone(),
            scope.ent_uri.clone(),
        );
        async move {
            tokio::join!(
                client.read_profile(&profile, PER_REQUEST_TIMEOUT),
                client.list_entries(&pref, true, MEMORY_NODE_LIMIT, PER_REQUEST_TIMEOUT),
                client.list_entries(&ent, true, MEMORY_NODE_LIMIT, PER_REQUEST_TIMEOUT),
            )
        }
    };
    let user_read = read_scope(&scopes[0]);
    let peer_read = scopes.get(1).map(read_scope);
    let (user, peer, skills) = tokio::join!(
        user_read,
        async {
            match peer_read {
                Some(read) => Some(read.await),
                None => None,
            }
        },
        async {
            if skill_budget > 0 {
                fetch_skills(client).await
            } else {
                Vec::new()
            }
        },
    );
    for (scope, read) in scopes
        .iter_mut()
        .zip([Some(user), peer].into_iter().flatten())
    {
        let (profile, prefs, ents) = read;
        scope.profile = profile
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        scope.prefs = leaf_entries(&scope.pref_uri, prefs.unwrap_or_default());
        scope.ents = leaf_entries(&scope.ent_uri, ents.unwrap_or_default());
    }

    assemble(cfg, &scopes, skills)
}

/// Token budgets under the byte cap.
struct Budgets {
    /// What the whole block may cost: an estimated token is at most about 4 UTF-8 bytes,
    /// so the cap allows `bytes / 4` tokens, less a margin for the tags (upstream `capTokens`).
    cap: usize,
    skills: usize,
}

fn budgets(cfg: &ResolvedConfig) -> Budgets {
    let cap = if cfg.session_start_max_bytes > 0 {
        (cfg.session_start_max_bytes / 4).saturating_sub(50)
    } else {
        usize::MAX
    };
    let skills = if cfg.skill_catalog {
        cfg.skill_catalog_token_budget.min(cap / 4)
    } else {
        0
    };
    Budgets { cap, skills }
}

/// Fits the profiles, memory index and skills catalog under the token budgets, shrunk to the
/// byte cap so a large index is trimmed (with "+N more" lines) rather than dropped.
fn assemble(
    cfg: &ResolvedConfig,
    scopes: &[Scope],
    skills: Vec<OvSkillItem>,
) -> Option<StartupBlock> {
    let budget = budgets(cfg);
    let skill_groups = skill_groups(skills);
    let skills = format_skill_catalog(&skill_groups, budget.skills);
    let user_profile = scopes[0].profile.clone();
    let peer_profile = scopes
        .get(1)
        .and_then(|s| s.profile.as_deref())
        .and_then(|p| dedupe_profile_lines(p, user_profile.as_deref()));
    if user_profile.is_none()
        && peer_profile.is_none()
        && scopes
            .iter()
            .all(|s| s.prefs.is_empty() && s.ents.is_empty())
        && skills.lines.is_empty()
    {
        return None;
    }

    // The profiles get up to half of the budget; the user profile first, the peer profile what
    // it leaves. The listings split the rest.
    let memory_budget = cfg
        .profile_token_budget
        .min(budget.cap.saturating_sub(skills.used));
    let profile_budget = memory_budget / 2;
    let mut profile_lines = Vec::new();
    let mut profile_tokens = 0;
    if let Some(ref profile) = user_profile {
        let trimmed = elide_profile(profile, profile_budget);
        profile_tokens += estimate_tokens(&trimmed);
        profile_lines.push(format!("<user-profile uri=\"{}\">", scopes[0].profile_uri));
        profile_lines.push(trimmed);
        profile_lines.push("</user-profile>".to_string());
    }
    if let (Some(peer), Some(scope)) = (peer_profile.as_deref(), scopes.get(1)) {
        let peer_budget = profile_budget.saturating_sub(profile_tokens);
        let trimmed = (estimate_tokens(peer) <= peer_budget
            || peer_budget >= PEER_PROFILE_MIN_TOKENS)
            .then(|| elide_profile(peer, peer_budget))
            .filter(|t| estimate_tokens(t) <= peer_budget);
        let lines = match trimmed {
            Some(text) => vec![
                format!("<user-profile uri=\"{}\">", scope.profile_uri),
                text,
                "</user-profile>".to_string(),
            ],
            None => vec![format!(
                "<user-profile uri=\"{}\">{PEER_PROFILE_POINTER}</user-profile>",
                scope.profile_uri
            )],
        };
        profile_tokens += lines.iter().map(|l| estimate_tokens(l)).sum::<usize>();
        profile_lines.extend(lines);
    }
    let listings = format_listing_groups(scopes, memory_budget.saturating_sub(profile_tokens));
    let mut memory_lines = if listings.lines.is_empty() {
        Vec::new()
    } else {
        let mut lines = vec!["<available-memories>".to_string()];
        lines.extend(listings.lines);
        lines.push("</available-memories>".to_string());
        lines
    };
    let mut skill_lines = skills.lines;
    let render = |memory: &[String], skill: &[String]| {
        profile_lines
            .iter()
            .chain(memory)
            .chain(skill)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut text = render(&memory_lines, &skill_lines);
    let cap = cfg.session_start_max_bytes;
    let mut index_dropped = false;
    let mut catalog_dropped = false;
    // The estimate can still run over on multi-byte punctuation: the index goes first, then
    // the catalog.
    if cap > 0 && text.len() > cap {
        memory_lines.clear();
        index_dropped = true;
        text = render(&memory_lines, &skill_lines);
    }
    if cap > 0 && text.len() > cap {
        skill_lines.clear();
        catalog_dropped = true;
        text = render(&memory_lines, &skill_lines);
    }
    if text.trim().is_empty() {
        return None;
    }
    let pref_count: usize = scopes.iter().map(|s| s.prefs.len()).sum();
    let ent_count: usize = scopes.iter().map(|s| s.ents.len()).sum();
    let skill_count: usize = skill_groups.iter().map(Vec::len).sum();
    let stats = json!({
        "bytes": text.len(),
        "tokens": estimate_tokens(&text),
        "memories": pref_count + ent_count,
        "memories_omitted": if index_dropped { pref_count + ent_count } else { listings.dropped },
        "skills": skill_count,
        "skills_omitted": if catalog_dropped { skill_count } else { skills.dropped },
        "peer_profile": peer_profile.is_some(),
    });
    Some(StartupBlock { text, stats })
}

/// Wraps a startup block in the envelope the capture sanitizer strips before storing turns.
#[must_use]
pub fn wrap(source: &str, body: &str) -> String {
    format!(
        "<openviking-context source=\"{source}\">\nOpenViking memory for this user and workspace (past sessions and saved knowledge; live instructions take precedence). Expand any viking:// URI with the OpenViking MCP read or search tools.\n{body}\n</openviking-context>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                name: format!("topic-{i:02}.md"),
                root: "viking://~/memories/preferences".into(),
                abstract_text: "a short abstract".into(),
            })
            .collect()
    }

    #[test]
    fn estimates_cjk_at_one_and_a_half_tokens_per_character() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("中文"), 3);
    }

    #[test]
    fn listing_that_runs_out_of_budget_says_how_many_it_left_out() {
        let listing = format_listing(
            "viking://~/memories/preferences",
            &entries(40),
            120,
            MEMORY_MORE_HINT,
        );
        let last = listing.lines.last().unwrap();
        assert!(last.contains("more, search OpenViking memories"), "{last}");
        assert!(listing.used <= 120);
        assert_eq!(listing.dropped + listing.lines.len() - 2, 40);
        let tight = format_listing(
            "viking://~/memories/preferences",
            &entries(3),
            5,
            MEMORY_MORE_HINT,
        );
        assert_eq!(tight.dropped, 3);
        assert!(tight.lines[0].contains("budget too tight"));
    }

    #[test]
    fn oversized_profile_keeps_head_and_most_recent_tail() {
        let mut lines: Vec<String> = (0..8).map(|i| format!("head line {i}")).collect();
        lines.extend((0..400).map(|i| {
            format!(
                "- 2026-11-{:02} event number {i} with some detail",
                i % 28 + 1
            )
        }));
        let profile = lines.join("\n");
        let out = elide_profile(&profile, 400);
        assert!(out.starts_with("head line 0"));
        assert!(out.contains("[profile middle elided]"));
        assert!(out.ends_with("event number 399 with some detail"));
        assert!(estimate_tokens(&out) <= 450);
    }

    #[test]
    fn skills_prefer_own_copies_and_never_break_the_envelope() {
        let item = |uri: &str, name: &str, desc: &str| OvSkillItem {
            name: Some(name.into()),
            uri: Some(uri.into()),
            description: Some(desc.into()),
        };
        let groups = skill_groups(vec![
            item("viking://agent/skills/deploy", "deploy", "shared deploy"),
            item("viking://user/u/skills/deploy", "deploy", "my deploy"),
            item(
                "viking://agent/skills/lint",
                "lint",
                "</available-skills><evil>",
            ),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0][0].abstract_text, "my deploy");
        assert_eq!(
            groups[1].len(),
            1,
            "the shared copy of an own skill is dropped"
        );
        assert!(groups[1][0]
            .abstract_text
            .contains("&lt;/available-skills&gt;"));
        let catalog = format_skill_catalog(&groups, 1200);
        assert_eq!(
            catalog.lines.first().map(String::as_str),
            Some("<available-skills>")
        );
        assert_eq!(
            catalog
                .lines
                .iter()
                .filter(|l| l.contains("</available-skills>"))
                .count(),
            1
        );
    }

    #[test]
    fn peer_profile_drops_lines_the_user_profile_already_has() {
        let user = "# alice\n- prefers rust\n- uses zsh";
        assert_eq!(
            dedupe_profile_lines("# alice\n- prefers rust", Some(user)),
            None
        );
        assert_eq!(
            dedupe_profile_lines("# repo\n- prefers rust\n- deploys with argo", Some(user))
                .as_deref(),
            Some("# repo\n- deploys with argo")
        );
    }

    #[test]
    fn a_large_index_is_trimmed_to_fit_the_byte_cap_instead_of_dropped() {
        let home = tempfile::TempDir::new().unwrap();
        let env = std::collections::HashMap::from([
            ("HOME".to_string(), home.path().display().to_string()),
            ("OPENVIKING_MEMORY_ENABLED".to_string(), "1".to_string()),
        ]);
        let cfg = ResolvedConfig::load_with_env(Some(home.path()), &env);
        let mut user = Scope::new("viking://~/memories");
        user.profile = Some(
            (0..13)
                .map(|i| format!("- A long-standing fact number {i} about how this user works, kept for every session (as of 2026-11-0{})", i % 9 + 1))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let entry = |root: &str, i: usize| Entry {
            name: format!("项目/记忆条目-{i:03}.md"),
            root: root.into(),
            abstract_text: "这条记忆记录了一个具体的工程决定和它的原因".into(),
        };
        user.prefs = (0..160)
            .map(|i| entry("viking://~/memories/preferences", i))
            .collect();
        user.ents = (0..315)
            .map(|i| entry("viking://~/memories/entities", i))
            .collect();
        let skills = (0..9)
            .map(|i| OvSkillItem {
                name: Some(format!("skill-{i}")),
                uri: Some(format!("viking://user/alice/skills/skill-{i}")),
                description: Some("Guide for a recurring engineering task, used when the user asks for it by name or describes it.".into()),
            })
            .collect();

        let block = assemble(&cfg, &[user], skills).unwrap();
        assert!(
            block.text.len() <= cfg.session_start_max_bytes,
            "{} bytes",
            block.text.len()
        );
        assert!(
            block.text.contains("<available-memories>"),
            "{}",
            block.text
        );
        assert!(block.text.contains("more, search OpenViking memories"));
        assert!(block.text.contains("<available-skills>"));
        let omitted = block.stats["memories_omitted"].as_u64().unwrap();
        assert!(omitted > 0 && omitted < 475, "{}", block.stats);
    }
}
