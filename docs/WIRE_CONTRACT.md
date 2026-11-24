# OpenViking & Antigravity CLI Wire Contract (`ov-agy`)

This document locks the verified public HTTP REST, MCP Streamable HTTP, configuration, and Antigravity CLI (`agy`) hook stdin/stdout wire contracts implemented by `ov-agy`.

`ov-agy` is a clean-room external client licensed under Apache-2.0 communicating strictly over OpenViking's documented public HTTP/MCP interfaces and Antigravity CLI's plugin/hook stdio JSON protocols.

---

## 1. OpenViking HTTP Headers & Envelope

### Outbound Request Headers

| Header | Condition | Value |
| :--- | :--- | :--- |
| `Content-Type` | JSON requests | `application/json` |
| `Authorization` | `api_key` non-empty | `Bearer <api_key>` (the key is never duplicated into `X-API-Key`) |
| `X-OpenViking-Account` | `authMode` is `trusted` and `account` non-empty | `<account>` |
| `X-OpenViking-User` | `authMode` is `trusted` and `user` non-empty | `<user>` |
| `X-OpenViking-Actor-Peer` | `peer` non-empty; on `/mcp` only when `recall.peer_scope` is `actor` | `<peer_id>` |
| `User-Agent` | Always | `openviking-memory-antigravity/<version>` |
| *(extra)* | `OPENVIKING_EXTRA_HEADERS` (JSON object) | Sent as given; `Authorization`, `X-API-Key`, `Accept`, `Content-Type`, `MCP-Protocol-Version` and `Mcp-Session-Id` are reserved and dropped with a warning |

`authMode` defaults to `trusted` when an account or user is configured and to `api_key` otherwise.

**TLS** (rustls, ring): every client trusts the bundled Mozilla roots plus the PEM bundle from `OPENVIKING_CA_CERTS` (personal `caCerts`, else `NODE_EXTRA_CA_CERTS`). The operating system's store (`SSL_CERT_FILE` / `SSL_CERT_DIR` replace it when set) is loaded up front by `mcp-proxy`, `doctor` and `setup`. Hooks and the worker run once per prompt and reading the macOS keychain costs about 120 ms, so they start without it and retry a request once with it after rustls reports an invalid peer certificate; the rest of that process keeps the store. Plain-HTTP servers never load it.

> **Root Key Guard**: OpenViking rejects `server.root_api_key` on `/mcp` (`403 Forbidden`) and tenant-scoped data endpoints. When the resolved `api_key` matches `ov.conf`'s `server.root_api_key` or `/mcp` returns `403`, `ov-agy` surfaces a remediation warning instructing the user to provision a `User` or `Admin` key via `ov config`.

### Standard JSON Response Envelope

- **Success** (`2xx`):
  ```json
  {
    "status": "ok",
    "result": {}
  }
  ```
- **Error** (`4xx` / `5xx`):
  ```json
  {
    "status": "error",
    "error": {
      "code": "PERMISSION_DENIED",
      "message": "..."
    }
  }
  ```

---

## 2. OpenViking REST Endpoints Used by `ov-agy`

### 2.1 `GET /health` and `GET /api/v1/system/status`
- `GET /health` returns top-level fields:
  ```json
  {
    "status": "ok",
    "healthy": true,
    "version": "0.x.y",
    "auth_mode": "api_key"
  }
  ```
- `GET /api/v1/system/status` returns authenticated identity info:
  ```json
  {
    "status": "ok",
    "result": {
      "initialized": true,
      "user": "alice"
    }
  }
  ```

### 2.2 Profiles, session archive and skill discovery (`SessionStart`)
- Queries private home-alias skills `viking://~/skills` and shared account skills `viking://agent/skills` (never uid-less `viking://user/skills`):
  - `GET /api/v1/fs/ls?uri=viking%3A%2F%2F~%2Fskills&include_abstract=true&limit=30`
  - `GET /api/v1/fs/ls?uri=viking%3A%2F%2Fagent%2Fskills&include_abstract=true&limit=30`
- Reads `profile.md` with `GET /api/v1/content/read` and recursively lists `preferences/` and `entities/` under `viking://~/memories` and the active `viking://~/peers/{peer}/memories`.
- Requests `latest_archive_overview` from `GET /api/v1/sessions/{session_id}/context?token_budget=1000`. Missing data is skipped.
- Entry fields: `name`, `uri`, `isDir`, `abstract` (optional), `access` (optional).

### 2.3 Context-Aware Recall (`POST /api/v1/search/search`)
- **Request** (`mode: "context"`):
  ```json
  {
    "query": "how do we handle worktree peer normalization?",
    "mode": "context",
    "session_id": "agy-<conversationId>",
    "dedup_turns": 5,
    "purpose": "coding",
    "score_threshold": 0.35,
    "query_expansion": "off"
  }
  ```
  - `query_expansion` is sent as configured (`recallQueryExpansion`: `auto` or `off`). Unset, it is `off` while `recallTimeoutMs` (default 3800, at most 25000) is below 10 s, because the server's expansion step has a 5 s fuse that the hook budget cannot wait for. With a longer budget the field is omitted and the server default applies. The `PreInvocation` hook timeout in `hooks.json` is 30 s; the plugin's own deadline governs.
  - Explicit recall limits set six weighted source quotas with one slot per source. The server applies its coding preset when quotas are omitted. Optional `max_tokens` (64–32000) and `rewrite` (true or "auto") control assembly.
  - Optional `"peer_scope": "actor"` when `recallPeerScope == "actor"`.
  - Optional `"exclude_uris": [...]` when configured (up to 200 entries).
  - Note: `mode: "context"` does **not** accept `target_uri` (server returns `400 INVALID_ARGUMENT`). If an older OpenViking server rejects `mode: "context"` (`400`/`422` unknown field), `ov-agy` falls back to `POST /api/v1/search/find` over `viking://~/memories` and `viking://~/skills` if neither actor-only scope nor URI exclusions are configured.
- **Response (`mode: "context"`)**:
  ```json
  {
    "status": "ok",
    "result": {
      "rendered": "<memory uri=\"viking://user/alice/memories/...\" ...>...</memory>",
      "digest": "",
      "entries": [
        {
          "uri": "viking://user/alice/memories/preferences/rust.md",
          "category": "preferences",
          "detail": "abstract",
          "score": 0.82,
          "text": "Prefer rustls-tls over OpenSSL for musl portability."
        }
      ],
      "stats": {
        "candidates": 4,
        "returned": 1,
        "used_tokens": 64,
        "rewrite": "off"
      }
    }
  }
  ```
  - Content selection priority: if `stats.rewrite == "no_relevant"`, emit nothing; otherwise `digest` (if non-empty) -> `rendered` (if non-empty) -> formatted `entries` (or `memories`/`resources`/`skills` list items).

### 2.4 Session Capture & Commit
- The worker reads completed messages from the full transcript when available and holds its cursor before an in-progress turn. Offline operations replay from `wal/<scope>/pending.jsonl` for the same URL, account, user, key and peer. The legacy `wal/pending.jsonl` is retained for manual recovery.
- **Tool parts**: a `PLANNER_RESPONSE` step's `tool_calls` (`[{name, args}]`, no ids) are answered by the next k `GENERIC` steps in call order (verified on 2954 of 2955 recorded steps). Each finished call becomes one part on the assistant message, in the combined form the sessions API documents:
  ```json
  {"type": "tool", "tool_id": "agy-<step_index>-<k>", "tool_name": "read_file", "tool_status": "completed", "tool_input": {"Path": "/repo/a.rs"}, "tool_output": "..."}
  ```
  - `tool_output` is the `GENERIC` content without its `Created At:` / `Completed At:` lines; `ERROR` or `CANCELED` steps set `tool_status: "error"`.
  - `call_mcp_tool` reports the inner tool: OpenViking's own server keeps bare names (`find`, `read`, ...) so the server recognizes them, other servers are named `<ServerName>/<ToolName>`, and `tool_input` is the MCP `Arguments`.
  - `finish` is control flow and never a tool part; its `result` (the only output of structured and subagent runs) is appended to the assistant text.
  - Until every result is written the cursor stays before the step. When the idle sweep finalizes a conversation, an unanswered step at the end of the file is emitted with the results it has.
  - Text parts are sent verbatim and tool output up to `captureToolMaxChars` (default 1,000,000; the server externalizes large output). Longer values keep their head and tail around `[… N characters omitted by ov-agy …]`. `captureFilters` substitutions also rewrite tool inputs and outputs; only the text decides drop/keep.
- **Batching**: messages go to `POST /api/v1/sessions/{session_id}/messages/batch` (`{"messages": [...]}`, at most 100). A 404/405 falls back to one `POST .../messages` per message, which also creates a missing session. A non-retryable rejection retries that batch one message at a time, so a single bad message does not cost the others. A retryable failure queues the remainder.
- **Commit policy**: after sending, the worker reads `pending_tokens` from `GET /api/v1/sessions/{session_id}` and commits when it reaches `commitTokenThreshold` (default 20000), as the Codex and Claude Code plugins do. Without session meta it estimates tokens as uncommitted bytes / 4. `commitEveryTurns`, when set, adds a message-count trigger.
- **Idle sweep**: agy has no `SessionEnd`. `SessionStart` therefore starts a detached `ov-agy sweep` at most once per 10 minutes, skipping the conversation that is starting. The sweep:
  - replays the starting workspace's queue;
  - finalizes every conversation idle for `OPENVIKING_AGY_IDLE_TTL_MS` (default 30 minutes) that still has uncommitted messages or an unread transcript tail: it catches up from the recorded transcript and commits with that conversation's own workspace and pinned peer;
  - retires committed state and per-conversation lock files untouched for 30 days.

  States written before this version record no transcript path and are committed but never caught up, because their cursor may refer to the other transcript serialization.
- **Session ID Convention**: `agy-{conversationId}`
- **Append Message (`POST /api/v1/sessions/{session_id}/messages`)**:
  ```json
  {
    "role": "user",
    "parts": [
      {
        "type": "text",
        "text": "<sanitized message text>"
      }
    ],
    "peer_id": "github.com-org-repo"
  }
  ```
  - `role`: `"user"` or `"assistant"`.
  - `peer_id`: included when non-empty.
  - If the server returns `404 NOT_FOUND` because the session does not yet exist, `ov-agy` creates the session via `POST /api/v1/sessions` (`{"session_id": "agy-<conversationId>"}`) and retries `POST /api/v1/sessions/{session_id}/messages` once.
  - A write (message or commit) counts as delivered only when the reply is 2xx **and** a JSON object whose `status` is not `"error"`. A 2xx without an OpenViking JSON envelope, such as a gateway or SSO login page, is queued to the WAL like a network error, so the cursor never advances past undelivered turns.
- **Commit Session (`POST /api/v1/sessions/{session_id}/commit`)**:
  ```json
  {
    "keep_recent_count": 0
  }
  ```
  - Response: `{"status": "ok", "result": {"session_id": "...", "status": "accepted", "task_id": "...", "archived": true}}`.

---

## 3. MCP Streamable HTTP Endpoint (`/mcp`)

- The proxy requests `roots/list` from clients advertising roots and refreshes the first local workspace after `notifications/roots/list_changed`. Installed plugins fall back to personal configuration when the host provides no local root.
- **Transport**: Stdio newline-delimited JSON-RPC 2.0 <-> `POST {base_url}/mcp`.
- **Headers**:
  - `Content-Type: application/json`
  - `Accept: application/json, text/event-stream`
  - `MCP-Protocol-Version: 2025-03-26` (updated from negotiated `initialize` result `protocolVersion` if returned)
  - `Mcp-Session-Id: <session_id>` (captured from `mcp-session-id` response header on `initialize` and sent on all subsequent requests)
- **Response Handling**:
  - `202 Accepted` / `204 No Content` / empty body for notifications: no output written to `stdout`.
  - `application/json`: single-line compact JSON + `\n` written to `stdout` and flushed immediately.
  - `text/event-stream`: parsed via `eventsource-stream`; each non-empty JSON-RPC `data` event is compacted onto a single line + `\n` and flushed immediately.
- **Concurrency**: `initialize` and notifications are forwarded in order on the reader. Every request with both a `method` and an `id` runs on its own task, at most 16 at once, so a slow `search` does not hold up a parallel `read` or a `notifications/cancelled`. Replies carry their ids and may be written out of order; each reply is one atomic line. A message with an `id` but no `method` is the host's reply to a server request and is forwarded without expecting an answer.
- **Session Recovery (`404 Not Found`)**:
  - When a non-`initialize` request with a cached `initialize` payload receives HTTP `404`, `ov-agy` clears `mcp-session-id`, replays `initialize` + `notifications/initialized` once to acquire a fresh `mcp-session-id`, and retries the original request once. Concurrent requests that hit the same expired session replace it only once.
  - Anything that replaces the session (404 recovery, a roots change to another identity, new credentials) waits for the requests still in flight on the old one.
- **Credential Reload (`401` / `403`)**: the proxy remembers the size and modification time of `ovcli.conf` and `ov.conf`. When a request or `initialize` is rejected with 401/403 and either file changed since it was read (`ov config switch`, `ov-agy setup`), the proxy reloads the configuration for the current workspace, reconnects with the new identity and retries once. Unchanged files leave the error to the host.
- **Error Frames** (written with the request's `id`):

  | Code | Cause |
  | :--- | :--- |
  | `-32001` | Transport failure, 401, or 403. The 403 message names `server.root_api_key` as the cause only when the configured key matches it; otherwise it mentions both the root key and a gateway rule. |
  | `-32002` | Any other HTTP status, a 404 that recovery could not fix, or a non-JSON-RPC 200 body (for example an SSO login page). |
  | `-32004` | The request outlived the 60 s request timeout; the server may still finish it. |
  | `-32700` / `-32600` | A stdin line that is not JSON, or not a JSON-RPC object or array. |
- **Shutdown**:
  - On `stdin` EOF the proxy waits for the requests in flight, then, with an active `mcp-session-id`, sends `DELETE {base_url}/mcp` with a `1s` timeout before exiting.

---

## 4. Configuration Files (`ovcli.conf`, `ov.conf`, `.openviking/config*.json`)

1. **Enablement Priority**:
   - `OPENVIKING_MEMORY_ENABLED` (`0|false|no|off` -> disabled; `1|true|yes|on` -> enabled)
   - `enabled` boolean in `ovcli.conf` (`plugin.antigravity.enabled` -> `plugin.enabled`) or `ov.conf` (`antigravity.enabled`)
   - File existence: enabled if `ovcli.conf` or `ov.conf` exists; silently disabled otherwise.
2. **Credential & Connection Priority** (honoring `OPENVIKING_CREDENTIAL_SOURCE=cli|env|auto`):
   - `OPENVIKING_URL` / `OPENVIKING_BASE_URL`, `OPENVIKING_API_KEY` / `OPENVIKING_BEARER_TOKEN`, `OPENVIKING_ACCOUNT`, `OPENVIKING_USER`, `OPENVIKING_PEER_ID`
   - Workspace `.openviking/config.local.json` and `.openviking/config.json` can override the peer and behavior settings. URL, keys and tenant identity come from the personal configuration or environment.
   - `OPENVIKING_CLI_CONFIG_FILE` or `~/.openviking/ovcli.conf` (`url`, `api_key`, `account`, `user`, `actor_peer_id`, `plugin`, `plugin.antigravity`)
   - `OPENVIKING_CONFIG_FILE` or `~/.openviking/ov.conf` (`server.url` or `server.host` + `server.port`, `server.root_api_key`, `antigravity`)
   - Default URL: `http://127.0.0.1:1933`.

---

## 5. Antigravity CLI (`agy`) Hook Wire Schemas

All `agy` hook payloads use `camelCase` JSON on `stdin` and expect a single JSON object on `stdout` with exit code `0`.

### Common `stdin` Fields
```json
{
  "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "workspacePaths": ["/path/to/workspace"],
  "transcriptPath": "/path/to/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl",
  "artifactDirectoryPath": "/path/to/.gemini/antigravity-cli/brain/<id>",
  "modelName": "auto",
  "lastUserInput": "Explain the module boundaries",
  "hookEventName": "PreInvocation",
  "cwd": "/path/to/workspace"
}
```

### 5.1 `SessionStart` & `PreInvocation`
- agy 1.5.16 restores history on ACP `session/load`. The next prompt sends `PreInvocation` without another `SessionStart`. It can recall newly stored memories.
- `SessionStart` injects an `<openviking-session>` status block and `<openviking-context source="startup">` (profiles, memory index, skill catalog, capped at `sessionStartMaxBytes`), records it in `<plugin data>/last-inject.md`, and starts the idle sweep.
- **`PreInvocation` additional `stdin` fields**: `invocationNum` (zero-based integer, recall only on 0), `initialNumSteps` (integer).
- **`stdout` (Context Injected)**:
  ```json
  {
    "injectSteps": [
      {
        "systemMessage": { "systemMessage": "<openviking-context>\n...\n</openviking-context>" }
      }
    ]
  }
  ```
  - **Safety Invariants**: Never emit `systemMessage.metadata` on model context. agy 1.5.16 no longer panics on it, but a step carrying `metadata.user_markdown` is hidden from the model entirely, even when `systemMessage` is non-empty (verified live; older builds panicked in `proto_utils.go`). Never emit `decision` on `SessionStart`, `PreInvocation`, or `Stop`.
  - agy 1.5.16 does not fire `SessionStart` on `--conversation` resume or after `/compact`. Compaction appends a `CHECKPOINT` transcript step holding a model-written summary, so injected startup context is not preserved verbatim. On the next `PreInvocation` (any `invocationNum`), `ov-agy` finds a `CHECKPOINT` newer than the one it last saw, injects the startup block again as `<openviking-context source="compaction">` (plus the session archive overview when `resumeArchiveInject` is on), and, with `autoCommitOnCompact`, starts a detached `ov-agy worker --finalize` that commits the session.
- **`stdout` (No-Op / Error Fallback)**: `{}`

### 5.2 `PreToolUse` (`hook pre-tool`)
- **`stdin`**:
  ```json
  {
    "toolCall": {
      "id": "toolu_01",
      "name": "view_file",
      "args": {
        "AbsolutePath": "/path/to/file.rs"
      }
    },
    "stepIdx": 4
  }
  ```
  *(Flat aliases `toolName` and `toolInput` are also accepted for test/harness compatibility.)*
- **`stdout`**:
  - A location argument (key ending in `path`, `file`, `directory`, `folder` or their plurals) beginning with `viking://` (case-insensitive):
    ```json
    {
      "decision": "deny",
      "reason": "Paths starting with viking:// are OpenViking URIs, not local files. Use the OpenViking MCP tools instead: read, list, tree, glob, grep, find or search (save a skill with add_skill)."
    }
    ```
  - Every other call, including unparseable input and `OPENVIKING_MEMORY_ENABLED=0`: `{"decision": "ask"}`.
  - Verified against agy 1.5.16: `{}` and `{"decision": ""}` deny the tool call, and a hook that times out fails it. `ask` is neutral: with `always-proceed` and in interactive Auto mode the call proceeds without a prompt, exactly as without the hook. `allow` auto-approves and would bypass the user's own `ask` rules, so it is never emitted.
  - Hook-side tool names come from agy step types, not from the model-facing tool list. The agy CLI toolset reports `read_file {Path}`, `write_file {Path, Content}` and `edit_file {Path, Target, Replacement}`, while listing and search go through `run_command`, which the guard does not match. The Antigravity app's `view_file`, `write_to_file`, `replace_file_content`, `multi_replace_file_content`, `list_dir`, `grep_search` and `find_by_name` stay in the matcher.
  - Cold start: until the release binary is cached, `scripts/ov-agy` answers hooks immediately (`ask` for `PreToolUse`, `{}` otherwise) and starts one background download; only `mcp-proxy` and other commands wait for it.

### 5.3 `PostInvocation` (`hook post-invocation`)
- **`stdin`**: the `PreInvocation` fields.
- **Behavior**: reads the transcript tail without network access. Only when the newest model step is a final answer (status `DONE`, no tool calls besides `finish`) does it report, once per user turn, the sources the turn used: the URIs recalled for that turn plus the `find`/`search` queries and `read` URIs of OpenViking MCP calls made since the user's input.
- **`stdout`**: a user-only step; agy renders `user_markdown` inline and the model receives nothing:
  ```json
  {"injectSteps": [{"systemMessage": {"systemMessage": "", "metadata": {"user_markdown": "OpenViking · 3 sources · 2 memories · 1 resource · 1 read"}}}]}
  ```
  `usageView=expanded` adds one line per source; `usageView=off` or `usageOutput=footer` makes the hook answer `{}`.

### 5.4 `Stop` (`hook stop`)
- **`stdin`**:
  ```json
  {
    "executionNum": 1,
    "terminationReason": "NO_TOOL_CALL",
    "error": "",
    "fullyIdle": true,
    "continuationActive": false
  }
  ```
- **Behavior**: When `fullyIdle == true` and `terminationReason` is not `"error"` / `"ERROR"`, spawns `ov-agy worker` detached with `Stdio::null()` streams and immediately writes `{}\n` to `stdout`.

---

## 6. Local State and Commands

Files in the plugin data directory (`OPENVIKING_PLUGIN_DATA`, by default `~/.gemini/<product>/plugin_data/openviking` under agy):

| File | Writer | Content |
| :--- | :--- | :--- |
| `state/<conversation>.json` | worker, hooks | Transcript cursor, uncommitted turns and bytes, pinned peer, last checkpoint, recall and usage bookkeeping. |
| `wal/<scope>/pending.jsonl` | worker | Offline queue for one URL/account/user/key/peer scope; entries expire after `pendingTtlDays` or `pendingMaxRetries`. |
| `last-recall.json` | `PreInvocation` | `outcome` (`injected`, `no_relevant`, `failed`), latency, query and injected sizes, error. |
| `last-capture.json` | worker | `turns_captured`, `turns_queued`, `turns_failed` (rejected for good), `batches`, `pending_tokens`, `commit_threshold`, `commit` (`committed`, `queued`, `failed`, `pending`), error. |
| `last-inject.md` | `SessionStart`, `PreInvocation` | The last startup or compaction block, headed by `<!-- source=... at=<epoch ms> -->`. |

Commands that read them:

- `ov-agy doctor [--json] [--offline] [--timeout-ms N] [--cwd DIR]`: sections of `ok`/`info`/`warn`/`fail` findings, each problem with a `fix`. The human report exits 1 when any finding fails; `--json` exits 0 and carries `ok`, `sections`, and the flat fields (`health_ok`, `server_user`, `server_role`, `server_ready`, `mcp_status_code`, `mcp_tool_count`, `data_dirs`, `warnings`, ...).
- `ov-agy status [--json] [--offline] [--cwd DIR]`: one screen with the server, identity, toggles, and the last injection, recall and capture. Always exits 0.
- `ov-agy setup [--url URL] [--api-key-stdin | --api-key-env VAR] [--account A --user U] [--no-verify] [--yes] [--dry-run]`: checks the values with `GET /health` (key accepted, not `role=root`) and `GET /api/v1/system/status`, then merges `url`, `api_key`, `account` and `user` into `ovcli.conf` (`OPENVIKING_CLI_CONFIG_FILE`), keeping every other key, after copying the old file to `ovcli.conf.bak-<epoch ms>`. Both files are mode 0600; a file that is not a JSON object is never overwritten.
