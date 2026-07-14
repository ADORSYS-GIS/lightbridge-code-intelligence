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
- Epic [#137](https://github.com/adorsys-gis/lightbridge-code-intelligence/issues/137) — native review agent (proof-of-work).
- [#247](https://github.com/vymalo/lightbridge-code-intelligence/issues/247) — raw-SSE verification of the streamed reasoning key (2026-07-14 update above).
- [#411](https://github.com/vymalo/lightbridge-code-intelligence/issues/411) — follow-up: root-cause `reasoning_chars: 0` on real deep-tier turns (opened from this update).
