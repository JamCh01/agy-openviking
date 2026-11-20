---
name: ov-rule-search
description: >-
  Query compiled team architectural and compliance rules in OpenViking via
  rule_search before modifying unfamiliar modules, public APIs, security
  boundaries, or infrastructure configurations.
---

# OpenViking Architectural Rule Search (`ov-rule-search`)

Before modifying unfamiliar modules, cross-service contracts, database schemas, or security-sensitive code, query OpenViking's compiled Rules library so hidden team constraints are honored upfront.

## Workflow

1. **Construct Concrete `task_context`**:
   - Summarize:
     - **Goal**: What the user asked to build, refactor, or fix.
     - **Target Scope**: Module paths, APIs, services, or config files you plan to touch.
     - **Planned Actions**: Concrete changes (e.g., adding a dependency, changing wire serialization, altering auth middleware).
     - **Known Facts & Open Unknowns**: Include verified repository facts without guessing unobserved state.
2. **Invoke `rule_search`**:
   - Call the registered OpenViking MCP `rule_search` tool (e.g., `openviking_rule_search` or `rule_search`) with `task_context` (and optional `rule_probability_threshold`, default `0.65`).
   - Do **not** substitute generic `find` or `search` for `rule_search` when `rule_search` is available; compiled Rules carry structured `applies_when` and `exception` predicates.
3. **Evaluate Applicability Independently**:
   - Inspect every returned Rule in `rules` / `rendered`:
     - Empty `applies_when` means the Rule applies unconditionally unless its `exception` holds.
     - Non-empty `applies_when` applies only when concrete facts in the current task satisfy that condition.
4. **Handle Incomplete Delivery or Unavailable Tool**:
   - If `delivery.complete` is `false`, read any omitted Rule `viking://` URIs via `read` before proceeding.
   - If `rule_search` fails or is unavailable on the server, note that compiled Rule verification could not run rather than claiming zero rules apply.
5. **Re-check on Scope Shift**:
   - Re-run `rule_search` if your implementation plan expands into a new subsystem, external protocol, or deployment configuration.
