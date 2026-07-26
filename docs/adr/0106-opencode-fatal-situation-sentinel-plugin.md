# ADR-0106: OpenCode fatal-situation sentinel plugin

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Extends:** [ADR-0095](0095-opencode-plugins-recording-and-gates.md)

## Context and Problem Statement

[ADR-0095](0095-opencode-plugins-recording-and-gates.md) put recording and gate enforcement into
first-party OpenCode plugins (`integrations/opencode/plugins/{recorder,gate-interlock,logger}`).
None of the three plugins are responsible for detecting and surfacing a **fatal** OpenCode-side
situation — a crash, a stuck/hung ACP session, an unrecoverable provider error, or the session
exiting without ever calling `FINISH`/`ABORT`. Today that class of failure is only visible as an
absence (no recorder JSONL, no telemetry) which the control plane has to infer rather than being
told directly, and with more presets/entry-points ([ADR-0103](0103-repo-configurable-opencode-review-presets.md))
and a broader tool surface ([ADR-0104](0104-full-opencode-fs-tool-suite.md), [ADR-0105](0105-github-mcp-via-app-derived-token.md))
there are more ways a session can end badly.

## Decision Drivers

- Fatal-session detection belongs in the same place as recording/gates — inside OpenCode, where the
  actual crash/hang is observed — not reconstructed after the fact from telemetry gaps on the
  control-plane side.
- Must not become a fourth silently-diverging mediation surface; reuse the existing plugin
  transport/logging conventions from ADR-0095 rather than inventing a new reporting channel.

## Considered Options

- **A — Infer fatal sessions control-plane-side from telemetry timeouts only.** Rejected: this is
  today's status quo and is exactly the gap this ADR closes — a timeout conflates "still working" with
  "already dead," and gives no root cause.
- **B — A fourth first-party plugin (`sentinel`) that hooks OpenCode's own error/exit/session
  lifecycle events, and reports a structured fatal-event record through the existing recorder JSONL
  path plus a direct signal to the runner.** Chosen.

## Decision Outcome

Chosen option: **B**. Add `integrations/opencode/plugins/sentinel/`, registered alongside the other
three in `integrations/opencode/config/opencode.jsonc`. It observes provider errors, uncaught
exceptions, and session-exit-without-terminal-tool-call, and emits a structured `fatal_event` record
(kind, message, last-tool-call, session id) through the same JSONL recorder path ADR-0095 already
established, plus writes a small sentinel marker file the agent-runner checks on session exit so a
fatal outcome is reported deterministically rather than inferred from a timeout.

### Consequences

- Good, because a fatal OpenCode session now produces a direct, structured cause instead of a
  telemetry silence the control plane has to interpret.
- Good, because it reuses the ADR-0095 recorder/logging path rather than adding a new transport.
- Neutral, because this plugin is diagnostic only — it does not change gate or recording behavior,
  and it does not retry or recover the session itself (retry/backoff stays the runner's job).

## More Information

The recorder's proven role as "completeness authority" (ADR-0095, ADR-0103's more.info note) is the
direct precedent for trusting a plugin-side signal over a control-plane inference.
