//! Antigravity `transcript.jsonl` tail reader, incremental cursor reader, and sanitizer.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde_json::{Map, Value};

use crate::config::{self, CompiledFilter};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTranscriptTurn {
    pub role: String,
    pub text: String,
}

/// Incremental batch of turns and updated file byte cursor read from `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Sanitizes a captured turn prior to sending to `OpenViking` `/sessions/{id}/messages`:
/// 1. Strips injected `<openviking-context>`, `<openviking-session>`, and `<available-skills>` blocks.
/// 2. Applies `captureFilters` regex rules.
/// 3. Truncates safely on a UTF-8 character boundary to `max_chars`.
#[must_use]
pub fn sanitize_turn_text(
    raw: &str,
    capture_filters: &[CompiledFilter],
    role: &str,
    max_chars: usize,
) -> Option<String> {
    let stripped = strip_injected_blocks(raw);
    if stripped.is_empty() {
        return None;
    }

    let verdict = config::apply_input_filters(&stripped, capture_filters, role);
    if verdict.dropped || verdict.text.is_empty() {
        return None;
    }

    let mut out = verdict.text;
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect::<String>();
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
pub fn read_new_turns_from_offset(path: &Path, start_offset: u64) -> Result<TranscriptDelta> {
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
    let mut consumed = 0u64;

    loop {
        line_buf.clear();
        let bytes_read = reader.read_until(b'\n', &mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        let complete_line = line_buf.last() == Some(&b'\n');
        match serde_json::from_slice::<Value>(&line_buf) {
            Ok(record) => {
                let capture_step = record["type"] == "PLANNER_RESPONSE"
                    || (record["type"] == "USER_INPUT"
                        && record["source"]
                            .as_str()
                            .is_none_or(|s| s == "USER_EXPLICIT"));
                if capture_step
                    && matches!(
                        record["status"].as_str(),
                        Some("PENDING" | "QUEUED" | "WAITING" | "RUNNING" | "GENERATING")
                    )
                {
                    break;
                }
                if let Some(turn) = parse_transcript_record(&record) {
                    turns.push(turn);
                }
            }
            Err(_) if !complete_line && !line_buf.iter().all(u8::is_ascii_whitespace) => break,
            Err(_) => {}
        }
        consumed += bytes_read as u64;
    }

    Ok(TranscriptDelta {
        turns,
        new_byte_offset: effective_start + consumed,
    })
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
                });
            }
            "PLANNER_RESPONSE" => {
                let text = extract_text_from_record(map)?;
                return Some(RawTranscriptTurn {
                    role: "assistant".to_string(),
                    text,
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
        let clean =
            sanitize_turn_text(dirty, &[], "user", 24_000).expect("clean text should remain");
        assert_eq!(
            clean, "Actual user question about architecture.",
            "all injected OpenViking tags must be stripped cleanly"
        );
        assert!(
            !clean.contains("secret-old-memory"),
            "old recalled context must never survive sanitization"
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
}
