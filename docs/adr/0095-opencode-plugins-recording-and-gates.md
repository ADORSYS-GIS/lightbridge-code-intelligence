# ADR-0095: Tool-call recording and gate enforcement live in first-party OpenCode plugins

- **Status:** Proposed
- **Date:** 2026-07-16
- **Deciders:** @stephane-segning

## Context and Problem Statement

Handing the agent loop to OpenCode ([RFC-0009](../rfc/0009-opencode-acp-agent-host.md),
[ADR-0094](0094-opencode-acp-open-mode-host.md)) surrenders two properties the native loop
enforced in code, and both are non-negotiable:

1. **Recording.** Every tool call's *actual* arguments and results — the right-bytes standard —
   plus per-turn reasoning must land in the transcript store
   ([ADR-0034](0034-agent-run-transcript-and-observability.md) lineage). ACP session updates alone
   are not a guaranteed-complete source (subagent-internal calls may not surface to the client).
2. **Gates.** The quality machinery — refute pass
   ([ADR-0091](0091-refute-pass-outward-disconfirmation-search.md)), coverage gate — was hardened
   into *enforced* mechanisms (`TurnFilter::force_names()`, PRs #403/#404) precisely because
   honor-system prompt gates were observed being gamed. [ADR-0026](0026-native-review-agent.md)'s
   reversal must not be re-run.

Where does this machinery live now, and what shape can it take against OpenCode's actual extension
contract?

## Decision Drivers

- **Enforcement must be mechanical**, not persuasive — the model must be *unable* to skip a gate,
  not merely told not to.
- **The plugin hook surface is what it is** (verified against `@opencode-ai/plugin`, 2026-07-16):
  `tool.execute.before` can throw and rewrite args; `tool.execute.after` sees results;
  `chat.params` is sampling-only — **no `tools`/`tool_choice` control**, so the native loop's
  force-now shape is not expressible; `experimental.chat.{system,messages}.transform` can rewrite
  prompts/messages but is `experimental.`-prefixed; the `event` bus streams message parts.
- **In-process beats protocol-level for capture:** plugins fire inside OpenCode for *every* tool
  call — including subagent-internal ones — where an ACP client sees only what the protocol
  chooses to surface.
- **TypeScript is the house ecosystem** for this layer (company OpenCode practice,
  `vymalo/opencode-oauth2` precedent).

## Considered Options

- **Option A — two first-party plugins** under `integrations/opencode/plugins/*`: a **recorder**
  and a **gate-interlock** (this ADR).
- **Option B — do both at the ACP boundary** in the Rust supervisor (record session updates,
  gate via `permission.ask` responses).
- **Option C — gates as prompts** (subagent instructions only), recording via Option A or B.

## Decision Outcome

Chosen option: **Option A** — the machinery lives *inside* the loop process, as plugins we own:

### The recorder — `@lightbridge/opencode-plugin-recorder`

Appends one JSONL line per event to a per-task file (`LCI_RECORDER_PATH`):

- `tool.before` — `{tool, callID, sessionID, args}` from `tool.execute.before` (raw args, pre-execution);
- `tool.after` — `{tool, callID, sessionID, title, output, metadata}` from `tool.execute.after`;
- `reasoning.part` — reasoning message parts observed on the `event` bus (`message.part.updated`),
  covering the ADR-0060 per-turn reasoning requirement;
- session lifecycle markers.

The agent-plane supervisor ships the file to the transcript store at task end. **The recorder is
the completeness authority**: RFC-0009 probe item (f) asserts recorder-JSONL ⊇ ACP-visible
information, and item (d) (subagent-internal calls) is answered here, not at the protocol.

### The gate-interlock — `@lightbridge/opencode-plugin-gate-interlock`

**Enforcement = block-until, on the stable hook.** Per-session state (built from
`tool.execute.after` observations) tracks which gate-relevant tools have actually run;
`tool.execute.before` on the **terminal tool** (`submit_findings` / `propose_pr`) **throws** until
preconditions hold, and the thrown message is the steering channel — it names exactly what is
missing (e.g. "refute each P0/P1 finding via `refute_finding` before submitting"). The model
cannot game a tool that refuses to execute; thrash is bounded by the supervisor's turn budget.

This deliberately differs from the native loop's force-now (`force_names()` restricts the tool set
and forces the next call): force-now is not expressible through `chat.params`. Block-until is
weaker on token cost (the model may spend turns before satisfying the gate) but equal on the
property that matters — **the gate cannot be skipped**.

**Experimental hooks carry steering only, never enforcement.** Mid-loop lead injection (the
SastAnchorGate pattern, ADR-0091 refinement) may use `experimental.chat.messages.transform`; if an
OpenCode upgrade breaks it, gates degrade to less-guided but remain *enforced* via the stable
`tool.execute.before`. This is the churn firewall.

### Location and loading

- Plugins are pnpm workspace packages under **`integrations/opencode/plugins/*`** (kebab-case
  dirs, `@lightbridge/opencode-plugin-*` names), Biome + `tsc --noEmit` gated like every TS
  package. They are **not published to npm** — the agent image vendors them and the rendered
  per-task config loads them (path entry vs. `.opencode/plugins/` symlink: RFC-0009 probe resolves
  the mechanics).
- Configuration rides environment variables (`LCI_GATE_TERMINAL_TOOL`, `LCI_GATE_REQUIRED_TOOLS`,
  `LCI_RECORDER_PATH`, …) rendered by the supervisor per task/mode.
- **OpenCode is version-pinned** in the agent image; every bump re-runs the RFC-0009 probe before
  it ships (the hook contract is the load-bearing dependency).

### Consequences

- **Good:** the two properties that made the native loop trustworthy — right-bytes transcripts and
  ungameable gates — survive the host swap, in ~two small TypeScript files we fully own.
- **Good:** capture fidelity *improves* over the protocol-level alternative (subagent-internal
  calls included by construction).
- **Bad / accepted:** the plugins are coupled to the `@opencode-ai/plugin` contract; a breaking
  hook change stalls an OpenCode upgrade until the plugins adapt (the pin + probe make this a
  visible, gated event rather than a silent regression).
- **Bad / accepted:** block-until spends model turns that force-now saved; measured under the
  supervisor's turn budget, revisited if it dominates cost telemetry.

## Pros and Cons of the Options

### Option A — first-party plugins (chosen)

- Good: in-process = complete capture + mechanical enforcement; stable-hook-only enforcement is
  churn-resistant; TS = house ecosystem, afternoon-sized escape hatch.
- Bad: coupled to the plugin contract (pinned + probed).

### Option B — everything at the ACP boundary

- Good: zero coupling to the plugin contract; pure Rust.
- Bad: **capture is incomplete by construction** — the client sees what ACP surfaces, and
  subagent-internal tool calls are not guaranteed to be in that set; gating via `permission.ask`
  covers permission-checked tools only and turns every gate into a permission round-trip. Fails
  driver 1 and 3.

### Option C — gates as prompts

- Good: no code at all.
- Bad: re-runs the exact honor-system regression that `force_names()` exists to prevent — the
  observed failure mode (PRs #403/#404), not a theoretical one. Rejected on the record.

## More Information

- [RFC-0009](../rfc/0009-opencode-acp-agent-host.md) — probe items (b), (d), (e), (f) validate
  this ADR's mechanisms; a failing (b) or (e) fails the design.
- [ADR-0094](0094-opencode-acp-open-mode-host.md) — the host these plugins ride in.
- [ADR-0034](0034-agent-run-transcript-and-observability.md) — the transcript store the recorder
  feeds; [ADR-0091](0091-refute-pass-outward-disconfirmation-search.md) — the refute pass the
  interlock enforces.
- PoC: `integrations/opencode/plugins/{recorder,gate-interlock}/`, exercised by
  `integrations/opencode/probe/`.
