//! `PostInvocation` hook showing which `OpenViking` sources the turn drew on, once per turn, as a
//! user-only line after the final answer (the Codex plugin's "OV-Usage" report).

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value};

use crate::config::{self, ResolvedConfig};
use crate::hooks::transcript;
use crate::wire::{InjectOutput, PreInvocationInput};

/// The final answer and the turn's own `OpenViking` calls sit at the end of the transcript.
const TAIL_BYTES: u64 = 512 * 1024;

/// Evaluates `PostInvocation` without network access; it only reads local state.
#[must_use]
pub fn run_sync(raw_stdin: &str) -> InjectOutput {
    let Ok(parsed) = serde_json::from_str::<PreInvocationInput>(raw_stdin) else {
        return InjectOutput::empty();
    };
    let cfg = ResolvedConfig::load(parsed.common.effective_workspace().map(Path::new));
    if !cfg.enabled || cfg.usage_view == "off" || cfg.usage_output != "notice" {
        return InjectOutput::empty();
    }
    let Some(conversation_id) = parsed
        .common
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return InjectOutput::empty();
    };
    if cfg.is_bypassed(Some(conversation_id), parsed.common.effective_workspace()) {
        return InjectOutput::empty();
    }
    let Some(path) = parsed.common.transcript_path.as_deref() else {
        return InjectOutput::empty();
    };
    let Some(turn) = read_finished_turn(&transcript::preferred_transcript(Path::new(path))) else {
        return InjectOutput::empty();
    };
    let state = config::load_conversation_state(&cfg, conversation_id);
    if state.usage_reported_turn == Some(turn.user_step) {
        return InjectOutput::empty();
    }
    let recalled: Vec<String> = if state.recall_turn == Some(turn.user_step) {
        state.recalled_uris.clone()
    } else {
        Vec::new()
    };
    let _ = config::update_conversation_state(&cfg, conversation_id, |s| {
        s.usage_reported_turn = Some(turn.user_step);
    });
    let Some(report) = format_report(&recalled, &turn, cfg.usage_view == "expanded") else {
        return InjectOutput::empty();
    };
    cfg.log_debug(
        "hook.post_invocation",
        "usage_reported",
        json!({ "turn": turn.user_step, "recalled": recalled.len(), "read": turn.read.len() }),
    );
    InjectOutput::user_notice(report)
}

/// The turn that just ended with a final answer, and the `OpenViking` lookups made in it.
#[derive(Debug, Default)]
struct FinishedTurn {
    user_step: u64,
    queries: Vec<String>,
    read: BTreeSet<String>,
}

fn read_tail(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    if start > 0 {
        let first_newline = buf.iter().position(|b| *b == b'\n')?;
        buf.drain(..=first_newline);
    }
    Some(buf)
}

/// Scans backwards: the newest model step must be a final answer (text, no tool calls but
/// `finish`), and everything back to the user's input is this turn.
fn read_finished_turn(path: &Path) -> Option<FinishedTurn> {
    let tail = read_tail(path)?;
    let mut turn = FinishedTurn::default();
    let mut decided = false;
    for line in tail.split(|b| *b == b'\n').rev() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mentions = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
        if !decided {
            if !(mentions(b"PLANNER_RESPONSE") || mentions(b"GENERIC") || mentions(b"USER_INPUT")) {
                continue;
            }
            let record: Value = serde_json::from_slice(line).ok()?;
            match record["type"].as_str() {
                Some("PLANNER_RESPONSE") => {
                    let calls = record["tool_calls"].as_array();
                    let final_answer = record["status"].as_str().is_none_or(|s| s == "DONE")
                        && calls.is_none_or(|calls| {
                            calls.iter().all(|c| c["name"].as_str() == Some("finish"))
                        });
                    if !final_answer {
                        return None;
                    }
                    decided = true;
                    continue;
                }
                Some("GENERIC" | "USER_INPUT") => return None,
                _ => continue,
            }
        }
        if mentions(b"USER_INPUT") {
            if let Ok(record) = serde_json::from_slice::<Value>(line) {
                if record["type"] == "USER_INPUT"
                    && record["source"]
                        .as_str()
                        .is_none_or(|s| s == "USER_EXPLICIT")
                {
                    turn.user_step = record["step_index"].as_u64()?;
                    return Some(turn);
                }
            }
        }
        if mentions(b"call_mcp_tool") && mentions(b"openviking") {
            if let Ok(record) = serde_json::from_slice::<Value>(line) {
                collect_lookups(&record, &mut turn);
            }
        }
    }
    None
}

fn collect_lookups(record: &Value, turn: &mut FinishedTurn) {
    for call in record["tool_calls"].as_array().into_iter().flatten() {
        let args = &call["args"];
        if call["name"] != "call_mcp_tool"
            || !args["ServerName"]
                .as_str()
                .is_some_and(|s| s.to_ascii_lowercase().contains("openviking"))
        {
            continue;
        }
        let arguments = &args["Arguments"];
        match args["ToolName"].as_str() {
            Some("search" | "find") => {
                if let Some(query) = arguments["query"].as_str() {
                    turn.queries.push(query.to_string());
                }
            }
            Some("read") => {
                let uris = arguments["uris"].as_array().into_iter().flatten();
                for uri in arguments["uri"]
                    .as_str()
                    .into_iter()
                    .chain(uris.filter_map(Value::as_str))
                {
                    turn.read.insert(uri.to_string());
                }
            }
            _ => {}
        }
    }
}

/// A source the answer could have drawn on: not a bare top-level directory.
fn is_source(uri: &str) -> bool {
    let parts: Vec<&str> = uri
        .trim_start_matches("viking://")
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    !uri.ends_with('/')
        && parts.len() >= 2
        && !matches!(
            parts.last().copied(),
            Some("resources" | "user" | "agent" | "session" | "memories" | "skills" | "peers")
        )
}

fn group_of(uri: &str) -> &'static str {
    if uri.contains("/skills/") {
        "skill"
    } else if uri.contains("/memories/") {
        "memory"
    } else if uri.contains("resources") {
        "resource"
    } else {
        "source"
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Upstream's summary line, e.g. `OpenViking · 3 sources · 2 memories · 1 resource · 1 read`.
fn format_report(recalled: &[String], turn: &FinishedTurn, expanded: bool) -> Option<String> {
    let sources: BTreeSet<&str> = recalled
        .iter()
        .map(String::as_str)
        .chain(turn.read.iter().map(String::as_str))
        .filter(|uri| is_source(uri))
        .collect();
    if sources.is_empty() && turn.queries.is_empty() {
        return None;
    }
    let count = |group: &str| sources.iter().filter(|uri| group_of(uri) == group).count();
    let mut parts = vec![
        "OpenViking".to_string(),
        plural(sources.len(), "source", "sources"),
    ];
    for (group, one, many) in [
        ("memory", "memory", "memories"),
        ("resource", "resource", "resources"),
        ("skill", "skill", "skills"),
    ] {
        let n = count(group);
        if n > 0 {
            parts.push(plural(n, one, many));
        }
    }
    if !turn.read.is_empty() {
        parts.push(format!("{} read", turn.read.len()));
    }
    let mut lines = vec![parts.join(" · ")];
    if expanded {
        for uri in &sources {
            let how = if turn.read.contains(*uri) {
                "read"
            } else {
                "auto-recalled"
            };
            lines.push(format!("- `{uri}` · {how}"));
        }
        for query in &turn.queries {
            lines.push(format!("- searched “{query}”"));
        }
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn transcript(lines: &[Value]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file
    }

    #[test]
    fn reports_only_after_a_final_answer_and_counts_this_turns_lookups() {
        let user = json!({"step_index":4,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"why?"});
        let lookup = json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
            {"name":"call_mcp_tool","args":{"ServerName":"openviking_openviking","ToolName":"find","Arguments":{"query":"deploy target"}}},
            {"name":"call_mcp_tool","args":{"ServerName":"openviking_openviking","ToolName":"read","Arguments":{"uri":"viking://resources/docs/deploy.md"}}}
        ]});
        let result = json!({"step_index":6,"type":"GENERIC","status":"DONE","content":"ok"});
        let answer =
            json!({"step_index":8,"type":"PLANNER_RESPONSE","status":"DONE","content":"Because."});

        let mid = transcript(&[user.clone(), lookup.clone(), result.clone()]);
        assert!(
            read_finished_turn(mid.path()).is_none(),
            "a tool result is not the end of a turn"
        );

        let done = transcript(&[user, lookup, result, answer]);
        let turn = read_finished_turn(done.path()).unwrap();
        assert_eq!(turn.user_step, 4);
        assert_eq!(turn.queries, ["deploy target"]);
        let report = format_report(
            &["viking://user/u/memories/preferences/x.md".into()],
            &turn,
            true,
        )
        .unwrap();
        assert!(
            report.starts_with("OpenViking · 2 sources · 1 memory · 1 resource · 1 read"),
            "{report}"
        );
        assert!(report.contains("`viking://resources/docs/deploy.md` · read"));
        assert!(report.contains("searched “deploy target”"));
    }

    #[test]
    fn directories_are_not_sources() {
        assert!(!is_source("viking://~/memories"));
        assert!(!is_source("viking://resources/"));
        assert!(is_source("viking://~/memories/preferences/rust.md"));
    }
}
