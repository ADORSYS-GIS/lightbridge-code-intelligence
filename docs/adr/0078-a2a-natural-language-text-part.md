# ADR-0078: A2A `review` accepts a natural-language `text` part alongside the `data` part (RFC-0006)

- **Status:** Accepted
- **Date:** 2026-07-09
- **Deciders:** @stephane-segning

> **Accepted 2026-07-10, implemented in this PR.** `parse_review_request` now sources the prompt
> from `text` parts (winning over `data.prompt`, concatenated in message order and trimmed) while the
> target/scope stay read solely from `data`; a `text`-only message returns the guided `INVALID_PARAMS`
> instead of a bare rejection. The agent card advertises `text/plain` as an input mode with a
> `text`+`data` example, and `docs/a2a-review-skill.md` documents the form. Phase-4 `input-required`
> remains the deferred upgrade of the text-only branch.

## Context and Problem Statement

The A2A `review` skill ([RFC-0006](../rfc/0006-a2a-agent-surface.md), Phase 1, live #308) takes a
**structured `data` part** and nothing else: [`parse_review_request`](../../services/control-plane/src/a2a/mapping.rs)
reads the first `data` object off the message and returns `ParseError::NoDataPart` when a message
carries only `text`. A2A messages are natively multi-part (`text` / `file` / `data`), and A2A peers
— chat agents, orchestrators, humans driving Postman — *naturally* express intent as
**natural language**: "review PR 128, focus on the auth changes and the new migration." Today that
request is rejected unless the caller hand-builds the structured object.

The question: **can a caller send a natural-language `text` part *and* the `data` part in the same
message, and if so, what does each control?** This ADR designs that. It is a **design-only** record;
implementation is deferred (there is no code change in this PR).

The tension is specific and safety-critical. Two of the review's inputs must be **exact** and the
`a2a` role **cannot** derive them:

- The **target** (`repo`, `pr`, and especially `headSha`) must be precise. The role holds **no forge
  credentials** (RFC-0006 trust boundary) — it cannot resolve "PR 128" to a head commit, so
  `headSha` is *required* and a missing head is `REJECTED` rather than guessed (a null head would
  silently review the default branch and post a wrong review — the rule in
  [`handler.rs`](../../services/control-plane/src/a2a/handler.rs)).
- The **scope** (diff vs whole-tree) is decided *solely* by whether `baseSha` is present (ADR echoed
  back to callers in the completion context, PR #315). It is not a matter of interpretation.

Everything *else* — the review's **emphasis/instruction** (today's optional `prompt`, which becomes
the run's `command_text` and is shown to the agent, [ADR-0033](0033-inbound-command-parsing-and-run-kinds.md)) — is
**inherently fuzzy** and already free text. That is exactly what natural language is good for, and
where misinterpretation is cheap (it steers attention; it cannot mis-target or mis-scope the run).

So the design must let NL carry the *instruction* without ever letting it decide the *target* or
*scope*.

## Decision Drivers

- **Ergonomics:** accept the message shape A2A peers naturally send (text), so the skill is usable
  from a chat/agent client without hand-assembling JSON.
- **Safety boundary is non-negotiable:** NL must never redirect a review to a different repo/PR or
  silently change scope. Target and scope stay structured and authoritative.
- **No forge credentials, no guessing:** the role still cannot resolve a head/base from prose, so
  the fields it can't safely infer stay caller-supplied.
- **Least surprise / backward compatible:** existing `data`-only callers are unaffected; the new
  behaviour is purely additive.
- **Forward-compatible with Phase 4 (`input-required`):** the eventual clarify-then-confirm loop
  (RFC-0006 Phase 4, gated on [ADR-0076](0076-restate-task-lifecycle-workflow.md) awakeables) should
  slot in without redesign.

## Considered Options

- **A. `text` augments `data` — text is the instruction, data is the target/scope (chosen).** A
  message may carry both. The `data` part remains authoritative for `repo`/`pr`/`forge`/`headSha`/
  `baseSha`; the `text` part(s) supply the natural-language instruction and become the run's
  `prompt`/`command_text`. `text`-only (no data part) is **rejected with actionable guidance** now,
  and becomes the entry point for Phase-4 `input-required` clarification later.
- **B. Parse the target *out of* the natural language** (extract repo/PR/SHAs from prose via the
  agent or a regex). Rejected: the role has no forge credentials to resolve a PR→headSha, so it would
  either guess (the exact silently-wrong-review failure `headSha`-required prevents) or need a
  round-trip it can't make in Phase 1. NL cannot safely produce an exact commit SHA.
- **C. Let `text` override structured fields** (e.g. "actually review PR 130" in text wins over
  `data.pr`). Rejected: this hands target/scope control to fuzzy input — the precise failure mode
  the boundary exists to prevent.

## Decision Outcome

Chosen: **Option A.** The `data` part is the **precise, authoritative target + scope**; the `text`
part is the **natural-language instruction**. NL steers emphasis; structured data fixes what is
reviewed and how widely. Concretely (the parser contract to implement later):

### Precedence and merge rules

1. **Target + scope come only from `data`.** `repo`, `pr`, `forge`, `headSha`, `baseSha` are read
   from the `data` object exactly as today. `text` **never** sets or overrides them. `headSha` stays
   required; `baseSha` still decides scope. A `data` object that is missing/invalid on these fields
   fails exactly as it does now (`REJECTED` / `INVALID_PARAMS`).
2. **The instruction (`prompt`) is sourced with this precedence:**
   - one or more **`text` parts present** → the prompt is the text parts **concatenated in message
     order** (newline-joined), trimmed. This is the human's direct instruction and **wins over**
     `data.prompt`.
   - no text part, **`data.prompt` present** → use it (today's behaviour, unchanged).
   - neither → the existing default intent string.
   A `data.prompt` supplied *alongside* text is treated as a lower-priority hint; the winning text is
   what reaches the agent. (Rationale: when a caller typed a sentence, that sentence is the intent.)
3. **`text`-only messages (no `data` part) are rejected — for now — with actionable guidance.**
   Instead of the bare `NoDataPart`, return an `INVALID_PARAMS` whose message names the minimum
   precise fields the caller must supply in a `data` part (`repo`, `pr`, `headSha`) and points at the
   calling guide. This keeps the safety boundary (no target-from-prose) while turning a dead-end
   error into a fixable one. **Phase 4** upgrades this exact branch: with `input-required`, a
   text-only (or partial) request instead transitions to `INPUT_REQUIRED`, the server asks for the
   missing precise fields, and the caller confirms — a real clarify-then-confirm loop rather than a
   rejection.
4. **`file` parts remain ignored** (out of scope; reserved).

### What changes (design; not built here)

- **`parse_review_request`** ([mapping.rs](../../services/control-plane/src/a2a/mapping.rs)): also
  collect `text` parts (in order), apply the precedence above, and replace the blanket `NoDataPart`
  rejection with the guided error for the text-only case. The function stays pure and unit-testable;
  the target/scope parsing is untouched.
- **Card + docs** ([card.rs](../../services/control-plane/src/a2a/card.rs),
  [docs/a2a-review-skill.md](../a2a-review-skill.md)): advertise `text/plain` as an accepted input
  mode for the *instruction*; add an example carrying a `text` instruction next to the `data` target;
  state plainly that text sets emphasis only — never target or scope.
- **Nothing else moves:** the submission gates (approval, permission, quota, `headSha`-required), the
  idempotency tuple, the trust boundary, and the completion artifacts are unchanged. The prompt has
  *always* been free text fed to the agent, so this adds **no new injection surface** — it only
  changes which field the free text arrives in.

### Why this is safe

The one property that matters: **natural language can change *what the review emphasises*, never
*what it targets or how widely it looks*.** Target (`repo`/`pr`/`headSha`) and scope (`baseSha`) are
structured and authoritative; a caller cannot, by writing prose, redirect a review onto a different
PR, invent a head commit, or silently flip a diff-scoped review into a whole-tree audit. The agent's
mediated-tool boundary ([ADR-0037](0037-agent-acts-via-mediated-tools.md)) and focused-review
scope ([ADR-0029](0029-focused-review-not-generic-runner.md)) are unchanged; the text lands in
`command_text`, exactly where `prompt` already did.

## Consequences

- **Good:** the skill accepts the message shape peers naturally send; a chat/agent client can drive a
  review with a sentence + a small structured target, instead of hand-building the whole object.
- **Good:** purely additive and backward compatible — `data`-only callers see no change; the new
  behaviour is opt-in by sending text.
- **Good:** the text-only branch becomes the natural, already-designed seam for Phase-4
  `input-required`, so this does not paint us into a corner.
- **Neutral:** a caller who puts target details *only* in prose ("review PR 128") still gets a
  (now-actionable) rejection until Phase 4 — by design, not omission.
- **Bad (minor):** two ways to express the instruction (`text` vs `data.prompt`) means a precedence
  rule to document and test; mitigated by the single clear rule (text wins, concatenated in order).

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| N1 | NL is misread and the agent emphasises the wrong thing | Medium | Low | NL only steers emphasis; it cannot change target/scope. A mis-emphasis is a weaker review, not a wrong-target or wrong-scope one — the same exposure `prompt` already carries |
| N2 | A caller expects prose to set the target ("review PR 128") and is surprised by the rejection | Medium | Low | The text-only error names the exact precise fields to supply and links the guide; Phase 4 replaces the rejection with a clarify-then-confirm loop |
| N3 | `text` and `data.prompt` conflict; ambiguity about which the agent sees | Low | Low | One documented rule: text present ⇒ text wins (concatenated in order); `data.prompt` is a lower-priority hint. Covered by unit tests when built |
| N4 | Prompt-injection via the `text` part | Low | Low | No new surface: the prompt has always been free text fed to the agent; the mediated-tool boundary (ADR-0037) and the fact that text cannot influence target/scope hold the line |
| N5 | Peers send the instruction as a `file` part or rich content | Low | Low | `file` parts stay reserved/ignored; only `text` and `data` are honoured. Revisit if a peer needs it |

## Out of scope

- **Extracting target fields from prose** (repo/PR/SHA resolution from NL) — rejected (Option B);
  the role has no forge credentials to resolve a head/base safely.
- **`input-required` clarify-then-confirm** — the Phase-4 upgrade of the text-only branch, gated on
  [ADR-0076](0076-restate-task-lifecycle-workflow.md) (Restate Phase B awakeables). Designed for, not
  built here.
- **A second `ask`/conversational skill** — this ADR is only about the `review` skill accepting an
  NL instruction, not a new skill. Rig's rejection for eaig-backed tool-use surfaces
  ([ADR-0075](0075-rig-for-new-agent-surfaces.md)) still stands for any future `ask` skill.
- **`file` part handling.**

## More Information

- [RFC-0006](../rfc/0006-a2a-agent-surface.md) — the A2A agent surface; this refines the `review`
  skill's input contract.
- [docs/a2a-review-skill.md](../a2a-review-skill.md) — the calling guide (data-part contract, the
  `headSha`-required and `baseSha`-scoping rules this ADR preserves).
- [ADR-0033](0033-inbound-command-parsing-and-run-kinds.md) — `command_text`/prompt, the field the NL instruction
  becomes.
- [ADR-0029](0029-focused-review-not-generic-runner.md) / [ADR-0037](0037-agent-acts-via-mediated-tools.md)
  — the scope + trust boundaries the NL instruction does not reopen.
- [ADR-0076](0076-restate-task-lifecycle-workflow.md) — the Restate Phase B awakeables that unblock
  the Phase-4 `input-required` upgrade of the text-only branch.
- PR #315 — the completion **context** part (effective SHAs + derived scope + review link) that lets
  a caller confirm what a (possibly NL-instructed) review actually looked at.
