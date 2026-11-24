---
name: ov-doctor
description: >-
  Diagnose and troubleshoot the OpenViking plugin configuration, credentials,
  server health, Root API Key 403 misconfigurations, MCP handshake, Git worktree
  peer identity, and offline WAL queue. Use when OpenViking recall, capture, or
  MCP tools are not working or when the user runs /ov-doctor.
---

# OpenViking Doctor (`ov-doctor`)

Run the bundled native diagnostic command to inspect the OpenViking plugin end-to-end without exposing raw secrets.

## Step 1: Run Diagnostics

Execute `./scripts/ov-agy doctor` from the plugin root (or `${PLUGIN_ROOT}/scripts/ov-agy doctor` if running from the workspace):

```bash
"${PLUGIN_ROOT:-.}/scripts/ov-agy" doctor
```

For machine-readable JSON output:

```bash
"${PLUGIN_ROOT:-.}/scripts/ov-agy" doctor --json
```

The diagnostic report verifies:
1. **Enablement & Config Source**: Whether the plugin is enabled and which layer supplied configuration (`env`, workspace `.openviking/config*.json`, `~/.openviking/ovcli.conf`, or `~/.openviking/ov.conf`).
2. **Masked API Key & Root-Key Guard**: Masks credentials and flags if `api_key` matches `ov.conf`'s `server.root_api_key` (which OpenViking rejects with `403 Forbidden` on `/mcp` and personal memory endpoints).
3. **`/health` Reachability & Latency**: Measures round-trip latency and checks server `version` and `auth_mode`.
4. **`/mcp` Streamable HTTP Handshake**: Sends an MCP `initialize` probe to verify `200 OK`, session establishment (`mcp-session-id`), and absence of `401`/`403` gateway rejections.
5. **Git `peer` Identity**: Confirms worktree-aware repository `peer` normalization across main checkouts and linked Git worktrees.
6. **Offline Capture WAL**: Reports pending queued entries and byte size in the active `wal/<scope>/pending.jsonl` queue.

## Step 2: Remediation Guide

- **Plugin Disabled (`enabled: false`)**:
  - Create `~/.openviking/ovcli.conf` (`chmod 600`) with `"url"` and `"api_key"`, or set `OPENVIKING_MEMORY_ENABLED=1` if intentionally running purely from environment variables.
- **Root Key `403 Forbidden` Warning**:
  - `server.root_api_key` in `ov.conf` is for cluster administration only and cannot access `/mcp` or tenant memory APIs.
  - Issue or select a **User** or **Admin** API key via `ov config` (or `POST /api/v1/admin/accounts/<account>/users`) and save it in `~/.openviking/ovcli.conf`.
- **`/health` Unreachable**:
  - Verify `openviking-server` is running on the configured URL (default `http://127.0.0.1:1933`) and check firewall/proxy settings.
- **Pending Offline WAL Entries (`pending_wal_entries > 0`)**:
  - Once `/health` and credentials are healthy, the next `Stop` capture worker replays the queue for the matching URL, account, user, API key and peer. A rotated key selects a new queue. The old unscoped `wal/pending.jsonl` requires manual verification of its original destination before recovery.

## Safety Rules

- Never print raw API keys or dump `~/.openviking/ovcli.conf` / `~/.openviking/ov.conf` contents verbatim.
- Ask the user before modifying `~/.openviking/ovcli.conf` or restarting `openviking-server`.
