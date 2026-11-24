# OpenViking Context & Memory (`ov-agy`)

Zero-Node-dependency native Rust plugin bridging [OpenViking](https://github.com/volcengine/OpenViking)'s `viking://` hierarchical L0/L1/L2 virtual filesystem and long-term memory into Google Antigravity CLI (`agy`).

## Key Capabilities

- **Streamable HTTP MCP Proxy**: Bridges stdio JSON-RPC 2.0 to OpenViking `/mcp` (`application/json` and `text/event-stream` SSE), up to 16 requests at once, with automatic `404` session recovery and a reconnect when a 401/403 follows a key change.
- **Prompt-Cache-Safe Context Recall**: Injects profiles, the memory index and the skill catalog at `SessionStart` (again after `/compact`), and deduplicated `mode: "context"` memories at `PreInvocation` once per user turn without disrupting inner tool-call prompt caches.
- **Source Line**: after each answer, a user-only `OpenViking · N sources` line shows what the turn drew on.
- **Virtual URI Boundary Guard**: Sub-millisecond `PreToolUse` check preventing `viking://` virtual paths from leaking into local filesystem tools.
- **Anti-Poisoning Capture & Offline WAL**: Strips injected context tags and archives turns, tool calls included, asynchronously via `ov-agy worker`, queuing unsent messages in `wal/<scope>/pending.jsonl`, scoped to the URL, account, user, key and peer, during server outages. A new session commits conversations that went idle.
- **Git Worktree Peer Normalization**: Maps main checkouts and linked Git worktrees (SSH and HTTPS remotes) to a single canonical repository `peer` identifier.

## Quick Setup

1. Ensure an OpenViking server is running (default `http://127.0.0.1:1933`), then run the setup command, which checks the url and key against the server and writes them to `~/.openviking/ovcli.conf` (backup kept, mode 0600):

   ```bash
   ~/.gemini/config/plugins/openviking/scripts/ov-agy setup
   ```

   Or configure `~/.openviking/ovcli.conf` yourself:

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

3. Check everything with `/ov-doctor`, or `ov-agy doctor` and `ov-agy status` from a terminal.

Windows is untested: the hooks and the MCP server start `sh ./scripts/ov-agy`, which needs `sh` from Git for Windows on `PATH`.

## Bundled Skills, Subagent, and Rule

| Customization | Name | Purpose |
| :--- | :--- | :--- |
| **Skill** | `openviking-memory` | Choose between context search, `find`, list search and `grep`; read `viking://` URIs; decide when (not) to write |
| **Skill** | `openviking-skills` | Find, use, create, share and migrate skills stored in OpenViking |
| **Skill** | `/ov-doctor` | Check the install, configuration, server (`/health`, `/ready`, `/mcp` tools) and recent activity, with a fix for each problem |
| **Skill** | `/ov-ingest` | Import documentation, URLs, Git repositories (`add_resource`), and skills (`add_skill`) into `viking://` |
| **Skill** | `/ov-experience` | Recall operational troubleshooting procedures from `viking://~/memories/experiences` via `find`/`search` and `read` |
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
