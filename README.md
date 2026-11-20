# `agy-openviking` (`ov-agy`)

Production-grade, zero-Node-dependency Rust plugin integrating [volcengine/OpenViking](https://github.com/volcengine/OpenViking) with Google Antigravity CLI (`agy`).

Licensed under **Apache-2.0**. Implemented cleanly over OpenViking's public HTTP REST API (`/health`, `/api/v1/*`) and MCP Streamable HTTP endpoint (`/mcp`).

---

## Architecture Highlights

- **Two-Tier Runtime & Fail-Open Guarantee**:
  - `ov-agy hook pre-tool` and `ov-agy hook stop` run on a **zero-Tokio synchronous fast path** (`< 1ms`), spawning `ov-agy worker` detached (`Stdio::null()`) when `fullyIdle == true`.
  - `ov-agy hook session-start`, `ov-agy hook pre-invocation`, `ov-agy mcp-proxy`, `ov-agy worker`, and `ov-agy doctor` use a lightweight single-threaded Tokio runtime (`new_current_thread`).
  - Every hook catches errors/timeouts and emits valid Antigravity fallback JSON with exit code `0` so `agy` never stalls when OpenViking is offline.
- **Prompt-Cache-Safe Turn Deduplication**:
  - `PreInvocation` seeks only the trailing `64 KiB` of `transcript.jsonl`, hashes `(conversationId, turn_offset, query)`, and fires `POST /api/v1/search/search` (`mode: "context"`) once per user turn — returning `{}` with zero network I/O on inner tool-loop iterations (`invocationNum > 1`).
- **Anti-Poisoning Capture Worker & Offline WAL**:
  - Strips `<openviking-context>`, `<openviking-session>`, and `<available-skills>` blocks before sending turns to `POST /api/v1/sessions/agy-{conversationId}/messages`, commits periodically, and queues unsent payloads to `wal/pending.jsonl` (up to `10 MiB`, protected by `fs4` advisory locks) during server outages.
- **Git Worktree Peer Normalization**:
  - Resolves linked `.git` worktree files (`gitdir:` + `commondir`) back to the shared `.git/config`, normalizing SSH and HTTPS remotes (e.g., `git@github.com:Org/Repo.git` and `https://github.com/Org/Repo.git`) into the identical `github.com-org-repo` `peer` ID.
- **Streamable HTTP MCP Proxy**:
  - Bridges stdio JSON-RPC to `POST {base_url}/mcp` across both `application/json` and `text/event-stream` (SSE) responses, transparently re-initializing `mcp-session-id` once on `404 Not Found` after server restarts.

---

## Installation & Usage

### 1. Configure OpenViking Credentials

Configure `~/.openviking/ovcli.conf` (`chmod 600`) with a **User** or **Admin** API key (never `server.root_api_key`):

```json
{
  "url": "http://127.0.0.1:1933",
  "api_key": "your-user-or-admin-key",
  "plugin": {
    "antigravity": {
      "enabled": true
    }
  }
}
```

### 2. Build (Development) & Install in `agy`

```bash
cargo build --release
agy plugin validate --strict .
agy plugin install .
```

### 3. Bundled Customizations

| Type | Name | Purpose |
| :--- | :--- | :--- |
| **Skill** | `/ov-doctor` | Run `./scripts/ov-agy doctor` to inspect config, Root-Key 403 guard, `/health`, `/mcp`, `peer` ID, and WAL |
| **Skill** | `/ov-ingest` | SOP for importing docs, websites, and Git repos into `viking://` resources/skills |
| **Skill** | `/ov-experience` | Telemetry-preserving troubleshooting workflow (`search_experience` + `read_experience`) |
| **Skill** | `/ov-rule-search` | Query compiled team architectural constraints via `rule_search` before modifying modules |
| **Subagent** | `viking-navigator` | Hierarchical L0 -> L1 -> L2 `viking://` explorer keeping main agent context clean |
| **Rule** | `openviking-context` | Enforces `viking://` virtual URI boundaries and `viking://~` home-alias usage |

---

## Wire Specification & Verification

- Wire contract reference: [docs/WIRE_CONTRACT.md](docs/WIRE_CONTRACT.md)
- Run full test & lint suite:
  ```bash
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  ./scripts/ov-agy doctor --json
  agy plugin validate --strict .
  ```
