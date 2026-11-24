//! Synchronous `< 1ms` `PreToolUse` safety guard blocking local access to `viking://` URIs.
//!
//! agy 1.5.16 treats a missing or empty `decision` as a denial and fails the tool call when
//! the hook times out, so every path returns an explicit decision. `ask` is the neutral
//! answer: it hands the call to the user's normal permission flow (`always-proceed`, Auto
//! mode and saved rules all behave as if no hook ran), while `allow` would auto-approve and
//! bypass the user's own `ask` rules.

use serde_json::{json, Value};

use crate::config;
use crate::wire::{PreToolInput, PreToolOutput};

/// Explanatory message returned to `agy` when a local filesystem tool targets `viking://`.
pub const VIKING_DENY_REASON: &str = "Paths starting with viking:// are OpenViking URIs, not local files. Use the OpenViking MCP tools instead: read, list, tree, glob, grep, find or search (save a skill with add_skill).";

/// Evaluates a `PreToolUse` payload synchronously without allocating an async runtime.
#[must_use]
pub fn run_sync(raw_stdin: &str) -> PreToolOutput {
    if config::is_env_explicitly_disabled() {
        return PreToolOutput::ask();
    }

    let parsed: PreToolInput = match serde_json::from_str(raw_stdin) {
        Ok(val) => val,
        Err(err) => {
            config::debug_log_from_env(
                "hook.pre_tool",
                "parse_error",
                json!({ "error": err.to_string() }),
            );
            return PreToolOutput::ask();
        }
    };

    let has_viking_in_call = parsed
        .tool_call
        .as_ref()
        .is_some_and(|tc| contains_viking_uri(&tc.args));
    if has_viking_in_call || contains_viking_uri(&parsed.tool_input) {
        config::debug_log_from_env(
            "hook.pre_tool",
            "denied_viking_uri",
            json!({ "tool": parsed.effective_tool_name() }),
        );
        return PreToolOutput::deny(VIKING_DENY_REASON);
    }

    PreToolOutput::ask()
}

/// Only location fields address a filesystem object. Text and search patterns may quote a URI.
fn contains_viking_uri(args: &Value) -> bool {
    args.as_object().is_some_and(|map| {
        map.iter().any(|(key, value)| {
            let field = key.to_ascii_lowercase();
            if ![
                "path",
                "file",
                "directory",
                "folder",
                "paths",
                "directories",
            ]
            .iter()
            .any(|suffix| field.ends_with(suffix))
            {
                return false;
            }
            let is_uri = |v: &Value| {
                v.as_str().is_some_and(|s| {
                    s.trim_start()
                        .get(.."viking://".len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("viking://"))
                })
            };
            match value {
                Value::Array(values) => values.iter().any(is_uri),
                other => is_uri(other),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_viking_uris_and_defers_everything_else_to_the_user_policy() {
        for raw in [
            r#"{"toolCall":{"id":"1","name":"view_file","args":{"AbsolutePath":"viking://~/memories/foo.md"}}}"#,
            r#"{"toolCall":{"id":"4","name":"read_file","args":{"Path":"viking://resources/spec.md"}}}"#,
            r#"{"toolCall":{"id":"5","name":"edit_file","args":{"Path":"viking://~/memories/note.md","Target":"a","Replacement":"b"}}}"#,
            r#"{"toolName":"read_file","toolInput":{"Path":"VIKING://~/skills"}}"#,
        ] {
            assert_eq!(
                run_sync(raw),
                PreToolOutput::deny(VIKING_DENY_REASON),
                "viking:// location must be denied: {raw}"
            );
        }

        for raw in [
            r#"{"toolCall":{"id":"2","name":"view_file","args":{"Path":"/tmp/local.md"}}}"#,
            r#"{"toolCall":{"id":"2","name":"read_file","args":{"Path":"/Users/dev/.ssh/config"}}}"#,
            r#"{"toolCall":{"id":"3","name":"write_to_file","args":{"TargetFile":"/tmp/out.md","CodeContent":"ok"}}}"#,
            r#"{"toolCall":{"id":"6","name":"write_file","args":{"Path":"/tmp/out.md","Content":"ok"}}}"#,
            r#"{"toolName":"write_file","toolInput":{"Path":"/tmp/out.md","Content":"viking://~/skills"}}"#,
            r#"{"toolName":"grep_search","toolInput":{"SearchPath":"/tmp","Query":"viking://resources"}}"#,
            "not json",
        ] {
            assert_eq!(
                run_sync(raw),
                PreToolOutput::ask(),
                "local or unparseable calls must keep the user's permission flow: {raw}"
            );
        }
    }

    #[test]
    fn deny_reason_names_only_tools_the_server_exposes() {
        for tool in [
            "read",
            "list",
            "tree",
            "glob",
            "grep",
            "find",
            "search",
            "add_skill",
        ] {
            assert!(VIKING_DENY_REASON.contains(tool), "missing {tool}");
        }
        assert!(!VIKING_DENY_REASON.contains(" ls"));
    }
}
