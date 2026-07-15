# ADR-0060: Capture the model's reasoning (proof-of-work) + the GLM-5.2 latency finding

- **Status:** Accepted
- **Date:** 2026-06-27
- **Deciders:** @stephane-segning

## Context and Problem Statement

Reviews on **GLM-5.2** (DeepInfra, served as `adorsys-reviewer-pro`) are slow — multi-minute turns —
and we could not say *why* from the logs. Three blind spots:

1. The per-turn log ([ADR-0039](0039-agent-llm-resilience-and-observability.md)) printed
   `reasoning_tokens: -1`. The accessor only read `usage.completion_tokens_details.reasoning_tokens`,
   but the gateway reports the reasoning slice at the **top level** of `usage` — so we always logged the
   `None` sentinel.
2. The model's actual chain-of-thought (`reasoning_content`, DeepSeek/GLM lineage) was **parsed and
   discarded** on both the streaming and non-stream paths (`#[allow(dead_code)]`). The "agent reasoning"
   log line actually logged the *visible answer*, not the thinking.
3. Whether the configured reasoning budget (`review.extra.reasoning_effort = "low"`) was **actually on
   the wire** was unprovable from a running pod — the startup log didn't echo the `extra` in force.

The result: "is GLM-5.2 over-thinking, and is `reasoning_effort: low` even applied?" was unanswerable
from observability alone. This blocked any informed tuning or model decision.

## Decision

**Make the model's reasoning a first-class, logged signal, and prove the applied reasoning budget.**

- **Capture `reasoning_content`** on both transports into a new `Completion::reasoning` field —
  reassembled from SSE deltas (streaming) or read off the message (non-stream). It is kept **off**
  `ChatMessage` on purpose: it is for transcript/logs only and is **not** echoed back to the model on
  the next turn.
- **Read the top-level `usage.reasoning_tokens`** as a fallback to the nested OpenAI-style field, so the
  count is no longer silently lost.
- **Log per turn:** `reasoning_chars` on the `agent turn complete` line (the reliable "how far did it
  think" magnitude even when the gateway folds reasoning into `completion_tokens` and reports
  `reasoning_tokens: 0`), plus the chain-of-thought itself on the `agent reasoning` line, bounded by a
  new `REASONING_LOG_CHARS` env (default `4000`; `0` = unbounded) — the old 600-char cap was too narrow.
- **Log the active `review.extra`** at agent start, so a run proves *from its own logs* which reasoning
  budget was applied, not just which one the ConfigMap claims.

This is an **observability** change. It does **not** change the model; the model lever stays
`review.model` in `ai-helm-values` (one-line, no rebuild — [ADR-0051](0051-per-model-config.md)).

## Finding (in-prod gateway tests, 2026-06-27)

Direct calls to the prod gateway (`adorsys-reviewer-pro` → `zai-org/GLM-5.2`; `adorsys-reviewer` →
`MiniMaxAI/MiniMax-M2.7`), confirming the instrumentation above:

| Model | Request | Wall-clock | Completion tokens | ≈ tok/s | Cost ($) |
|---|---|---:|---:|---:|---:|
| GLM-5.2 | greeting, **default** effort | **4m02s** | 219 | ~0.9 ⚠️ | 0.00068 |
| GLM-5.2 | trivia, `reasoning_effort: low` | 35.6s | 501 | ~14 | 0.00153 |
| GLM-5.2 | VAT calc, `reasoning_effort: low` | 53.6s | 616 | ~11.5 | 0.00189 |
| **MiniMax-M2.7** | VAT calc, no effort param | **13.8s** | 643 | **~47** | **0.00066** |

Conclusions:

- **`reasoning_effort: low` *is* applied by the runner** — it is not a reserved key, so it survives
  `with_extra` and is `#[serde(flatten)]`-ed into every body (covered by a serialization test, and now
  visible in the startup log). It cuts GLM-5.2 wall-clock ~4× (4m → ~35–53s) on these prompts.
- **GLM-5.2 on DeepInfra is simply slow and verbose**: ~11–15 tok/s even at `low`, and it folds its
  thinking into `completion_tokens` (reporting `reasoning_tokens: 0`). A prod review turn of ~7k
  completion tokens at that rate ≈ 500s — matching the multi-minute turns observed live.
- **MiniMax-M2.7 is ~3–4× faster *and* ~⅓ the cost** for equivalent output. This is precisely the
  "reopen" trigger named in [ADR-0054](0054-review-model-and-provider-selection.md), whose decision was
  to **stay on M2.7** — yet prod has since drifted to GLM-5.2. This ADR records the data; the model
  decision is a separate, operator-owned `review.model` change.

> ⚠️ The 4m02s / ~0.9 tok/s default-effort row is an outlier (likely a DeepInfra cold-start / queue
> spike at that moment), not a representative decode rate. The `low`-effort rows are the steady state.

## Update (2026-07-14): raw-SSE verification of the streamed reasoning key (#247)

[#237](https://github.com/vymalo/lightbridge-code-intelligence/issues/237) added
`#[serde(default, alias = "reasoning")]` to `reasoning_content` on both the non-stream
(`wire.rs::ResponseMessage`) and streaming (`stream.rs::StreamDelta`) DTOs, as a guess at why
streamed GLM-5.2 turns logged `reasoning_chars: 0` while non-stream turns logged thousands. It was
never checked against a raw SSE sample from the gateway. [#247](https://github.com/vymalo/lightbridge-code-intelligence/issues/247)
closes that gap.

**Method:** two ephemeral, short-lived diagnostic Jobs (`curlimages/curl`, deleted immediately
after) in the `lightbridge-agents` namespace, reusing the same `lightbridge-agent-secrets`
(`llm-base-url`/`llm-api-key`) and `lightbridge-agent-ca` the real agent-runner Jobs use — so the
request hit the real internal gateway with the real deep-tier params (`model: glm-5p2`,
`stream: true`, `reasoning_effort: high`), once with a plain prompt and once with a tool offered
(mirroring a review turn's `tool_choice: "auto"`). The raw `data: {...}` bytes were captured before
any deserialization.

**Finding: the gateway streams reasoning under `reasoning_content` — never `reasoning`.** Every
delta in both probes carried it under its primary (non-aliased) name, e.g.:

```
data: {"...,"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":"1",...
data: {"...,"choices":[{"index":0,"delta":{"...,"reasoning_content":" me read the file","tool_calls":null},...
data: {"...,"choices":[{"index":0,"delta":{"...,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_b63f","function":{"arguments":"{\"path\": \"src/main.rs\"}","name":"read_file"},"type":"function"}]},"finish_reason":null}],"usage":null}
```

This held for a plain text turn (315 completion tokens, real chain-of-thought streamed the whole
way) **and** for a tool-call turn (model reasons briefly — `"Let me read the file src/main.rs to
check for bugs."` — then emits the `read_file` call). `usage.reasoning_tokens` was `0` in both,
confirming the ADR's existing note that this gateway folds reasoning into `completion_tokens` and
reports the token breakdown as `0` regardless — `reasoning_content` length remains the only
reliable signal.

**Conclusion on the ticket's literal question: the `#[serde(alias = "reasoning")]` is confirmed
harmless and correctly shaped, but was never the actual mechanism.** This gateway/model pairing has
always emitted `reasoning_content` under its primary name; the alias is dead code for this
provider (kept as cheap defensive coverage for a provider that does use the other name, per the
original #237 rationale — no reason to remove it).

**Open question the ticket surfaced, not resolved by this verification:** a real deep-tier review
run captured during this investigation (task `5a82c172-…`, PR
[#409](https://github.com/vymalo/lightbridge-code-intelligence/pull/409), image
`sha-71261f6` — which already contains the #237 alias) logged `reasoning_chars: 0` on **all 7**
`agent turn complete` lines, including turns that (per the probes above) should be entirely capable
of carrying `reasoning_content`. So the original symptom the alias was meant to fix **still
reproduces in production**, via some mechanism other than the wire field name — plausibly something
specific to the much larger real request (a ~23k-token system prompt + diff + 20 tools, versus the
probes' single short prompt and ≤1 tool). This is outside #247's scope (verifying the serde key) and
is **not** root-caused here; see
[#411](https://github.com/vymalo/lightbridge-code-intelligence/issues/411) for the follow-up
investigation. Per this repo's refactor/investigation discipline, a new spike is filed rather than
guessing at a fix in this same change.

## Update (2026-07-14): dimension-isolation probes find no request-shape cause (#411)

[#411](https://github.com/vymalo/lightbridge-code-intelligence/issues/411) was filed from the update
above to isolate which dimension of a large real request — system-prompt size, tool count, or
turn/message-history depth — suppresses `reasoning_content` capture, since #247's minimal-prompt
probes could not explain PR [#409](https://github.com/vymalo/lightbridge-code-intelligence/pull/409)
logging `reasoning_chars: 0` on all 7 turns.

**Method:** seven ephemeral `curlimages/curl` Jobs in `lightbridge-agents` (same secrets/CA pattern as
#247, deleted after each probe), each varying exactly one dimension against the real deep-tier config
(`model: glm-5p2`, `stream: true`, `reasoning_effort: high`). Critically, this round used the **actual
production system prompt and tool schemas**, not synthetic approximations: the `review-system.md` and
`agent.json` deep-tier config were read directly off the live `lightbridge-agent-config` ConfigMap
(`kubectl -n lightbridge-agents get configmap lightbridge-agent-config`), and the 10 real built-in tool
specs were taken verbatim from `services/review-agent/src/tools/*.rs` (padded to ~20 with
similarly-shaped synthetic `mcp__<server>__lookup` specs to match production's external-knowledge-tool
count, since live MCP discovery wasn't reproduced). The real `review-system.md` is **25,302 chars**
(≈6.3k tokens at the codebase's own `PROMPT_CHARS_PER_TOKEN` estimate) — smaller than the ticket's
"~23k tokens" estimate, which likely conflated the file size with the full assembled prompt (system +
tool-protocol + diff + tool schemas).

| Probe | Shape | Prompt tokens | `reasoning_content` |
|---|---|---:|---|
| A (baseline) | Minimal prompt, 0 tools, trivial question | ~50 | **Present** — extensive (matches #247) |
| B | Minimal prompt, 20 tools, trivial "don't call tools yet" question | 2,171 | **Absent** — 0 chars, direct terse answer |
| C | Real `review-system.md` + tool-protocol, 0 tools | 6,260 | **Present** — 325 chars |
| D | Real prompt + real diff sample + 20 tools, investigative task (turn 1) | 20,775 | **Present** — 370 chars, 3 tool calls |
| E | Same conversation as D, turn 2 (post tool-result) | 21,147 | **Present** — 603 chars, 2 tool calls |
| F | Real prompt + a ~260KB diff (near the deep tier's 300k-char ceiling) + 20 tools | 75,678 | **Present** — 770 chars, 3 tool calls |
| G | Minimal prompt, 20 tools, forced investigative task (must call a tool) | 2,175 | **Present** — brief, 33 chars, 1 tool call |

**Finding: none of the three hypothesized dimensions reproduce the symptom.** Prompt size (up to
~76k tokens, near the deep tier's real ceiling), tool count (20, matching production), and turn depth
(turn 1 → turn 2 of the same conversation) all left `reasoning_content` streaming normally whenever the
turn's task was genuinely investigative — which every real review turn is. The **only** reproducible
zero (probe B) correlates with **task triviality**, not tool count: probe G shows that offering the
same 20 tools to a task that actually requires investigation still produces reasoning (if briefer, 33
chars) — so probe B's zero is the model choosing not to think for a question it can answer in one
clause, not a wire-level suppression. That pattern doesn't fit PR #409's incident either, since a real
review's turns are inherently investigative (the `finish` verdict turn is the closest analog and still
wouldn't explain all 7 being zero, only possibly the last).

**Conclusion on the ticket's acceptance criteria:** the request shape is **not** the mechanism —
`reasoning_content` deltas are present, off the real wire, for every faithful reconstruction of
production's actual size/tool-count/turn-depth, including well past where the hypothesis predicted
suppression. This rules out a structural, request-shape-driven code defect in
`stream.rs::StreamDelta`/`collect_stream` or `agent-runner/src/review/transcript.rs`'s
`reasoning_chars` line — both remain verified correct, now under realistic load as well as #247's
isolated probes. PR #409's specific 7-turn zero streak remains **unexplained** but, given it could
not be reproduced despite deliberately exceeding its own request's scale on every axis, is most
plausibly a **one-off gateway/provider-side anomaly** (transient backend routing or degraded
behavior at that moment) rather than a recurring
systemic bug — there is nothing here to fix. Since the original raw wire data for that specific
incident was never captured (only the aggregate `reasoning_chars: 0` log line survived), a recurrence
today would be equally undiagnosable;
[#417](https://github.com/vymalo/lightbridge-code-intelligence/issues/417) proposes the lightweight
observability (a correlation id or bounded raw-sample logged only when `reasoning_chars == 0`) needed
to actually root-cause it if it happens again, rather than guessing further. #411 is left open,
linked to #417.

## Update (2026-07-15): the `agent reasoning` line was silently dropped — restored, and `agent content` added (#411)

Two independent problems surfaced after #411's capture fix (PR #425) landed and deep-tier
`reasoning_chars` finally went non-zero in prod:

1. **Capture bug (fixed in #425, separate PR):** the *streaming* deserializer dropped every delta
   carrying an explicit `"tool_calls": null` (GLM-5.2, MiMo), because `#[serde(default)]` on a
   non-`Option` `Vec` rejects an explicit `null`. That is why `reasoning_chars: 0` persisted on every
   deep-tier turn even though the wire carried reasoning — the earlier curl probes above tested the
   wire, never the Rust deserializer.

2. **Visibility regression (this ADR's concern):** the `agent reasoning` log line specified here — the
   one that logs the *actual chain-of-thought text*, bounded by `REASONING_LOG_CHARS` — was **lost**
   when `agent-runner`'s review path was split into `review/transcript.rs` (the "split god-files, no
   behavior change" pass #395, and the #423 telemetry-on-the-turn rewrite). The reconstructed code kept
   only the `reasoning_chars` **count** on `agent turn complete` and threw the text away again. So a
   maintainer tailing a pod saw `reasoning_chars: 11675` — proof the data survived the parser, but not
   a single character of what the model actually thought. A char count is not proof-of-work.

**Restored and extended:** `append_transcript` again emits the per-turn `agent reasoning` line (the
chain-of-thought text, bounded by `REASONING_LOG_CHARS`, default 4000, `0` = unbounded), and now a
symmetric `agent content` line (the model's *visible answer* for the turn, bounded by a new
`CONTENT_LOG_CHARS`, same semantics). Both are load-bearing for an operator and were requested as
distinct signals: reasoning shows *how* the model got there, content shows *what* it concluded. Both
skip cleanly on a pure tool-call turn (no prose). Reasoning stays off `ChatMessage` (never echoed to
the model) exactly as this ADR requires — it is a logs-only signal. This closes the visibility half of
#411 (capture in #425, visibility here); persisting reasoning to the DB transcript / UI remains the
still-open follow-up noted under Consequences.

## Consequences

- **Good:** a run's reasoning is now legible from a pod log tail and measurable per turn; the applied
  reasoning budget is provable; the `reasoning_tokens` count is no longer dropped.
- **Logs-only (for now):** reasoning is **not** persisted to the DB transcript ([ADR-0034](0034-agent-run-transcript-and-observability.md)) —
  that needs a control-plane handler + migration. A follow-up if we want it in the UI proof-of-work.
- **Cost / limits:** full chain-of-thought can be verbose; `REASONING_LOG_CHARS` bounds the live log
  (the magnitude is always logged via `reasoning_chars`).
- **Reopened then re-settled** [ADR-0054](0054-review-model-and-provider-selection.md): the data above is
  the latency trigger that ADR named. Acting on it, the operator **reverted prod to `adorsys-reviewer`
  (MiniMax-M2.7)** on 2026-06-27 — a `review.model` change in `ai-helm-values`, no rebuild — which
  realigns prod with ADR-0054's standing decision and resolves the GLM-5.2 drift. This ADR's instrumentation
  stands regardless of the model in force.

## References

- [ADR-0034](0034-agent-run-transcript-and-observability.md) — the run transcript this reasoning will (later) feed.
- [ADR-0039](0039-agent-llm-resilience-and-observability.md) — per-turn structured logging this extends.
- [ADR-0045](0045-context-window-budget.md) — the context budget (`context_window`, separate knob).
- [ADR-0051](0051-per-model-config.md) — per-model config; the `review.model` / `review.extra` levers.
- [ADR-0054](0054-review-model-and-provider-selection.md) — model & provider selection (reopened by the finding).
- Epic [#137](https://github.com/vymalo/lightbridge-code-intelligence/issues/137) — native review agent (proof-of-work).
- [#247](https://github.com/vymalo/lightbridge-code-intelligence/issues/247) — raw-SSE verification of the streamed reasoning key (2026-07-14 update above).
- [#411](https://github.com/vymalo/lightbridge-code-intelligence/issues/411) — follow-up: root-cause `reasoning_chars: 0` on real deep-tier turns; dimension-isolation probes documented in the second 2026-07-14 update above. Left open, linked to #417.
- [#417](https://github.com/vymalo/lightbridge-code-intelligence/issues/417) — follow-up: log a correlation id / bounded raw-sample when `reasoning_chars == 0` recurs, so a future incident is diagnosable (opened from #411's update).
