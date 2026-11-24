---
name: ov-doctor
description: >-
  Diagnose and fix the OpenViking plugin for agy: the plugin install (hooks,
  launcher, MCP server), the configuration (ovcli.conf, ov.conf, OPENVIKING_*
  variables, workspace files), the server (reachability, api key, root key,
  /ready, /mcp tools) and recent activity (last injection, recall, capture,
  offline queue, hook errors). Use when OpenViking memory is not working: no
  <openviking-context> block, empty recall, captures not landing, missing or
  failing OpenViking MCP tools, 401/403, right after installing or switching
  servers or keys, or when the user runs /ov-doctor or asks for the plugin's
  status ("记忆没生效", "插件状态", "连不上 OpenViking").
---

# OpenViking Doctor (`ov-doctor`)

The plugin ships two read-only commands: `doctor` runs every check and suggests a fix for each problem; `status` shows in one screen what the plugin is doing right now.

## Step 1: Run the doctor

The plugin root is two directories above this `SKILL.md`; an installed plugin lives at `~/.gemini/config/plugins/openviking`. Your shell does not receive `PLUGIN_ROOT`, so call the launcher by its absolute path:

```bash
"$HOME/.gemini/config/plugins/openviking/scripts/ov-agy" doctor --cwd "$PWD"
```

If the plugin is installed elsewhere, take the `path` of the plugin named `openviking` from `agy plugin list --json`.

- `--json` prints the machine-readable report: `ok`, every finding under `sections`, and flat fields such as `health_ok`, `server_user`, `mcp_tool_count`, `data_dirs`.
- `--offline` skips the server checks; `--timeout-ms` sets the per-request timeout (default 3000).
- `--cwd <workspace>` evaluates the peer and workspace files of that workspace.

The human report exits 1 when a check fails; `--json` always exits 0. It has four sections:

1. **Plugin install**: binary and manifest versions, enablement in agy, the five hooks, the launcher, the MCP server entry.
2. **Configuration**: enablement and its reason, which files or variables supplied the connection, url lint (an `/api/v1` or `/mcp` suffix, whitespace, a missing scheme), the masked key (a v2 key also shows the account and user it names), the root-key guard, the peer and its source, the recall and capture toggles, bypass patterns, `OPENVIKING_*` variables (secrets masked), proxy variables, the TLS roots (bundled, system store, `OPENVIKING_CA_CERTS`), the debug log.
3. **Server**: `/health` with and without the key (reachability, version, `auth_mode`, the identity the key resolves to, a root key), `/ready` (storage, vector index, embedder), `/api/v1/system/status`, a read of `viking://~/memories`, and an MCP `initialize` plus `tools/list` with the tool count.
4. **Recent activity**: the last context injection, recall and capture, the offline queue, every agy data directory with its conversations and queues, and recent errors from the debug log.

For a quick look instead, run `.../scripts/ov-agy status` (add `--offline` to skip the one `/health` request).

## Step 2: Symptom, cause, fix

| Symptom | Usual cause | Where to look, what to do |
| --- | --- | --- |
| No `<openviking-context source="startup">` block when a session starts | Plugin disabled, server unreachable within the 4 s start budget, or the first session after install while the binary downloads | Configuration and Server sections. After a fresh install, start a second session once the download finishes. |
| Startup block present, but nothing recalled for prompts | Recall timed out, nothing relevant, a short prompt, recall off, or a bypass pattern | `status` shows the last recall: `failed` with a timeout means `OPENVIKING_RECALL_TIMEOUT_MS` (default 3800) is too short for this server; `no_relevant` is normal for unrelated prompts. |
| OpenViking MCP tools missing in agy | The MCP server did not start (binary download blocked, plugin disabled) | Plugin install section. Behind a firewall, set `OPENVIKING_RELEASE_BASE_URL` to a mirror of the release assets. |
| MCP tools fail with 401 or 403 | Key rejected, a root key, or a gateway rule | Server section. After `ov config switch` or `ov-agy setup`, the running proxy picks the new key up on the next 401/403; a 403 with a valid user key usually means a gateway in front of the server. |
| MCP tool fails with `-32004` | The request outlived the proxy's 60 s timeout | The server may still finish it. Narrow the query, or read fewer URIs at once. |
| Work is not showing up in memory | Below the commit threshold, queued offline, or rejected | `status` shows the last capture. `commit pending (x/20000)` is normal: commits happen at the threshold, at `/compact`, or when a later session starts after 30 idle minutes. Queued turns replay once the server is back; rejected ones (401/403/404/422) are not retried. |
| Memories land under the wrong project | The peer comes from the repository's `origin` remote, a workspace `peer.id`, or `OPENVIKING_PEER_ID` | Configuration section, `peer ... (source=...)`. Set `{"peer": {"id": "..."}}` in `.openviking/config.json` to choose it. |
| A file tool call on a `viking://` path is denied | The plugin's guard: those are database URIs | Use the OpenViking MCP tools (`read`, `list`, `find`, `search`, ...). |
| "agy last wrote plugin state to …" | The doctor ran with a different data directory than the hooks | Run it again with `OPENVIKING_PLUGIN_DATA=<that directory>` to see the queue as the hooks do. |
| Proxy warning for the server host | `HTTPS_PROXY`/`ALL_PROXY` set without a `NO_PROXY` entry | Add the host (for a local server `127.0.0.1,localhost`) to `NO_PROXY` in the environment that starts agy. |
| TLS error against a self-hosted server | The certificate does not chain to a trusted root: ov-agy trusts the bundled Mozilla roots, the system store (or what `SSL_CERT_FILE` / `SSL_CERT_DIR` name instead) and `OPENVIKING_CA_CERTS` | The Configuration section's `TLS roots` line shows what is loaded. Add the CA to the system store, or set `OPENVIKING_CA_CERTS=/path/to/ca.pem` in the environment that starts agy. |
| "Ignoring … in workspace config.json" | A personal setting in a repository file | Move it into the `plugin.antigravity` block of `~/.openviking/ovcli.conf`. |

Fixes for configuration problems:

- **No credentials or the wrong server**: `ov config` (OpenViking CLI) or the plugin's own `scripts/ov-agy setup`, which checks the url and key against the server, then merges them into `~/.openviking/ovcli.conf` with a backup and mode 0600. For scripts: `printf '%s\n' "$KEY" | ov-agy setup --url https://ov.example.com --api-key-stdin --yes`.
- **Root key (403 on `/mcp` and tenant data)**: `server.root_api_key` is for administration only. Create a user key with it, then switch to that key.
- **Plugin disabled in agy**: `agy plugin enable openviking`, or turn it on in agy's Customizations page.

## Safety rules

- Never print raw API keys, and never dump `~/.openviking/ovcli.conf` or `~/.openviking/ov.conf` verbatim; the doctor already masks secrets.
- Ask the user before changing `~/.openviking/ovcli.conf`, running `setup`, or restarting `openviking-server`.
