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
   - Returned Rules are constraints, not evidence that an unobserved applicability condition is true. Do not leave facts out of `task_context` to steer which Rules come back.
   - Inspect every returned Rule in `rules` / `rendered`:
     - Empty `applies_when` means the Rule applies unconditionally unless its `exception` holds.
     - Non-empty `applies_when` applies only when concrete facts in the current task satisfy that condition.
4. **Check the Delivery Before Relying on the Result**:
   - `delivery` carries `complete`, `rule_count` and `rendered_chars`. `complete: true` means the matched Rules were delivered whole, not that the whole library was searched; an empty result is not a compliance verdict.
   - `complete: false` means some matched Rules are missing from `rendered`. `read` the `uri` of every entry in `rules` that `rendered` does not contain, in bounded sections, before acting. Do not repeat an identical oversized search, and do not treat the delivered subset as a complete check. If the remaining Rules cannot be retrieved, report the incomplete check and pause the action.
   - `judge: "pass"` with a `warning` means the server could not judge applicability: every candidate came back with `probability` 1.0, so check each Rule's `applies_when` and `exception` yourself.
   - A failed or unavailable `rule_search` is not "no Rules apply". HTTP 422 means a blank `task_context`, one over 64,000 characters, or a threshold outside 0 to 1: fix the request. Otherwise note that compiled Rule verification could not run instead of substituting `find` or `search`.
5. **Re-check on Scope Shift**:
   - Re-run `rule_search` if your implementation plan expands into a new subsystem, external protocol, or deployment configuration.
   - After `/compact`, earlier Rule results may be gone from the context: search again for the current task.
