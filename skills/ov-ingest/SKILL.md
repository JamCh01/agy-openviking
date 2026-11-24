---
name: ov-ingest
description: >-
  Index local specifications, documentation directories, web pages, or remote
  Git repositories into OpenViking viking:// resources or skills and verify
  asynchronous indexing completion. Use when the user asks to import, ingest, or
  sync docs/repos into OpenViking.
---

# OpenViking Resource & Skill Ingestion (`ov-ingest`)

Follow this SOP to import external documentation, local specs, or Git repositories into the `viking://` knowledge base and confirm that indexing finished.

## Step 1: Choose the Target and the Tool

- **Shared team knowledge (`viking://resources/...`)**: use `add_resource` for architecture docs, API specs, websites and Git repositories shared across the account. It is the default target when neither `to` nor `parent` is given.
- **Private knowledge (`viking://~/resources/...`)**: pass `parent="viking://~/resources/<folder>"` or an exact `to="viking://~/resources/<name>"`. Missing parent directories are created automatically. Never use uid-less `viking://user/resources/...`. If you cannot tell whether material is private, prefer `parent="viking://~/resources/inbox"` and tell the user.
- **Reusable agent skills**: use `add_skill`, never `write` or `add_resource`, so OpenViking parses the `SKILL.md` frontmatter. An empty `target_uri` installs into the user's own skills; `target_uri="viking://agent/skills"` shares the skill with the whole account.

Before adding, inspect the target space with `list` or `tree` and reuse an existing directory instead of creating a synonym (for example `docs/` versus `documents/`).

## Step 2: Ingest

1. **Remote URL or Git repository**: call `add_resource` with `path` set to the URL and a short `description` of why the resource matters.
   - `to` is an exact final URI and syncs an existing target to the new source, deleting entries the source no longer has. Use it only to refresh a stable path in place.
   - `parent` stores a new source under a directory and never overwrites; a name collision gets a suffix such as `name_1`.
   - `args={"site": true}` ingests a whole site from a bare domain (a sitemap, RSS or Atom URL already does), `args={"parse_mode": "no_split"}` keeps each document in one file, and `args={"auth_config": {"token": "..."}}` reads a private HTTPS Git repository.
   - `watch_interval` (minutes) keeps a remote source refreshed. Prefer 1440 or more, because every refresh re-embeds the whole resource.
2. **Local files or directories**:
   - Exclude secrets, `.env*`, `.git/`, `target/` and `node_modules/` first.
   - Pass the local path as `path`. The response is an upload instruction: HTTP POST the file (zip a directory first) to the returned URL without leaking credentials, and the server ingests it. Do not call `add_resource` again for that file.
   - If the `ov` CLI is installed, `ov add-resource <path> --to viking://resources/<name> --wait` uploads a local file or directory directly and waits for processing.
3. **Skills**: pass the full `SKILL.md` text as `data`, or a Git URL or local path as `path` (`list_only=true` previews a multi-skill source). Ask the user before installing from a source they did not name.

## Step 3: Verify Asynchronous Indexing

Parsing, L0/L1 summary generation (`.abstract.md`, `.overview.md`) and embedding run in the background after the tool returns.

1. Record the root URI (and task id, if any) from the response.
2. There is no task-status MCP tool. Check progress with `tree` (`include_abstract=true`) or `list` on the root URI, and with a targeted `find(query="...", target_uri="<root uri>")`. With the `ov` CLI installed, `ov task status <task_id>` reports the task directly.
3. If indexing is still running, report the root URI and task id to the user instead of waiting indefinitely.
