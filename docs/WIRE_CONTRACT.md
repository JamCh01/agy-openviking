# OpenViking & Antigravity CLI Wire Contract (`ov-agy`)

This document locks the verified public HTTP REST, MCP Streamable HTTP, configuration, and Antigravity CLI (`agy`) hook stdin/stdout wire contracts implemented by `ov-agy`.

`ov-agy` is a clean-room external client licensed under Apache-2.0 communicating strictly over OpenViking's documented public HTTP/MCP interfaces and Antigravity CLI's plugin/hook stdio JSON protocols.

---

## 1. OpenViking HTTP Headers & Envelope

### Outbound Request Headers

| Header | Condition | Value |
| :--- | :--- | :--- |
| `Content-Type` | JSON requests | `application/json` |
| `Authorization` | `api_key` non-empty | `Bearer <api_key>` |
| `X-API-Key` | `api_key` non-empty | `<api_key>` |
| `X-OpenViking-Account` | `account` non-empty | `<account>` |
| `X-OpenViking-User` | `user` non-empty | `<user>` |
| `X-OpenViking-Actor-Peer` | `peer` non-empty | `<peer_id>` |
| `User-Agent` | Always | `openviking-memory-antigravity/<version>` |

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

### 2.2 Skill Discovery (`SessionStart`)
- Queries private home-alias skills `viking://~/skills` and shared account skills `viking://agent/skills` (never uid-less `viking://user/skills`):
  - `GET /api/v1/fs/ls?uri=viking%3A%2F%2F~%2Fskills&include_abstract=true&limit=50`
  - `GET /api/v1/fs/ls?uri=viking%3A%2F%2Fagent%2Fskills&include_abstract=true&limit=50`
- Entry fields: `name`, `uri`, `isDir`, `abstract` (optional), `access` (optional).

### 2.3 Context-Aware Recall (`POST /api/v1/search/search`)
- **Request** (`mode: "context"`):
  ```json
  {
    "query": "how do we handle worktree peer normalization?",
    "mode": "context",
    "session_id": "agy-<conversationId>",
    "dedup_turns": 5,
    "limit": 6,
    "score_threshold": 0.35
  }
  ```
  - Optional `"peer_scope": "actor"` when `recallPeerScope == "actor"`.
  - Optional `"exclude_uris": [...]` when configured (up to 200 entries).
  - Note: `mode: "context"` does **not** accept `target_uri` (server returns `400 INVALID_ARGUMENT`). If an older OpenViking server rejects `mode: "context"` (`400`/`422` unknown field), `ov-agy` falls back to `POST /api/v1/search/find` over `viking://~/memories` and `viking://~/skills`.
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
- **Commit Session (`POST /api/v1/sessions/{session_id}/commit`)**:
  ```json
  {
    "keep_recent_count": 0
  }
  ```
  - Response: `{"status": "ok", "result": {"session_id": "...", "status": "accepted", "task_id": "...", "archived": true}}`.

---

## 3. MCP Streamable HTTP Endpoint (`/mcp`)

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
- **Session Recovery (`404 Not Found`)**:
  - When a non-`initialize` request with a cached `initialize` payload receives HTTP `404`, `ov-agy` clears `mcp-session-id`, replays `initialize` + `notifications/initialized` once to acquire a fresh `mcp-session-id`, and retries the original request once.
- **Shutdown**:
  - On `stdin` EOF with an active `mcp-session-id`, sends `DELETE {base_url}/mcp` with a `1s` timeout before exiting.

---

## 4. Configuration Files (`ovcli.conf`, `ov.conf`, `.openviking/config*.json`)

1. **Enablement Priority**:
   - `OPENVIKING_MEMORY_ENABLED` (`0|false|no|off` -> disabled; `1|true|yes|on` -> enabled)
   - `enabled` boolean in `ovcli.conf` (`plugin.antigravity.enabled` -> `plugin.enabled`) or `ov.conf` (`antigravity.enabled`)
   - File existence: enabled if `ovcli.conf` or `ov.conf` exists; silently disabled otherwise.
2. **Credential & Connection Priority** (honoring `OPENVIKING_CREDENTIAL_SOURCE=cli|env|auto`):
   - `OPENVIKING_URL` / `OPENVIKING_BASE_URL`, `OPENVIKING_API_KEY` / `OPENVIKING_BEARER_TOKEN`, `OPENVIKING_ACCOUNT`, `OPENVIKING_USER`, `OPENVIKING_PEER_ID`
   - Workspace `.openviking/config.local.json` -> `.openviking/config.json` (`version: 1`, `peer.id`, `peer.source`, `recall.*`, `capture.*`)
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
- **`PreInvocation` additional `stdin` fields**: `invocationNum` (1-based integer), `initialNumSteps` (integer).
- **`stdout` (Context Injected)**:
  ```json
  {
    "injectSteps": [
      {
        "ephemeralMessage": "<openviking-context>\n...\n</openviking-context>"
      }
    ]
  }
  ```
  - **Safety Invariants**: Never emit `systemMessage.metadata` (avoids `proto_utils.go` panic) and never emit `decision` on `SessionStart`, `PreInvocation`, or `Stop`.
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
  - Any string argument containing `"viking://"`:
    ```json
    {
      "decision": "deny",
      "reason": "Paths starting with viking:// live in the OpenViking virtual filesystem. Use OpenViking MCP tools (read, ls, tree, search, add_skill) instead of local file tools."
    }
    ```
  - Read-only local tools (`view_file`, `list_dir`, `grep_search`, `read_file`, `find_by_name`):
    `{"decision": "allow"}`
  - Write/edit or other tools (`write_to_file`, `replace_file_content`, `multi_replace_file_content`, etc.):
    `{"decision": "ask"}`
  - **Never** output `{}` for `PreToolUse`.

### 5.3 `Stop` (`hook stop`)
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
