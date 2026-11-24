//! Antigravity `transcript.jsonl` tail reader, incremental cursor reader, and sanitizer.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde_json::{Map, Value};

use crate::config::{self, CompiledFilter};
use crate::wire::OvMessagePart;

/// Initial trailing byte window (`64 KiB`) read by `PreInvocation` for fast tail extraction.
pub const TAIL_SEEK_WINDOW_BYTES: u64 = 64 * 1024;

/// Maximum backward expansion window (`2 MiB`) when large intermediate tool outputs exceed `64 KiB`.
const MAX_TAIL_SCAN_BYTES: u64 = 2 * 1024 * 1024;

/// Latest user turn located near the tail of `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestUserTurn {
    pub byte_offset: u64,
    pub step_index: Option<u64>,
    pub answered: bool,
    pub truncated: bool,
    pub text: String,
}

/// Raw unsanitized user or assistant turn parsed from `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTranscriptTurn {
    pub role: String,
    pub text: String,
    pub tools: Vec<RawToolUse>,
}

/// One finished tool call: an entry of a `PLANNER_RESPONSE` step's `tool_calls` paired with
/// the `GENERIC` step that answered it. agy writes the k results of a step right after it,
/// in call order, and gives them no ids, so `id` is synthesized from the step.
#[derive(Debug, Clone, PartialEq)]
pub struct RawToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub output: String,
    pub failed: bool,
    /// The result was still running when agy wrote it, as for a backgrounded command.
    pub running: bool,
}

/// Upper bound for any captured string. Codex and Claude Code send text verbatim and leave
/// large tool output to the server, which externalizes it; this only stops pathological
/// payloads.
pub const MAX_CAPTURE_CHARS: usize = 1_000_000;

/// Incremental batch of turns and updated file byte cursor read from `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptDelta {
    pub turns: Vec<RawTranscriptTurn>,
    pub new_byte_offset: u64,
}

fn compiled_regex(
    slot: &'static OnceLock<Option<Regex>>,
    pattern: &'static str,
) -> Option<&'static Regex> {
    slot.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

fn replace_tag_pattern(
    input: &str,
    slot: &'static OnceLock<Option<Regex>>,
    pattern: &'static str,
) -> String {
    match compiled_regex(slot, pattern) {
        Some(re) => re.replace_all(input, "").into_owned(),
        None => input.to_string(),
    }
}

fn re_user_request_unwrap() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    compiled_regex(&RE, r"(?is)<user_request\b[^>]*>(.*?)</user_request>")
}

/// Strips injected `OpenViking` XML blocks (`<openviking-context>`, `<openviking-session>`,
/// `<available-skills>`) and Antigravity system metadata wrappers from text.
#[must_use]
pub fn strip_injected_blocks(raw: &str) -> String {
    static RE_OV_CONTEXT: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_OV_SESSION: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_AVAILABLE_SKILLS: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_ADDITIONAL_METADATA: OnceLock<Option<Regex>> = OnceLock::new();
    static RE_USER_SETTINGS_CHANGE: OnceLock<Option<Regex>> = OnceLock::new();

    let mut cleaned = replace_tag_pattern(
        raw,
        &RE_OV_CONTEXT,
        r"(?is)<openviking-context\b[^>]*>.*?</openviking-context>",
    );
    cleaned = replace_tag_pattern(
        &cleaned,
        &RE_OV_SESSION,
        r"(?is)<openviking-session\b[^>]*>.*?</openviking-session>",
    );
    cleaned = replace_tag_pattern(
        &cleaned,
        &RE_AVAILABLE_SKILLS,
        r"(?is)<available-skills\b[^>]*>.*?</available-skills>",
    );
    cleaned = replace_tag_pattern(
        &cleaned,
        &RE_ADDITIONAL_METADATA,
        r"(?is)<additional_metadata\b[^>]*>.*?</additional_metadata>",
    );
    cleaned = replace_tag_pattern(
        &cleaned,
        &RE_USER_SETTINGS_CHANGE,
        r"(?is)<user_settings_change\b[^>]*>.*?</user_settings_change>",
    );
    if let Some(re) = re_user_request_unwrap() {
        if let Some(caps) = re.captures(&cleaned) {
            if let Some(inner) = caps.get(1) {
                let rest = re.replace_all(&cleaned, "");
                if rest.trim().is_empty() {
                    cleaned = inner.as_str().to_string();
                } else {
                    let inner_trimmed = inner.as_str().trim();
                    let rest_trimmed = rest.trim();
                    cleaned = format!("{inner_trimmed}\n{rest_trimmed}");
                }
            }
        }
    }

    cleaned.trim().to_string()
}

/// Keeps the head and tail of an oversized string around a visible marker.
#[must_use]
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars / 4 * 3;
    let tail = max_chars - head;
    let omitted = count - head - tail;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(count - tail).collect();
    format!("{head_text}\n[… {omitted} characters omitted by ov-agy …]\n{tail_text}")
}

/// Turns one transcript turn into message parts, or `None` when nothing is left to send.
///
/// Injected `OpenViking` blocks are stripped from the text first. Only the text decides
/// keep or drop (`d`/`k` rules), and a turn dropped on its text takes its tool calls with
/// it; tool inputs and outputs get the `s` substitutions.
#[must_use]
pub fn shape_turn_parts(
    turn: &RawTranscriptTurn,
    capture_filters: &[CompiledFilter],
    tool_max_chars: usize,
    include_tools: bool,
) -> Option<Vec<OvMessagePart>> {
    let mut parts = Vec::new();
    let stripped = strip_injected_blocks(&turn.text);
    if !stripped.is_empty() {
        let verdict = config::apply_input_filters(&stripped, capture_filters, &turn.role);
        if verdict.dropped {
            return None;
        }
        if !verdict.text.is_empty() {
            parts.push(OvMessagePart::new_text(truncate_middle(
                &verdict.text,
                MAX_CAPTURE_CHARS,
            )));
        }
    }
    if include_tools {
        for tool in &turn.tools {
            let output = truncate_middle(
                &config::apply_substitutions(&tool.output, capture_filters, &turn.role),
                tool_max_chars,
            );
            parts.push(OvMessagePart::Tool {
                tool_id: Some(tool.id.clone()),
                tool_name: tool.name.clone(),
                tool_status: if tool.failed {
                    "error"
                } else if tool.running {
                    "running"
                } else {
                    "completed"
                }
                .to_string(),
                tool_input: Some(shape_tool_value(
                    &tool.input,
                    capture_filters,
                    &turn.role,
                    tool_max_chars,
                )),
                tool_output: (!output.trim().is_empty()).then_some(output),
            });
        }
    }
    (!parts.is_empty()).then_some(parts)
}

fn shape_tool_value(value: &Value, filters: &[CompiledFilter], role: &str, max: usize) -> Value {
    match value {
        Value::String(s) => Value::String(truncate_middle(
            &config::apply_substitutions(s, filters, role),
            max,
        )),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| shape_tool_value(item, filters, role, max))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), shape_tool_value(v, filters, role, max)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// How far back from the end of the transcript to look for a compaction.
const CHECKPOINT_SCAN_BYTES: u64 = 512 * 1024;

/// Step index of the newest compaction `CHECKPOINT` near the end of the transcript. agy appends
/// one when it compacts; the next `PreInvocation` sees it at the tail. Only lines that mention
/// `CHECKPOINT` are parsed, so this stays cheap on every model call.
#[must_use]
pub fn latest_checkpoint_step(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(CHECKPOINT_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    if start > 0 {
        // The first line is probably cut in the middle.
        reader.read_until(b'\n', &mut line).ok()?;
    }
    let mut newest = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        if !line.windows(10).any(|w| w == b"CHECKPOINT") {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if record["type"] == "CHECKPOINT" {
            if let Some(step) = record["step_index"].as_u64() {
                newest = Some(newest.map_or(step, |n: u64| n.max(step)));
            }
        }
    }
    newest
}

/// Seeks to the trailing `64 KiB` of `transcript.jsonl` (expanding up to `2 MiB` when a large
/// intermediate tool output exceeds `64 KiB`) to extract the latest user message and its exact
/// byte offset in the transcript file.
#[must_use]
pub fn extract_latest_user_turn_tail(
    path: &Path,
    before_step: Option<u64>,
) -> Option<LatestUserTurn> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len == 0 {
        return None;
    }

    for window in [TAIL_SEEK_WINDOW_BYTES, MAX_TAIL_SCAN_BYTES] {
        if let Some(found) =
            scan_latest_user_turn_in_window(&mut file, file_len, window, before_step)
        {
            return Some(found);
        }
        if file_len <= window {
            break;
        }
    }

    None
}

fn scan_latest_user_turn_in_window(
    file: &mut File,
    file_len: u64,
    window_bytes: u64,
    before_step: Option<u64>,
) -> Option<LatestUserTurn> {
    let seek_start = file_len.saturating_sub(window_bytes);
    file.seek(SeekFrom::Start(seek_start)).ok()?;

    let mut buf = Vec::with_capacity((file_len - seek_start) as usize);
    file.read_to_end(&mut buf).ok()?;

    // Align cleanly on a complete JSONL record boundary when seeking mid-file.
    let slice_offset = if seek_start > 0 {
        let nl_idx = buf.iter().position(|&b| b == b'\n')?;
        nl_idx + 1
    } else {
        0
    };

    let mut cursor = slice_offset;
    let mut latest: Option<LatestUserTurn> = None;

    while cursor < buf.len() {
        let line_start = cursor;
        let next_nl = buf[cursor..].iter().position(|&b| b == b'\n');
        let line_end = if let Some(rel) = next_nl {
            cursor += rel + 1;
            line_start + rel
        } else {
            cursor = buf.len();
            buf.len()
        };

        let line_bytes = &buf[line_start..line_end];
        let Ok(line_str) = std::str::from_utf8(line_bytes) else {
            continue;
        };
        let trimmed = line_str.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let step_index = record["step_index"].as_u64();
        if before_step
            .zip(step_index)
            .is_some_and(|(limit, step)| limit > 0 && step >= limit)
        {
            break;
        }
        if record["source"] == "MODEL" {
            if let Some(ref mut user) = latest {
                user.answered = true;
            }
        }
        if let Some(turn) = parse_transcript_record(&record) {
            if turn.role == "user" {
                let cleaned = strip_injected_blocks(&turn.text);
                if !cleaned.is_empty() {
                    latest = Some(LatestUserTurn {
                        byte_offset: seek_start + (line_start as u64),
                        text: cleaned,
                        step_index,
                        answered: false,
                        truncated: record["truncated_fields"]
                            .as_array()
                            .is_some_and(|fields| fields.iter().any(|f| f == "content")),
                    });
                }
            }
        }
    }

    latest
}

/// Both host transcripts enumerate the same steps. Byte offsets differ if a field was shortened.
pub fn preferred_transcript(path: &Path) -> PathBuf {
    let full = path.with_file_name("transcript_full.jsonl");
    if path
        .file_name()
        .is_some_and(|name| name == "transcript.jsonl")
        && full.is_file()
    {
        full
    } else {
        path.to_owned()
    }
}

/// Map an old byte cursor between the short and full serializations at a record boundary.
pub fn translate_cursor(old: &Path, new: &Path, offset: u64) -> Result<u64> {
    let old_file = File::open(old)?;
    let mut previous = BufReader::new(old_file.take(offset));
    let mut next = BufReader::new(File::open(new)?);
    let mut buf = Vec::new();
    let mut new_offset = 0;
    while previous.read_until(b'\n', &mut buf)? > 0 {
        buf.clear();
        let size = next.read_until(b'\n', &mut buf)?;
        anyhow::ensure!(
            size > 0,
            "full transcript has not caught up with saved cursor"
        );
        new_offset += size as u64;
        buf.clear();
    }
    Ok(new_offset)
}

/// Reads finished transcript records and leaves an in-progress message for the next Stop.
///
/// With `flush_pending`, used once a conversation has gone idle, a step still waiting for
/// tool results at the end of the file is emitted with the results it has: agy will not
/// write the missing ones after a cancelled turn.
pub fn read_new_turns_from_offset(
    path: &Path,
    start_offset: u64,
    flush_pending: bool,
) -> Result<TranscriptDelta> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let effective_start = if file_len < start_offset {
        0
    } else {
        start_offset
    };

    if effective_start == file_len {
        return Ok(TranscriptDelta {
            turns: Vec::new(),
            new_byte_offset: effective_start,
        });
    }

    file.seek(SeekFrom::Start(effective_start))?;
    let mut reader = BufReader::new(file);
    let mut line_buf = Vec::new();
    let mut turns = Vec::new();
    // `consumed` counts every byte read; `committed` stops before a step whose tool results
    // are not all written yet, so the next run reads that step again together with them.
    let mut consumed = 0u64;
    let mut committed = 0u64;
    let mut pending: Option<PendingStep> = None;

    loop {
        line_buf.clear();
        let bytes_read = reader.read_until(b'\n', &mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        let complete_line = line_buf.last() == Some(&b'\n');
        let record = match serde_json::from_slice::<Value>(&line_buf) {
            Ok(record) => record,
            Err(_) if !complete_line && !line_buf.iter().all(u8::is_ascii_whitespace) => break,
            Err(_) => {
                consumed += bytes_read as u64;
                if pending.is_none() {
                    committed = consumed;
                }
                continue;
            }
        };
        let step_type = record["type"].as_str().unwrap_or_default();

        if let Some(step) = pending.as_mut() {
            if step_type == "GENERIC" {
                // agy writes each step once. A backgrounded `run_command` stays `RUNNING`
                // forever while the conversation moves on, so a running result is final once
                // anything follows it; only the last record of the file may still change.
                if is_in_progress(&record) && !flush_pending && reader.fill_buf()?.is_empty() {
                    break;
                }
                step.answer(&record);
                consumed += bytes_read as u64;
                if step.answered == step.calls.len() {
                    turns.extend(pending.take().and_then(PendingStep::into_turn));
                    committed = consumed;
                }
                continue;
            }
            if !matches!(step_type, "USER_INPUT" | "PLANNER_RESPONSE") {
                consumed += bytes_read as u64;
                continue;
            }
            // The host moved on without answering every call, as in a cancelled turn.
            turns.extend(pending.take().and_then(PendingStep::into_turn));
            committed = consumed;
        }

        let capture_step = step_type == "PLANNER_RESPONSE"
            || (step_type == "USER_INPUT"
                && record["source"]
                    .as_str()
                    .is_none_or(|s| s == "USER_EXPLICIT"));
        if capture_step && is_in_progress(&record) {
            break;
        }
        if step_type == "PLANNER_RESPONSE" && is_done(&record) {
            if let Some(step) = PendingStep::from_record(&record, effective_start + consumed) {
                pending = Some(step);
                consumed += bytes_read as u64;
                continue;
            }
        }
        if let Some(turn) = parse_transcript_record(&record) {
            turns.push(turn);
        }
        consumed += bytes_read as u64;
        committed = consumed;
    }

    if flush_pending {
        if let Some(step) = pending.take() {
            turns.extend(step.into_turn());
            committed = consumed;
        }
    }

    Ok(TranscriptDelta {
        turns,
        new_byte_offset: effective_start + committed,
    })
}

fn is_in_progress(record: &Value) -> bool {
    matches!(
        record["status"].as_str(),
        Some("PENDING" | "QUEUED" | "WAITING" | "RUNNING" | "GENERATING")
    )
}

fn is_done(record: &Value) -> bool {
    record["status"]
        .as_str()
        .is_none_or(|status| status == "DONE")
}

/// A `PLANNER_RESPONSE` step waiting for the `GENERIC` results of its tool calls.
struct PendingStep {
    text: String,
    calls: Vec<PendingCall>,
    answered: usize,
    tools: Vec<RawToolUse>,
}

struct PendingCall {
    id: String,
    name: String,
    input: Value,
    /// `finish` ends the turn; its `result` is already part of the text.
    emit: bool,
}

impl PendingStep {
    fn from_record(record: &Value, offset: u64) -> Option<Self> {
        let calls = record["tool_calls"].as_array().filter(|c| !c.is_empty())?;
        let step = record["step_index"]
            .as_u64()
            .map_or_else(|| format!("b{offset}"), |i| i.to_string());
        let mut text = record
            .as_object()
            .and_then(extract_text_from_record)
            .unwrap_or_default();
        let mut pending_calls = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            let name = call["name"].as_str().unwrap_or("tool");
            let args = call.get("args").cloned().unwrap_or(Value::Null);
            let id = format!("agy-{step}-{index}");
            if name == "finish" {
                if let Some(result) = finish_result_text(&args) {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&result);
                }
                pending_calls.push(PendingCall {
                    id,
                    name: name.to_string(),
                    input: Value::Null,
                    emit: false,
                });
                continue;
            }
            let (name, input) = if name == "call_mcp_tool" {
                mcp_tool_identity(&args)
            } else {
                (name.to_string(), args)
            };
            pending_calls.push(PendingCall {
                id,
                name,
                input,
                emit: true,
            });
        }
        Some(Self {
            text,
            calls: pending_calls,
            answered: 0,
            tools: Vec::new(),
        })
    }

    fn answer(&mut self, record: &Value) {
        let Some(call) = self.calls.get(self.answered) else {
            return;
        };
        self.answered += 1;
        if !call.emit {
            return;
        }
        let failed = matches!(record["status"].as_str(), Some("ERROR" | "CANCELED"))
            || record["error"]
                .as_str()
                .is_some_and(|e| !e.trim().is_empty());
        let body = strip_step_timestamps(record["content"].as_str().unwrap_or_default());
        let output = if body.trim().is_empty() {
            record["error"].as_str().unwrap_or_default().to_string()
        } else {
            body
        };
        self.tools.push(RawToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            output,
            failed,
            running: !failed && is_in_progress(record),
        });
    }

    fn into_turn(self) -> Option<RawTranscriptTurn> {
        if self.text.trim().is_empty() && self.tools.is_empty() {
            return None;
        }
        Some(RawTranscriptTurn {
            role: "assistant".to_string(),
            text: self.text,
            tools: self.tools,
        })
    }
}

/// `finish(result=...)` carries the final answer of structured-output and subagent runs,
/// which write no other text.
fn finish_result_text(args: &Value) -> Option<String> {
    match args.get("result")? {
        Value::Null => None,
        Value::String(s) => (!s.trim().is_empty()).then(|| s.trim().to_string()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

/// `call_mcp_tool` wraps every MCP call. `OpenViking`'s own tools keep their bare names
/// (`find`, `read`, ...) so the server recognizes them; other servers are prefixed.
fn mcp_tool_identity(args: &Value) -> (String, Value) {
    let server = args["ServerName"].as_str().unwrap_or_default();
    let tool = args["ToolName"].as_str().unwrap_or("call_mcp_tool");
    let input = args
        .get("Arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let name = if server.is_empty() || server.to_ascii_lowercase().contains("openviking") {
        tool.to_string()
    } else {
        format!("{server}/{tool}")
    };
    (name, input)
}

/// `GENERIC` results start with `Created At:` / `Completed At:` lines before the output.
fn strip_step_timestamps(content: &str) -> String {
    let mut rest = content;
    loop {
        let line_end = rest.find('\n').unwrap_or(rest.len());
        let line = rest[..line_end].trim();
        if line.starts_with("Created At:") || line.starts_with("Completed At:") {
            rest = rest.get(line_end + 1..).unwrap_or_default();
        } else {
            return rest.trim().to_string();
        }
    }
}

fn parse_transcript_record(obj: &Value) -> Option<RawTranscriptTurn> {
    if obj["status"]
        .as_str()
        .is_some_and(|status| status != "DONE")
    {
        return None;
    }
    let map = obj.as_object()?;

    if let Some(source) = map.get("source").and_then(Value::as_str) {
        if source.eq_ignore_ascii_case("SYSTEM") {
            return None;
        }
    }

    if let Some(step_type) = map.get("type").and_then(Value::as_str) {
        match step_type {
            "USER_INPUT" => {
                if map
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "USER_EXPLICIT")
                {
                    return None;
                }
                let text = extract_text_from_record(map)?;
                return Some(RawTranscriptTurn {
                    role: "user".to_string(),
                    text,
                    tools: Vec::new(),
                });
            }
            "PLANNER_RESPONSE" => {
                let text = extract_text_from_record(map)?;
                return Some(RawTranscriptTurn {
                    role: "assistant".to_string(),
                    text,
                    tools: Vec::new(),
                });
            }
            "EPHEMERAL_MESSAGE" | "SYSTEM_MESSAGE" | "GENERIC" | "TOOL_RESULT" => {
                return None;
            }
            _ => {}
        }
    }

    let role_raw = map.get("role").and_then(Value::as_str).or_else(|| {
        map.get("message")
            .and_then(Value::as_object)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
    })?;

    let normalized_role = if role_raw.eq_ignore_ascii_case("user")
        || role_raw.eq_ignore_ascii_case("human")
    {
        "user"
    } else if role_raw.eq_ignore_ascii_case("assistant") || role_raw.eq_ignore_ascii_case("model") {
        "assistant"
    } else {
        return None;
    };

    let text = extract_text_from_record(map).or_else(|| {
        map.get("message")
            .and_then(Value::as_object)
            .and_then(extract_text_from_record)
    })?;

    Some(RawTranscriptTurn {
        role: normalized_role.to_string(),
        text,
        tools: Vec::new(),
    })
}

fn extract_text_from_record(map: &Map<String, Value>) -> Option<String> {
    if let Some(content_val) = map.get("content") {
        if let Some(s) = extract_text_from_value(content_val) {
            return Some(s);
        }
    }
    if let Some(parts_val) = map.get("parts") {
        if let Some(s) = extract_text_from_value(parts_val) {
            return Some(s);
        }
    }
    if let Some(text_val) = map.get("text") {
        if let Some(s) = extract_text_from_value(text_val) {
            return Some(s);
        }
    }
    None
}

fn extract_text_from_value(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(items) => {
            let mut segments = Vec::new();
            for item in items {
                match item {
                    Value::String(s) if !s.trim().is_empty() => {
                        segments.push(s.trim().to_string());
                    }
                    Value::Object(part_map) => {
                        let part_type = part_map
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("text");
                        if matches!(part_type, "text" | "input_text" | "output_text") {
                            if let Some(t) = part_map.get("text").and_then(Value::as_str) {
                                if !t.trim().is_empty() {
                                    segments.push(t.trim().to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if segments.is_empty() {
                None
            } else {
                Some(segments.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn strips_all_openviking_injected_blocks() {
        let dirty = "<openviking-context>\nsecret-old-memory\n</openviking-context>\n\
                     <openviking-session>healthy</openviking-session>\n\
                     <available-skills>- skill-a</available-skills>\n\
                     Actual user question about architecture.";
        let turn = RawTranscriptTurn {
            role: "user".to_string(),
            text: dirty.to_string(),
            tools: Vec::new(),
        };
        let parts = shape_turn_parts(&turn, &[], MAX_CAPTURE_CHARS, true);
        assert_eq!(
            parts,
            Some(vec![OvMessagePart::new_text(
                "Actual user question about architecture.".to_string()
            )]),
            "all injected OpenViking tags, including old recalled context, must be stripped"
        );
    }

    #[test]
    fn tail_seek_handles_files_larger_than_64kib() {
        let mut tmp = NamedTempFile::new().expect("temp transcript file should be created");
        for i in 0..1000 {
            writeln!(
                tmp,
                r#"{{"step_index":{i},"type":"PLANNER_RESPONSE","content":"padding line {i:060}"}}"#
            )
            .expect("padding JSONL line should write");
        }
        writeln!(
            tmp,
            r#"{{"step_index":1001,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nFinal tail prompt\n</USER_REQUEST>"}}"#
        )
        .expect("final user JSONL line should write");
        tmp.flush().expect("temp transcript should flush");

        let meta_len = tmp
            .as_file()
            .metadata()
            .expect("metadata should be readable")
            .len();
        assert!(
            meta_len > TAIL_SEEK_WINDOW_BYTES,
            "test fixture must exceed 64 KiB to exercise tail seeking, got {meta_len}"
        );

        let turn = extract_latest_user_turn_tail(tmp.path(), None)
            .expect("latest user turn should be found in trailing 64 KiB");
        assert_eq!(
            turn.text, "Final tail prompt",
            "tail seek should unwrap USER_REQUEST accurately"
        );
        assert!(
            turn.byte_offset > 0,
            "byte_offset should record exact file position, got {}",
            turn.byte_offset
        );
    }

    #[test]
    fn tail_seek_expands_past_large_intermediate_tool_result() {
        let mut tmp = NamedTempFile::new().expect("temp transcript file should be created");
        writeln!(
            tmp,
            r#"{{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nAudit large file handling\n</USER_REQUEST>"}}"#
        )
        .expect("initial user turn should write");
        tmp.flush().expect("initial user turn should flush");

        let initial_turn = extract_latest_user_turn_tail(tmp.path(), None)
            .expect("initial user turn must be found");

        // Append a > 80 KiB TOOL_RESULT line so the trailing 64 KiB window contains no USER_INPUT.
        let large_blob = "x".repeat(80 * 1024);
        writeln!(
            tmp,
            r#"{{"step_index":2,"type":"TOOL_RESULT","content":"{large_blob}"}}"#
        )
        .expect("large tool result line should write");
        tmp.flush().expect("large tool result should flush");

        let after_large_tool = extract_latest_user_turn_tail(tmp.path(), None)
            .expect("expanded tail scan must still locate prior user turn");
        assert_eq!(
            after_large_tool, initial_turn,
            "byte_offset and text must remain identical across >64 KiB intermediate tool outputs"
        );
    }

    fn write_lines(lines: &[serde_json::Value]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp transcript");
        for line in lines {
            writeln!(file, "{line}").expect("write transcript line");
        }
        file
    }

    #[test]
    fn pairs_tool_calls_with_their_generic_results_in_order() {
        use serde_json::json;
        let file = write_lines(&[
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\nFix the build\n</USER_REQUEST>"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Checking.","tool_calls":[
                {"name":"read_file","args":{"Path":"/repo/Cargo.toml"}},
                {"name":"call_mcp_tool","args":{"ServerName":"openviking_openviking","ToolName":"find","Arguments":{"query":"build"}}},
                {"name":"call_mcp_tool","args":{"ServerName":"codebase-memory-mcp","ToolName":"search_code","Arguments":{"q":"main"}}}
            ]}),
            json!({"step_index":2,"type":"GENERIC","status":"DONE","content":"Created At: 2026-11-24T22:37:06+08:00\nCompleted At: 2026-11-24T22:37:13+08:00\n[package]\nname = \"demo\""}),
            json!({"step_index":3,"type":"GENERIC","status":"DONE","content":"Created At: x\nCompleted At: y\nCreated At: x\nCompleted At: y\nresult: 2 hits"}),
            json!({"step_index":4,"type":"GENERIC","status":"ERROR","error":"boom","content":"Created At: x\nCompleted At: y\nEncountered error in step execution: boom"}),
            json!({"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"finish","args":{"result":{"fixed":true}}}]}),
            json!({"step_index":6,"type":"GENERIC","status":"DONE","content":"Created At: x\nCompleted At: y\nok"}),
        ]);
        let delta = read_new_turns_from_offset(file.path(), 0, false).expect("read transcript");
        assert_eq!(
            delta.new_byte_offset,
            std::fs::metadata(file.path()).unwrap().len()
        );
        assert_eq!(delta.turns.len(), 3, "{:#?}", delta.turns);

        let tools = &delta.turns[1].tools;
        assert_eq!(delta.turns[1].text, "Checking.");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["read_file", "find", "codebase-memory-mcp/search_code"]
        );
        assert_eq!(tools[0].id, "agy-1-0");
        assert_eq!(tools[0].output, "[package]\nname = \"demo\"");
        assert_eq!(tools[1].input, json!({"query":"build"}));
        assert_eq!(tools[1].output, "result: 2 hits");
        assert!(tools[2].failed);
        assert_eq!(tools[2].output, "Encountered error in step execution: boom");

        let finish = &delta.turns[2];
        assert!(
            finish.tools.is_empty(),
            "finish is control flow, not a tool part"
        );
        assert!(finish.text.contains("\"fixed\": true"), "{}", finish.text);
    }

    #[test]
    fn a_backgrounded_command_that_stays_running_does_not_stall_capture() {
        use serde_json::json;
        // Seen in real transcripts: 404 `RUNNING` results that agy never rewrites.
        let file = write_lines(&[
            json!({"step_index":0,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
                {"name":"run_command","args":{"CommandLine":"cargo build","WaitMsBeforeAsync":500}}
            ]}),
            json!({"step_index":1,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nCommand is running in the background"}),
            json!({"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Started the build."}),
            json!({"step_index":3,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"next question"}),
        ]);
        let delta = read_new_turns_from_offset(file.path(), 0, false).expect("read transcript");
        assert_eq!(
            delta.new_byte_offset,
            std::fs::metadata(file.path()).unwrap().len(),
            "capture must continue past a result that stays running"
        );
        assert_eq!(delta.turns.len(), 3);
        assert!(delta.turns[0].tools[0].running);
        assert_eq!(
            delta.turns[0].tools[0].output,
            "Command is running in the background"
        );
        let parts = shape_turn_parts(&delta.turns[0], &[], MAX_CAPTURE_CHARS, true).unwrap();
        assert!(matches!(
            &parts[0],
            OvMessagePart::Tool { tool_status, .. } if tool_status == "running"
        ));
    }

    #[test]
    fn holds_the_cursor_before_a_step_whose_results_are_not_written() {
        use serde_json::json;
        let user = json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"go"});
        let step = json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
            {"name":"run_command","args":{"CommandLine":"cargo test"}},
            {"name":"read_file","args":{"Path":"/repo/a.rs"}}
        ]});
        let first =
            json!({"step_index":2,"type":"GENERIC","status":"DONE","content":"Created At: x\nok"});
        let running = json!({"step_index":3,"type":"GENERIC","status":"RUNNING","content":""});
        let file = write_lines(&[user.clone(), step.clone(), first.clone(), running]);
        let user_len = format!("{user}\n").len() as u64;

        let delta = read_new_turns_from_offset(file.path(), 0, false).expect("read transcript");
        assert_eq!(delta.turns.len(), 1, "only the user turn is final");
        assert_eq!(
            delta.new_byte_offset, user_len,
            "cursor must stop before the step"
        );

        let unanswered = write_lines(&[user.clone(), step.clone(), first.clone()]);
        let flushed =
            read_new_turns_from_offset(unanswered.path(), user_len, true).expect("read transcript");
        assert_eq!(
            flushed.turns.len(),
            1,
            "an idle conversation emits the partial step"
        );
        assert_eq!(flushed.turns[0].tools.len(), 1);
        assert_eq!(
            flushed.new_byte_offset,
            std::fs::metadata(unanswered.path()).unwrap().len()
        );

        let done = json!({"step_index":3,"type":"GENERIC","status":"DONE","content":"Created At: x\nfn a() {}"});
        let file = write_lines(&[user, step, first, done]);
        let delta =
            read_new_turns_from_offset(file.path(), user_len, false).expect("read transcript");
        assert_eq!(delta.turns.len(), 1);
        assert_eq!(delta.turns[0].tools.len(), 2);
        assert_eq!(delta.turns[0].tools[1].output, "fn a() {}");
    }

    #[test]
    fn tool_parts_get_substitutions_and_a_dropped_turn_takes_its_tools() {
        use serde_json::json;
        let rules = config::compile_input_filters(&[
            "s/sk-[a-z0-9]+/[redacted]/g".to_string(),
            "d/^ignore me/".to_string(),
        ]);
        let tool = RawToolUse {
            id: "agy-1-0".into(),
            name: "run_command".into(),
            input: json!({"CommandLine":"curl -H 'Authorization: sk-abc123' x"}),
            output: "token sk-abc123 accepted".into(),
            failed: false,
            running: false,
        };
        let turn = RawTranscriptTurn {
            role: "assistant".into(),
            text: String::new(),
            tools: vec![tool.clone()],
        };
        assert_eq!(
            shape_turn_parts(&turn, &rules, MAX_CAPTURE_CHARS, true),
            Some(vec![OvMessagePart::Tool {
                tool_id: Some("agy-1-0".into()),
                tool_name: "run_command".into(),
                tool_status: "completed".into(),
                tool_input: Some(json!({"CommandLine": "curl -H 'Authorization: [redacted]' x"})),
                tool_output: Some("token [redacted] accepted".into()),
            }]),
            "redaction rules must also cover tool inputs and outputs"
        );
        assert!(shape_turn_parts(&turn, &rules, MAX_CAPTURE_CHARS, false).is_none());

        let dropped = RawTranscriptTurn {
            role: "assistant".into(),
            text: "ignore me please".into(),
            tools: vec![tool],
        };
        assert!(shape_turn_parts(&dropped, &rules, MAX_CAPTURE_CHARS, true).is_none());
    }

    #[test]
    fn truncation_keeps_head_and_tail_with_a_marker() {
        let text: String = "a".repeat(30) + &"b".repeat(40) + &"c".repeat(30);
        let out = truncate_middle(&text, 40);
        assert!(out.starts_with(&"a".repeat(30)));
        assert!(out.ends_with(&"c".repeat(10)));
        assert!(
            out.contains("[… 60 characters omitted by ov-agy …]"),
            "{out}"
        );
        assert_eq!(truncate_middle("short", 40), "short");
    }
}
