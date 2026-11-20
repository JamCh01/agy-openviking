---
name: ov-ingest
description: >-
  Index local specifications, documentation directories, web pages, or remote
  Git repositories into OpenViking viking:// resources or skills and verify
  asynchronous indexing completion. Use when the user asks to import, ingest, or
  sync docs/repos into OpenViking.
---

# OpenViking Resource & Skill Ingestion (`ov-ingest`)

Follow this SOP to import external documentation, local specs, or Git repositories into the `viking://` knowledge base cleanly and verify indexing status.

## Step 1: Choose Target Scope & Tool

- **Shared Team Knowledge (`viking://resources/...`)**:
  - Use the OpenViking MCP `add_resource` tool for architecture docs, API specs, websites, and Git repositories shared across the account.
- **Private User Knowledge (`viking://~/resources/...`)**:
  - Pass `parent="viking://~/resources/<folder>"` (with `create_parent=true`) or `to="viking://~/resources/<name>"` when the user wants private user-scoped storage. Never use uid-less `viking://user/resources/...`.
- **Reusable Agent Skills (`viking://~/skills` or `viking://agent/skills`)**:
  - Use `add_skill` (never `write` or `add_resource`) so OpenViking parses `SKILL.md` frontmatter and generates skill package indexes.

## Step 2: Execute Ingestion

1. **Remote URL or Git Repository**:
   - Call `add_resource` with `path="https://github.com/org/repo"` (or documentation URL), an explicit `to="viking://resources/<project-name>"` when updating a stable path, and a concise `reason` explaining why the resource matters.
   - For multi-page documentation sites, pass `args={"depth": 1, "max_pages": 20}` (or sitemap URL) only when recursive crawling is desired.
2. **Local Files or Directories**:
   - Exclude secrets, `.env*`, `.git/`, `target/`, and `node_modules/` before importing.
   - Pass the local path to `add_resource` (or if the MCP tool returns a signed `temp_upload` URL for local upload, upload the file/clean archive as instructed without leaking credentials).
   - If the `ov` CLI is installed locally, `ov add-resource <path> --to viking://resources/<name>` can also upload local directories directly while honoring `.gitignore`.

## Step 3: Verify Asynchronous Indexing Status

Resource parsing, L0/L1 summary generation (`.abstract.md`, `.overview.md`), and vector embedding run asynchronously in OpenViking queues:

1. Record the `root_uri` and `task_id` returned by `add_resource` / `add_skill`.
2. Check task progress via the OpenViking task status tool (or `ov task status <task_id>` if `ov` CLI is available), or inspect the destination with `ls` / `tree` (`include_abstract=true`) at `root_uri`.
3. Confirm `.abstract.md` / `.overview.md` summaries are populated or run a targeted `find(query="...", target_uri="<root_uri>")` check before declaring ingestion complete. If background indexing is still running, report the `root_uri` and `task_id` clearly instead of blocking indefinitely.
