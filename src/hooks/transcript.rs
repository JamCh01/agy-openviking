//! Antigravity `transcript.jsonl` tail reader, incremental cursor reader, and sanitizer.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde_json::{Map, Value};

use crate::config::{self, CompiledFilter};

/// Trailing byte window (`64 KiB`) read by `PreInvocation` to avoid scanning multi-MB transcripts.
pub const TAIL_SEEK_WINDOW_BYTES: u64 = 64 * 1024;

/// Latest user turn located near the tail of `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestUserTurn {
    pub byte_offset: u64,
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

fn re_ov_context() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<openviking-context\b[^>]*>.*?</openviking-context>")
            .expect("static openviking-context regex literal must be valid")
    })
}

fn re_ov_session() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<openviking-session\b[^>]*>.*?</openviking-session>")
            .expect("static openviking-session regex literal must be valid")
    })
}

fn re_available_skills() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<available-skills\b[^>]*>.*?</available-skills>")
            .expect("static available-skills regex literal must be valid")
    })
}

fn re_additional_metadata() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<additional_metadata\b[^>]*>.*?</additional_metadata>")
            .expect("static additional_metadata regex literal must be valid")
    })
}

fn re_user_settings_change() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<user_settings_change\b[^>]*>.*?</user_settings_change>")
            .expect("static user_settings_change regex literal must be valid")
    })
}

fn re_user_request_unwrap() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<user_request\b[^>]*>(.*?)</user_request>")
            .expect("static user_request regex literal must be valid")
    })
}

/// Strips injected OpenViking XML blocks (`<openviking-context>`, `<openviking-session>`,
/// `<available-skills>`) and Antigravity system metadata wrappers from text.
#[must_use]
pub fn strip_injected_blocks(raw: &str) -> String {
    let mut cleaned = re_ov_context().replace_all(raw, "").into_owned();
    cleaned = re_ov_session().replace_all(&cleaned, "").into_owned();
    cleaned = re_available_skills().replace_all(&cleaned, "").into_owned();
    cleaned = re_additional_metadata()
        .replace_all(&cleaned, "")
        .into_owned();
    cleaned = re_user_settings_change()
        .replace_all(&cleaned, "")
        .into_owned();

    if let Some(caps) = re_user_request_unwrap().captures(&cleaned) {
        if let Some(inner) = caps.get(1) {
            let rest = re_user_request_unwrap().replace_all(&cleaned, "");
            if rest.trim().is_empty() {
                cleaned = inner.as_str().to_string();
            } else {
                let inner_trimmed = inner.as_str().trim();
                let rest_trimmed = rest.trim();
                cleaned = format!("{inner_trimmed}\n{rest_trimmed}");
            }
        }
    }

    cleaned.trim().to_string()
}

/// Sanitizes a captured turn prior to sending to OpenViking `/sessions/{id}/messages`:
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

/// Seeks only to the trailing `64 KiB` of `transcript.jsonl` to extract the latest user message
/// and its exact byte offset in the transcript file.
#[must_use]
pub fn extract_latest_user_turn_tail(path: &Path) -> Option<LatestUserTurn> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len == 0 {
        return None;
    }

    let seek_start = file_len.saturating_sub(TAIL_SEEK_WINDOW_BYTES);
    file.seek(SeekFrom::Start(seek_start)).ok()?;

    let mut buf = Vec::with_capacity((file_len - seek_start) as usize);
    file.read_to_end(&mut buf).ok()?;

    // Align cleanly on a complete JSONL record boundary when seeking mid-file.
    let slice_offset = if seek_start > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            Some(nl_idx) => nl_idx + 1,
            None => 0,
        }
    } else {
        0
    };

    let mut cursor = slice_offset;
    let mut latest: Option<LatestUserTurn> = None;

    while cursor < buf.len() {
        let line_start = cursor;
        let next_nl = buf[cursor..].iter().position(|&b| b == b'\n');
        let line_end = match next_nl {
            Some(rel) => {
                cursor += rel + 1;
                line_start + rel
            }
            None => {
                cursor = buf.len();
                buf.len()
            }
        };

        let line_bytes = &buf[line_start..line_end];
        let Ok(line_str) = std::str::from_utf8(line_bytes) else {
            continue;
        };
        let trimmed = line_str.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(turn) = parse_transcript_json_line(trimmed) {
            if turn.role == "user" {
                let cleaned = strip_injected_blocks(&turn.text);
                if !cleaned.is_empty() {
                    latest = Some(LatestUserTurn {
                        byte_offset: seek_start + (line_start as u64),
                        text: cleaned,
                    });
                }
            }
        }
    }

    latest
}

/// Reads new user and assistant turns from `transcript.jsonl` starting at `start_offset`.
/// Handles file truncation/compaction safely by resetting to byte `0` if shrunk.
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
    let mut buf = Vec::with_capacity((file_len - effective_start) as usize);
    file.read_to_end(&mut buf)?;

    let mut turns = Vec::new();
    let mut consumed = 0usize;
    let mut cursor = 0usize;

    while cursor < buf.len() {
        let line_start = cursor;
        if let Some(rel_nl) = buf[cursor..].iter().position(|&b| b == b'\n') {
            let line_end = line_start + rel_nl;
            cursor = line_end + 1;
            consumed = cursor;

            if let Ok(line_str) = std::str::from_utf8(&buf[line_start..line_end]) {
                let trimmed = line_str.trim();
                if !trimmed.is_empty() {
                    if let Some(turn) = parse_transcript_json_line(trimmed) {
                        turns.push(turn);
                    }
                }
            }
        } else {
            if let Ok(line_str) = std::str::from_utf8(&buf[line_start..]) {
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    consumed = buf.len();
                } else if serde_json::from_str::<Value>(trimmed).is_ok() {
                    if let Some(turn) = parse_transcript_json_line(trimmed) {
                        turns.push(turn);
                    }
                    consumed = buf.len();
                }
            }
            break;
        }
    }

    Ok(TranscriptDelta {
        turns,
        new_byte_offset: effective_start + (consumed as u64),
    })
}

fn parse_transcript_json_line(line: &str) -> Option<RawTranscriptTurn> {
    let obj = serde_json::from_str::<Value>(line).ok()?;
    let map = obj.as_object()?;

    if let Some(source) = map.get("source").and_then(Value::as_str) {
        if source.eq_ignore_ascii_case("SYSTEM") {
            return None;
        }
    }

    if let Some(step_type) = map.get("type").and_then(Value::as_str) {
        match step_type {
            "USER_INPUT" => {
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

        let turn = extract_latest_user_turn_tail(tmp.path())
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
}
