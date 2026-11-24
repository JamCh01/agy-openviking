---
name: ov-experience
description: >-
  Retrieve and apply reusable OpenViking operational Experiences during bug
  troubleshooting, incident recovery, build/deployment failures, and complex
  multi-step engineering workflows.
---

# OpenViking Experience Memory (`ov-experience`)

OpenViking extracts Experiences (proven procedures and lessons from earlier work) into `viking://~/memories/experiences`. Use them to avoid repeating known mistakes, but treat them as advice, not as facts about the current machine.

## Retrieval

1. **Search the Experiences directory**:
   - Call `find` with `query` describing the component or tool, the intended operation and the concrete error signature, `target_uri="viking://~/memories/experiences"` and `limit=5`. Example query: `"rustls musl cross-compile ring linker failure"`.
   - `search` with `mode="list"` and the same `target_uri` also works and can use the session for context.
2. **Pick at most three hits** whose preconditions match the current OS, toolchain and architecture. Skip directory nodes and the `.abstract.md` / `.overview.md` sidecars.
3. **Read the full body** of each selected hit with `read` (`uris` reads several at once). An abstract is not a substitute for the steps and verification checks in the body.
4. **Search again at most once** with a refined query if nothing matched. Then continue without Experience guidance and say so.

## URI Contract

- Pass the exact `viking://...` URI returned by `find` or `search` verbatim to `read`.
- Never fabricate `viking://user/default/memories/experiences/...` or uid-less `viking://user/memories/experiences/...` paths, and never strip or rewrite the user segment of a returned URI.

## Applying an Experience

- The user's current instructions, the repository code and live command output take precedence over any Experience.
- Verify paths, versions and commands against the current workspace before running them, and stay within the user's permission boundaries.
- When an Experience contradicts what you observe, follow the evidence and mention the conflict to the user.
