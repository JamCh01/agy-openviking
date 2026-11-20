# OpenViking Context & Memory (`ov-agy`)

Zero-Node-dependency native Rust plugin bridging [OpenViking](https://github.com/volcengine/OpenViking)'s `viking://` hierarchical L0/L1/L2 virtual filesystem and long-term memory into Google Antigravity CLI (`agy`).

## Key Capabilities

- **Streamable HTTP MCP Proxy**: Bridges stdio JSON-RPC 2.0 to OpenViking `/mcp` (`application/json` and `text/event-stream` SSE) with automatic `404` session recovery.
- **Prompt-Cache-Safe Context Recall**: Injects session skills at `SessionStart` and deduplicated `mode: "context"` memories at `PreInvocation` once per user turn without disrupting inner tool-call prompt caches.
- **Virtual URI Boundary Guard**: Sub-millisecond `PreToolUse` check preventing `viking://` virtual paths from leaking into local filesystem tools.
- **Anti-Poisoning Capture & Offline WAL**: Strips injected context tags and archives turns asynchronously via `ov-agy worker`, queuing unsent messages in `wal/pending.jsonl` during server outages.
- **Git Worktree Peer Normalization**: Maps main checkouts and linked Git worktrees (SSH and HTTPS remotes) to a single canonical repository `peer` identifier.

## Quick Setup

1. Ensure an OpenViking server is running (default `http://127.0.0.1:1933`) and configure `~/.openviking/ovcli.conf`:

   ```json
   {
     "url": "http://127.0.0.1:1933",
     "api_key": "your-user-or-admin-api-key",
     "plugin": {
       "antigravity": {
         "enabled": true
       }
     }
   }
   ```

   **Important**: Do not use `server.root_api_key` from `ov.conf` for `/mcp` (OpenViking rejects Root API keys on `/mcp` with `403 Forbidden`). Always use a **User** or tenant **Admin** API key provisioned via `ov config`, or run a local unauthenticated server on `127.0.0.1:1933`.

2. On first cold start, `./scripts/ov-agy` automatically downloads and verifies (SHA-256) the prebuilt `ov-agy` binary matching the plugin version. In restricted networks, set `OPENVIKING_RELEASE_BASE_URL` to a mirror prefix.

## Bundled Skills, Subagent, and Rule

| Customization | Name | Purpose |
| :--- | :--- | :--- |
| **Skill** | `/ov-doctor` | Inspect resolved config, masked API key, Root-Key `403` guard, `/health`, `/mcp`, `peer` ID, and offline WAL |
| **Skill** | `/ov-ingest` | Import documentation, URLs, Git repositories (`add_resource`), and skills (`add_skill`) into `viking://` |
| **Skill** | `/ov-experience` | Recall operational troubleshooting procedures via `search_experience` and `read_experience` |
| **Skill** | `/ov-rule-search` | Check compiled team architectural rules via `rule_search` before modifying unfamiliar modules |
| **Subagent** | `viking-navigator` | Read-only L0 -> L1 -> L2 `viking://` explorer keeping the main agent context concise |
| **Rule** | `openviking-context` | Always-on rule enforcing `viking://` virtual path boundaries and `viking://~` home-alias addressing |

## Common Environment Overrides

- `OPENVIKING_MEMORY_ENABLED`: `true` or `false` master toggle.
- `OPENVIKING_URL` / `OPENVIKING_API_KEY`: Server URL and User/Admin API key overrides.
- `OPENVIKING_ACCOUNT` / `OPENVIKING_USER` / `OPENVIKING_PEER_ID`: Explicit multi-tenant and repository peer header overrides.
- `OPENVIKING_AUTO_RECALL` / `OPENVIKING_AUTO_CAPTURE`: Toggle automatic turn recall or background session capture.
- `OPENVIKING_RELEASE_BASE_URL`: Custom mirror base URL for prebuilt release archives and `checksums.txt`.

Source code and multilingual documentation: [https://github.com/JamCh01/agy-openviking](https://github.com/JamCh01/agy-openviking)
