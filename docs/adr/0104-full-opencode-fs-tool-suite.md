# ADR-0104: Full OpenCode fs-tool suite with mediated call logging

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Extends:** [ADR-0037](0037-agent-acts-via-mediated-tools.md), [ADR-0096](0096-mediated-forge-read-tools.md)

## Context and Problem Statement

The review agent's only filesystem tool today is `READ_FILE`
(`services/review-agent/src/tools/read_file.rs`) — there is no write, edit, list, or delete tool;
the OpenCode review agent description is explicit that it is "read-only; never edits or runs
commands." [ADR-0103](0103-repo-configurable-opencode-review-presets.md) keeps review read-only by
design (review posts findings, it doesn't patch code), but the broader OpenCode-hosted agent
surface (`open` mode, [ADR-0088](0088-open-mode-autonomous-ticket-agent.md)) and the new preset
model both need a **complete, consistently-logged** fs-tool surface, not an ad-hoc one grown tool
by tool as each mode needed something new.

## Decision Drivers

- Every tool call the model makes must be attributable after the fact — which tool, which args,
  which agent/preset/task — for the same reason [ADR-0037](0037-agent-acts-via-mediated-tools.md)
  mediates writes in the first place: the model never gets a raw shell or raw filesystem handle.
- Read (review) and write (open mode) surfaces should share one tool implementation family so a
  fix or a logging change lands once, not per-mode.

## Considered Options

- **A — Grow tools ad hoc per mode, as needed.** Rejected: this is how the codebase got to
  "only `read_file` exists" despite two modes needing filesystem access; it produces exactly the
  kind of tool-surface drift ADR-0103 is closing off on the model-config side.
- **B — One fs-tool crate/module (`read`, `write`, `edit`, `list`, `glob`/`search`), each call
  logged through the same recorder path OpenCode already uses (ADR-0095), gated per-preset/per-mode
  by which tools are exposed in that mode's OpenCode config.** Chosen.

## Decision Outcome

Chosen option: **B**. Add `WRITE_FILE`, `EDIT_FILE`, `LIST_DIRECTORY` (and `GLOB`/`SEARCH_FILES` if
a mode needs it) alongside the existing `READ_FILE` in `services/review-agent/src/tools/`, all
routed through the same MCP dispatch (`services/review-mcp`) and the same
`services/review-agent/src/opencode/recorder.rs` completeness-authority logging path. Review-mode
OpenCode config continues to expose only the read-family tools (unchanged behavior — review still
never edits); `open`-mode config exposes the full set. Every tool call is logged with the same
shape regardless of which mode invoked it.

### Consequences

- Good, because a future third mode needing filesystem access reuses the same tools instead of
  growing a fourth ad-hoc implementation.
- Good, because tool-call logs are uniform across modes, simplifying the observability work in
  [ADR-0106](0106-opencode-fatal-situation-sentinel-plugin.md).
- Neutral, because this does not change what review is allowed to do — the read-only posture is a
  config choice (which tools are listed for the review agent), not a code capability gap anymore.

## More Information

Related: the ADR-0088 `open`-mode write-tool needs this ADR's `WRITE_FILE`/`EDIT_FILE` as its
underlying mechanism rather than a mode-specific implementation.
