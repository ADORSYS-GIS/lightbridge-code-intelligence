# ADR-0039: Agent LLM resilience & observability — timeout, bounded retry, circuit breaker, failover

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** @stephane-segning

## Context and Problem Statement

The native review agent ([ADR-0026](0026-native-review-agent.md) / [ADR-0037](0037-agent-acts-via-mediated-tools.md))
drives the eaig LLM gateway one chat round-trip per turn (`MAX_TURNS = 16`). In production two
failure modes made a failed review **illegible and brittle**:

1. **No legible failure reason.** The transport used `reqwest::Client::new()` (no timeout) and
   `.error_for_status()`, which **discards the HTTP response body**. A gateway rejection (unknown
   model, quota, validation error) surfaced as a bare status code with no message — "the review
   failed without saying why."
2. **No resilience.** A single transient blip (a 503 from the gateway, a dropped connection, a 429)
   failed the whole turn and the run. There was no timeout at all, so a wedged connection could hang
   indefinitely; and there was no retry, so a momentary upstream hiccup wasted a Job.

Separately, a run was hard to follow from pod logs: the model id was never logged, and there was no
per-turn line (tokens, latency, which tools were called) — you had to open the persisted transcript
([ADR-0034](0034-agent-run-transcript-and-observability.md)) to see anything.

A hard constraint shapes the policy: **eaig can legitimately take up to ~2 minutes to answer one
turn** (large context + a slow/expensive model). So any timeout must be *generous* — an aggressive
timeout would kill a slow-but-valid response, which is worse than the problem it solves.

## Decision Drivers

- **Legible failures.** A failed review must say *why* in the logs/error, not just a status code.
- **Proof-of-work from logs alone.** Which model, which gateway, per-turn tokens/latency/tools —
  without opening the transcript or the dashboard.
- **Survive transient blips** (connection resets, 429, 5xx) without wasting a Job, but **fail fast** on
  deterministic errors (4xx) and on a chain that's clearly down.
- **Respect the 2-minute reality.** Generous timeout; never kill a slow-but-valid turn.
- **Fail-closed config philosophy** ([ADR-0037](0037-agent-acts-via-mediated-tools.md)): new knobs are
  optional with safe defaults, so the existing deploy works **without** an ai-helm values change.
- **Right scope for the breaker.** The Job is ephemeral (one task per process), so a *per-run /
  per-process* circuit breaker is correct — no cross-process / distributed breaker to build or operate.

## Decision Outcome

Add a resilience + observability layer to the chat transport and the agent loop:

### 1. Capture the HTTP error body (the key legibility fix)

`complete()` no longer uses `.error_for_status()`. On a non-2xx it reads the response body (bounded to
the first ~1 KB, on a char boundary) and folds it into the returned error alongside the status. A
gateway rejection now reads e.g. `chat completions API returned 400 Bad Request: {"error":{"message":
"unknown model 'x'"}}`.

### 2. Per-request timeout — **default 180s**

The chat `reqwest::Client` is built with a per-request timeout, default **180 seconds**, configurable
via `LLM_REQUEST_TIMEOUT_SECS` / `review.request_timeout_secs`. The default is deliberately generous
because eaig can take ~2 minutes per turn; this only kills a *wedged* request, not a slow-but-valid one.

### 3. Bounded retry with backoff + circuit breaker

Retries fire **only on transient failures**: connect/timeout transport errors, HTTP **429**, and HTTP
**5xx**. A 4xx other than 429 is deterministic (bad request, auth, unknown model) and is **never**
retried. Backoff is jittered exponential (`base · 2^attempt`, capped), with the jitter **deterministic**
(seeded by the attempt index — no `SystemTime::now`/RNG, so the schedule is reproducible in tests). A
429's `Retry-After` (integer seconds) is honoured over the computed backoff when present. Retries are
capped at **2** (→ 3 attempts), configurable via `LLM_MAX_RETRIES` / `review.max_retries`.

A **per-run circuit breaker** counts *consecutive* turn-failures; after **3** (configurable via
`LLM_CIRCUIT_BREAKER_THRESHOLD` / `review.circuit_breaker_threshold`) the run fails fast rather than
burning the whole 16-turn budget against a chain that's down. It resets on the first turn that
produces a model reply. Scope is per-process by design (the Job is ephemeral).

### 4. Optional failover to a secondary model

If `LLM_FALLBACK_MODEL` / `review.fallback_model` is set, the loop fails over to that model (same
gateway/key/timeout) for a turn once the primary exhausts its *transient* retries. Unset → single-model
behaviour, unchanged. Failover is logged clearly.

### 5. Structured logging (proof-of-work)

- **At review start:** model id, fallback model (or `(none)`), base-URL **host only** (path/key kept
  out of logs), and the resilience policy (timeout, retries, breaker threshold).
- **Per turn:** turn index, the tool names called, prompt/completion token counts, and the
  wall-clock turn latency (`latency_ms`).
- **Per tool dispatch:** the tool name; for the mediated write tools (`add_review_comment` /
  `add_comment`) a note that a finding/reply was buffered.

Lines are concise (one per turn + one per call) — the full payloads already live in the transcript
([ADR-0034](0034-agent-run-transcript-and-observability.md)).

### Consequences

- **Good:** failures are legible (status + body); a run is followable from pod logs alone; transient
  blips no longer waste a Job; a wedged request can't hang forever; a down chain fails fast instead of
  grinding through 16 turns; optional failover adds availability without complexity when unused. All
  defaults are safe, so **no ai-helm change is required** to deploy.
- **Bad / accepted trade-off:** a generous 180s timeout means a genuinely wedged request still ties up
  the turn for up to 3 minutes before the first retry — accepted, because the alternative (a tight
  timeout) kills valid slow responses, which is worse. The breaker is per-process only (correct for an
  ephemeral Job, but offers nothing across Jobs — out of scope).
- **Unchanged:** the task state-machine semantics are **not** touched — a failed review still leaves
  the task succeeded (indexing already landed); this ADR only makes the failure *legible* (better logs
  + error detail). Reworking that "review failed → task succeeds" behaviour is a separate investigation.

## Operator configuration (cross-repo, all optional)

These new keys live in **ADORSYS-GIS/ai-helm** (`config.*`, injected as env into the agent Job). All
optional — unset uses the safe default, so an existing deploy keeps working:

| Key (env) | File config | Default | Meaning |
| --- | --- | --- | --- |
| `LLM_REQUEST_TIMEOUT_SECS` | `review.request_timeout_secs` | `180` | Per-request timeout (s). Generous on purpose. |
| `LLM_MAX_RETRIES` | `review.max_retries` | `2` | Retries on transient failure (total attempts = +1). |
| `LLM_CIRCUIT_BREAKER_THRESHOLD` | `review.circuit_breaker_threshold` | `3` | Consecutive turn-failures before failing fast. |
| `LLM_FALLBACK_MODEL` | `review.fallback_model` | _(unset)_ | Secondary model to fail over to; unset = single model. |

## More Information

- Transport + loop: `services/agent-runner/src/review/native/{chat,agent}.rs`; config knobs:
  `services/agent-runner/src/bootstrap/config.rs` (numeric-string-tolerant deserializers in the shared
  `lci-config` crate).
- Builds on [ADR-0026](0026-native-review-agent.md) (native loop),
  [ADR-0037](0037-agent-acts-via-mediated-tools.md) (mediated tools; prompt/model as operator config),
  [ADR-0038](0038-per-repo-review-model.md) (per-repo model), and
  [ADR-0034](0034-agent-run-transcript-and-observability.md) (transcript — the deep record this logging
  complements).
- Source of truth: the maintainer's observability + resilience requirements for the review agent, a
  follow-up under epic **#137** ("Trustworthy review agent v2 — … observability & feedback"). A
  dedicated tracking ticket should be filed and linked.
