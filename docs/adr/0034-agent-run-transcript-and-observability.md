# ADR-0034: Persist the agent run transcript (tool calls, reasoning, tokens) and surface it

- **Status:** Superseded by [ADR-0100](0100-retire-db-transcript-logs-as-observability.md) (2026-07-18, epic #459)
- **Date:** 2026-06-22

> **Superseded (2026-07-18, epic #459 pivot).** The `agent_transcript` DB table and its ingest/read
> API were **removed** — see [ADR-0100](0100-retire-db-transcript-logs-as-observability.md). On the
> OpenCode-over-ACP review path we do not own the agent loop, so the recorder→transcript
> reconstruction was a post-hoc record, not a replay/resume seam; its only reader was the `clients/lci`
> TUI Run Detail view, also removed. **Loki logs are now the single run-observability surface.** This
> ADR is kept for historical context; the table (migrations `0014`/`0017`) is dropped by `0032`.

## Context and Problem Statement

A review run persists **only the final findings** (`reviews.findings` JSONB,
`migrations/0009_reviews.sql`). Nothing about *how* the agent got there is stored: no tool calls, no
tool results, no model reasoning, no token usage, no per-turn messages. stdout gets a one-line summary
(`agent-runner/src/main.rs`); there are no `tracing` spans around model/tool calls and no OTel. The
dashboard run page can only show findings because that is all that exists.

This blocks debugging ("why did it review the wrong thing?"), trust ("show me what it actually looked
at"), cost attribution, and the feedback loop ([ADR-0035](0035-review-feedback-signal.md)). The native
agent loop ([ADR-0026](0026-native-review-agent.md)) is the moment to capture this — the loop is ours,
and the data is in hand before it's discarded.

## Decision Drivers

- **Auditability/trust:** the user wants to see *all tool uses and the LLM's reasoning* in the UI.
- **Debuggability:** reconstruct a run turn-by-turn when output is wrong.
- **Cost visibility:** token usage per run.
- **Capture where it's free:** inside the native loop, not by scraping a subprocess.

## Considered Options

- **A. Persist a structured transcript in Postgres** (turns + tool invocations + token usage) and serve
  it to the dashboard; add `tracing` spans for live ops.
- **B. Logs only** — emit structured stdout/`tracing` events, rely on the cluster log stack; no DB, no UI.
- **C. External tracing backend** (OTel → Tempo/Jaeger) and link out from the dashboard.

## Decision Outcome

Chosen option: **A, with the `tracing` spans of B as a complement.** During the native loop the runner
records each step and posts it to the control plane:

- **`agent_turns`** — `(task_id, turn_no, role, content, finish_reason, created_at)`: the model's
  assistant turns including reasoning/explanatory text.
- **`tool_invocations`** — `(task_id, turn_no, tool_call_id, tool_name, arguments_json, result_json,
  duration_ms, created_at)`: every `vector_semantic_search` / `graph_*` / control-tool call with
  args + result.
- **Token usage** on the run record: `input_tokens`, `output_tokens` (and cost if the provider returns
  it).

A new endpoint `GET /tasks/{id}/transcript` serves the turn-by-turn breakdown, **enforcing the same
repository-level access control as the rest of the dashboard** — the caller must have read access to the
task's repository before any transcript is served, since it contains repo code, file paths, and tool
results. The dashboard run page
([ADR-0016](0016-dashboard-information-architecture.md)) gains an **expandable "Agent reasoning"
timeline** (turns + tool calls with latency) and a token/cost summary. Wrap loop steps in `#[instrument]`
spans so live operations are traceable even before a row is written.

Capture is **best-effort and non-blocking**: failing to persist a transcript step never fails the
review (mirrors the existing non-fatal review posture).

### Consequences

- Good: full audit trail; debug wrong reviews; cost per run; foundation for feedback
  ([ADR-0035](0035-review-feedback-signal.md)) and quality analysis.
- Bad: more write volume (tool args/results can be large — cap/truncate payloads); two new tables +
  endpoint + a non-trivial UI; PII/secret-leak care since tool results contain repo code (already
  inside our trust boundary, but transcript retention needs a policy).
- Neutral: depends on the native loop landing ([ADR-0026](0026-native-review-agent.md)); the OpenCode
  path can't cleanly produce this, which further motivates the cutover.

## Pros and Cons of the Options

### A. Structured transcript in Postgres + spans (chosen)
- Good: queryable, UI-renderable, owned; powers feedback + cost.
- Bad: write volume + payload capping; schema + UI work.

### B. Logs only
- Good: cheapest; nothing to store.
- Bad: no per-run UI, no correlation, ephemeral; doesn't satisfy "show it in the UI".

### C. External OTel backend
- Good: best for live distributed tracing/latency.
- Bad: another system to run; not a per-run user-facing artifact; weak for "show this user this run's
  reasoning". (Can be added later alongside A.)

## More Information

- Builds on [ADR-0026](0026-native-review-agent.md) (the loop). Schema near
  `migrations/0009_reviews.sql`; API in `control-plane/src/http/internal.rs` +
  `control-plane/src/queue/tasks.rs`; UI in `apps/web/components/runs/` +
  `apps/web/app/dashboard/runs/[id]/page.tsx`. Retention/truncation policy TBD in the implementing RFC.
