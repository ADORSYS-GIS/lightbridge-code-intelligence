# ADR-0070: Window-proportional budgets for the injected static context blocks

- **Status:** Accepted
- **Date:** 2026-07-05
- **Deciders:** @stephane-segning

## Context and Problem Statement

A review run's prompt is assembled (`build_messages` in
`services/agent-runner/src/review/native/agent.rs`) from the system prompt plus five **static context
blocks** injected before turn 0:

| Block | Assembly-side bound | Source |
|---|---|---|
| Diff | `review.max_diff_chars` (60k default), file-boundary packed, cut files disclosed | #275 |
| SAST digest | one line per finding, `sast.max_findings` (25) | [ADR-0061](0061-sast-deterministic-finding-source.md) |
| Prior reviews | `PRIOR_BLOCK_CHAR_CAP = 8_000` (latest detailed, older compressed) | [ADR-0065](0065-re-review-dedup-and-reconciliation.md) |
| Repo memory (👎) | `LIMIT 30` one-liners | [ADR-0044](0044-feedback-memory-m1.md) |
| AGENTS.md instructions | `TOTAL_CAP = 32 KiB` | [ADR-0036](0036-auto-read-agent-instruction-files.md) |

Every block is *individually* bounded — that part was already right (a "10 prior reviews" run does not
pile up; the newest is detailed and the other nine are one line each inside the 8k cap). The problem is
that **every bound is an absolute constant tuned for today's ~1M-token-window models**, and none is
relative to the window the run actually has. Point a tier at a small-window model (a cheap local model,
a 32k research alias) and the static blocks can consume the whole window before the review starts — the
60k-char diff cap alone is ~15k tokens, and a full SAST digest + priors + memory + instructions add
another ~10k. The model then overflows (or the ADR-0045 wind-down fires on turn 0) with no warning: the
assumption "the window is huge" is baked into five separate constants and written nowhere an operator
looks. Same failure family as [ADR-0069](0069-review-tier-minimum-model-capability.md) (a capability
assumption implicit in the config) and the #275 diff-budget waste (the prompt must carry the *right*
bytes, sized to what the model can actually hold).

## Decision

Derive each static block's char budget from the configured context window when one is set, keeping the
existing absolute constants as **ceilings** the window-share can only lower.

`PromptBudgets::for_review(review)` computes, per block, `min(absolute ceiling, share-of-window)`,
floored at `MIN_BLOCK_CHARS = 1_000`:

| Block | Window share | Ceiling (= today's constant) |
|---|---|---|
| Diff | 25% | `review.max_diff_chars` |
| AGENTS.md instructions | 2% | 32 KiB |
| Prior reviews | 2% | 8k |
| SAST digest | 1.5% | 6k |
| Repo memory | 1% | 4k |

Shares sum to ~31.5% of the window for all static context, leaving the rest for the system prompt, the
conversation, and the ADR-0045 wind-down headroom. The share is computed in chars as
`window_tokens * fraction * PROMPT_CHARS_PER_TOKEN` (4 chars/token — the same deliberate over-estimate
as the ADR-0045 `estimate_tokens` heuristic, erring toward *smaller* budgets, which is what a safety cap
wants).

Properties:

- **No window configured → the ceilings apply unchanged.** `review.context_window` is `None` by default
  ([ADR-0045](0045-context-window-budget.md)); every existing deploy behaves exactly as before — zero
  behaviour change until a window is set.
- **A large window → still the ceilings.** On a 1M window, 25% is 262k tokens ≫ the 60k-char diff cap, so
  `min()` returns the ceiling. The mechanism only *shrinks*; it can never grow a block past the bound its
  assembly side already enforces.
- **A small window → proportional shares.** On a 32k window the diff budget becomes 32k chars (25%) and
  the priors block 2.6k chars (2%) — the blocks shrink together instead of the diff alone eating the
  window.
- **Never silent.** Each block is cut by `cap_prompt_block`, which trims on a line boundary and appends an
  explicit `… [<block> truncated to fit the model's context window — N of M chars shown]` marker — the
  same never-truncate-silently rule as the diff packing (#275) and the prior-reviews block (ADR-0065).
  The diff already had this (its own disclosure block naming every unshown file); this extends it to the
  other four.
- **Floored, not nuked.** `MIN_BLOCK_CHARS` guarantees a shrunk block keeps its header + a few lines + the
  marker, so the *framing* ("this is untrusted", "don't re-report SAST") is never dropped along with the
  content — dropping the framing would be worse than dropping the content.
- **Observable.** When the window shrinks any block below its ceiling, the run logs one
  `prompt budgets: window-proportional caps active` line with the resolved char budgets, so a
  small-window deploy is legible from the pod log (as ADR-0069's disclosure and ADR-0060's telemetry are).

Reuses the existing `review.context_window` knob ([ADR-0045](0045-context-window-budget.md)) — the one
that already drives wind-down/trim — so there is **no new config surface and no chart/values change**.
The operator sets the window to the serving model's real size (which they should for ADR-0045 anyway),
and the prompt budgets follow automatically.

## Consequences

- **Good:** a small-window model no longer silently overflows before the review starts; the blocks
  degrade together and disclose what they cut; the mechanism is invisible on today's large-window prod
  models; no new config to reason about. The window-is-huge assumption is now enforced in code, not
  scattered across five constants.
- **The shares are fixed fractions, not per-block knobs.** If a specific block needs a different split on
  a specific tier, that is a follow-up (a `review.<tier>.promptBudget.*` map, same shape as
  `max_diff_chars`) — deferred until a concrete need appears; the fixed shares cover the general case.
- **The 4 chars/token heuristic is approximate.** It over-estimates (smaller budgets) on purpose; a model
  whose real ratio is richer just gets slightly more headroom than necessary — the safe direction.
- **This is a cap, not a summarizer.** A genuinely huge prior-reviews history on a small window is *cut*,
  not intelligently compressed; the newest review's detail is what survives (the ADR-0065 ordering already
  puts it first), which is the highest-signal part.

## Alternatives considered

- **Leave the absolute constants (status quo).** Rejected — it is the latent overflow this ADR removes;
  correct only by accident of the current models all having ~1M windows.
- **Per-block operator knobs instead of window-derived shares.** More flexible, more config to tune and
  keep coherent, and it still lets an operator misconfigure a block larger than the window. The
  window-derived share is self-correcting and needs no new surface; per-block knobs can layer on later if
  needed.
- **A single global "static context ≤ X% of window" budget shared across blocks.** Simpler to state, but
  it needs a priority order to divide a shared pool under contention (which block yields first?) — more
  mechanism than fixed per-block shares, for no clear gain at this scale.
- **Drop low-signal blocks entirely on a small window** (e.g. no SAST digest under 32k). Rejected — the
  blocks carry real signal (SAST is the only deterministic finding source); shrink-and-disclose keeps the
  signal proportional instead of making a binary on/off cliff.

## References

- [ADR-0045](0045-context-window-budget.md) — the context-window budget + `estimate_tokens`; this reuses
  its `context_window` knob and chars/token heuristic.
- [ADR-0065](0065-re-review-dedup-and-reconciliation.md) — the prior-reviews block cap this generalizes.
- [ADR-0061](0061-sast-deterministic-finding-source.md) — the SAST digest, one of the capped blocks.
- #275 — file-boundary diff packing + disclose-what-was-cut; the posted-output honesty precedent this
  extends to the other blocks.
- [ADR-0069](0069-review-tier-minimum-model-capability.md) — sibling: an implicit model-capability
  assumption made explicit; this does the same for the window-size assumption.
