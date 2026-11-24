//! Synchronous `< 1ms` `PreToolUse` safety guard blocking local access to `viking://` URIs.

use serde_json::{json, Value};

use crate::config;
use crate::wire::{PreToolInput, PreToolOutput};

/// Explanatory message returned to `agy` when a local filesystem tool targets `viking://`.
pub const VIKING_DENY_REASON: &str = "Paths starting with viking:// live in the OpenViking virtual filesystem. Use OpenViking MCP tools (read, ls, tree, search, add_skill) instead of local file tools.";

/// Evaluates a `PreToolUse` payload synchronously without allocating an async runtime.
#[must_use]
pub fn run_sync(raw_stdin: &str) -> PreToolOutput {
    let parsed: PreToolInput = match serde_json::from_str(raw_stdin) {
        Ok(val) => val,
        Err(err) => {
            config::debug_log_from_env(
                "hook.pre_tool",
                "parse_error",
                json!({ "error": err.to_string() }),
            );
            // Never return `{}` on `PreToolUse`; fall back to `ask` if stdin JSON is malformed.
            return PreToolOutput::ask();
        }
    };

    let tool_name = parsed.effective_tool_name();

    if !config::is_env_explicitly_disabled() {
        let has_viking_in_call = parsed
            .tool_call
            .as_ref()
            .is_some_and(|tc| contains_viking_uri(&tc.args));
        let has_viking_in_flat = contains_viking_uri(&parsed.tool_input);

        if has_viking_in_call || has_viking_in_flat {
            config::debug_log_from_env(
                "hook.pre_tool",
                "denied_viking_uri",
                json!({ "tool": tool_name }),
            );
            return PreToolOutput::deny(VIKING_DENY_REASON);
        }
    }

    if is_read_only_local_tool(tool_name) {
        PreToolOutput::allow()
    } else {
        PreToolOutput::ask()
    }
}

fn is_read_only_local_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "view_file" | "list_dir" | "grep_search" | "read_file" | "find_by_name"
    )
}

/// Recursively inspects all string values inside `value` for `"viking://"`.
#[must_use]
pub fn contains_viking_uri(value: &Value) -> bool {
    match value {
        Value::String(s) => contains_ascii_case_insensitive(s, "viking://"),
        Value::Array(arr) => arr.iter().any(contains_viking_uri),
        Value::Object(map) => map.values().any(contains_viking_uri),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn contains_ascii_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    let n_bytes = needle_lower.as_bytes();
    let h_bytes = haystack.as_bytes();
    if n_bytes.is_empty() || h_bytes.len() < n_bytes.len() {
        return false;
    }
    h_bytes
        .windows(n_bytes.len())
        .any(|win| win.eq_ignore_ascii_case(n_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_nested_viking_uri_and_allows_or_asks_local_paths() {
        let deny_out = run_sync(
            r#"{"toolCall":{"id":"1","name":"view_file","args":{"Path":"viking://~/memories/foo.md"}}}"#,
        );
        assert_eq!(
            deny_out,
            PreToolOutput::deny(VIKING_DENY_REASON),
            "viking:// URI must be denied on local file tools"
        );

        let allow_out = run_sync(
            r#"{"toolCall":{"id":"2","name":"view_file","args":{"Path":"/tmp/local.md"}}}"#,
        );
        assert_eq!(
            allow_out,
            PreToolOutput::allow(),
            "read-only local tool should return decision=allow"
        );

        let ask_out = run_sync(
            r#"{"toolCall":{"id":"3","name":"write_to_file","args":{"TargetFile":"/tmp/out.md","CodeContent":"ok"}}}"#,
        );
        assert_eq!(
            ask_out,
            PreToolOutput::ask(),
            "mutating local tool should preserve user permission policy via decision=ask"
        );

        let cli_read_deny = run_sync(
            r#"{"toolCall":{"id":"4","name":"read_file","args":{"Path":"viking://resources/spec.md"}}}"#,
        );
        assert_eq!(
            cli_read_deny,
            PreToolOutput::deny(VIKING_DENY_REASON),
            "CLI read_file targeting viking:// must be denied"
        );

        let cli_edit_deny = run_sync(
            r#"{"toolCall":{"id":"5","name":"edit_file","args":{"Path":"viking://~/memories/note.md","Target":"a","Replacement":"b"}}}"#,
        );
        assert_eq!(
            cli_edit_deny,
            PreToolOutput::deny(VIKING_DENY_REASON),
            "CLI edit_file targeting viking:// must be denied"
        );

        let cli_write_ask = run_sync(
            r#"{"toolCall":{"id":"6","name":"write_file","args":{"Path":"/tmp/out.md","Content":"ok"}}}"#,
        );
        assert_eq!(
            cli_write_ask,
            PreToolOutput::ask(),
            "CLI write_file on local path should return decision=ask"
        );
    }
}
