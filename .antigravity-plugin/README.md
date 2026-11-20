# OpenViking Context & Memory (`ov-agy`)

Zero-Node-dependency Rust plugin integrating [OpenViking](https://github.com/volcengine/OpenViking) with Google Antigravity CLI (`agy`).

## Capabilities

- **Automatic Session & Turn Recall**: Injects concise session status, available remote `viking://` skills, and deduplicated turn-level context (`mode: "context"`) while preserving prompt cache stability across multi-step tool loops.
- **Virtual URI Safety Guard**: Prevents `viking://` virtual paths from leaking into local filesystem tools (`PreToolUse`).
- **Detached Capture & Offline WAL**: Sanitizes injected context tags and captures conversation turns asynchronously via `ov-agy worker` with durable offline queue replay (`wal/pending.jsonl`).
- **Streamable HTTP MCP Proxy**: Bridges stdio JSON-RPC to OpenViking `/mcp` (both `application/json` and `text/event-stream` SSE) with transparent `404` session recovery.
- **Git Worktree Peer Normalization**: Maps main checkouts and linked Git worktrees to a single canonical repository `peer` identity.

## Quick Setup

1. Ensure an OpenViking server is running and configure `~/.openviking/ovcli.conf` with a **User** or **Admin** API key (do not use `server.root_api_key`):
   ```json
   {
     "url": "http://127.0.0.1:1933",
     "api_key": "your-user-or-admin-key"
   }
   ```
2. Install and verify:
   ```bash
   agy plugin install .
   ./scripts/ov-agy doctor
   ```
3. Run `/ov-doctor` inside `agy` at any time to inspect connectivity, credentials, Git `peer` ID, and offline WAL status.
