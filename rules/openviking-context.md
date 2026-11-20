---
trigger: always_on
description: OpenViking virtual filesystem, context tags, and memory tool usage rules
---

# OpenViking Context & Virtual Filesystem Rules

1. **Virtual URI Boundary (`viking://`)**:
   - Paths starting with `viking://` reside in the OpenViking virtual filesystem, **never** on the local OS disk.
   - Never pass `viking://` URIs to local filesystem or shell tools (`view_file`, `write_to_file`, `replace_file_content`, `multi_replace_file_content`, `list_dir`, `grep_search`, or `run_command`). Always use OpenViking MCP tools (`read`, `ls`, `tree`, `find`, `search`, `grep`, `glob`, `add_resource`, `add_skill`).
2. **Home-Alias Addressing (`viking://~`)**:
   - Address private user roots via the home alias (`viking://~/memories`, `viking://~/resources`, `viking://~/skills`, `viking://~/sessions`), and account-shared roots via `viking://resources` or `viking://agent/skills`.
   - Never fabricate `viking://user/default/...` or uid-less `viking://user/memories` URIs. When expanding a hit returned by OpenViking, pass its canonical `viking://` URI verbatim.
3. **Progressive L0 -> L1 -> L2 Retrieval**:
   - Check injected `<openviking-context>`, `<openviking-session>`, and `<available-skills>` blocks first before issuing duplicate searches.
   - Inspect L0 abstracts (`.abstract.md`) and L1 overviews (`.overview.md`) before reading large L2 documents, or delegate broad knowledge-base surveys to the `viking-navigator` subagent.
4. **Provenance Priority**:
   - Recalled memories reflect historical notes and prior sessions; current user instructions and live workspace code always take precedence on conflict.
