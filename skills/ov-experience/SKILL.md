---
name: ov-experience
description: >-
  Retrieve and apply reusable OpenViking operational Experiences during bug
  troubleshooting, incident recovery, build/deployment failures, and complex
  multi-step engineering workflows while preserving OpenViking's telemetry
  contract.
---

# OpenViking Experience Memory (`ov-experience`)

Use OpenViking Experience memories to recall proven troubleshooting procedures and operational lessons from prior trajectories while maintaining strict telemetry attribution.

## Mandatory Telemetry & URI Contract

1. **Invoke Exact Experience Tools First**:
   - When troubleshooting a bug, incident, build failure, or complex operational workflow, always invoke `search_experience` and `read_experience` by their exact registered MCP tool names (`openviking_search_experience` / `search_experience` and `openviking_read_experience` / `read_experience`) when exposed by the OpenViking MCP server so OpenViking records recall and consumption telemetry against the session trajectory.
   - If the connected OpenViking server version exposes only generic retrieval tools (`find` / `search` and `read`), scope the search strictly to `target_uri="viking://~/memories/experiences"` (`limit=5`) and read the matched file URIs with `read`.
2. **Pass Canonical `viking://` URIs Verbatim**:
   - Always pass the exact `viking://...` URI returned by `search_experience` (or `find`/`search`) verbatim into `read_experience` (`read`).
   - **NEVER** fabricate `viking://user/default/memories/experiences/...` or uid-less `viking://user/memories/experiences/...` paths, and never strip or rewrite the user/tenant segment from a returned URI.
   - Ignore directory nodes and sidecar files (`.abstract.md`, `.overview.md`) when reading full Experience procedures.

## Troubleshooting SOP

1. **Formulate Focused Query**:
   - Include the component/tool name, intended operation, and concrete error signature or symptom (e.g., `"rustls musl cross-compile ring linker failure"`).
2. **Triage & Read (`1-3` Hits Max)**:
   - Call `search_experience` first. Select only the `1-3` entries whose preconditions match the current OS, toolchain, and architecture.
   - Call `read_experience` with each selected canonical `viking://` URI verbatim to inspect the full steps and verification checks.
3. **Validate Against Live Evidence**:
   - Treat Experience guidance as advisory operational knowledge subordinate to the user's current instructions, repository code, and permission boundaries.
   - Verify paths, versions, and commands against the current workspace before executing changes.
