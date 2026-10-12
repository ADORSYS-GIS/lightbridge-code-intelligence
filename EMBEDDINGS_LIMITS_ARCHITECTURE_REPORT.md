# Why embedding-provider limits keep breaking indexing, and the cleaner fix

## 1. The pattern, not just the one bug

PR #636 fixes one specific bug (a single line can bypass the chunk-size cap). But it's the
**fourth** time this general class of failure has hit production, each time discovered only after
a real indexing task failed, each time patched with a new static number rather than a change to
how the system decides what "too big" means:

| # | When | Symptom | What was actually wrong | Fix |
|---|---|---|---|---|
| 1 | PR #619 | `embedding batch 27`: 400 "prompt too long: 42774, model maximum context length: 40959" | No chunk had a byte cap at all — a `ROADMAP.md` window came out at 160KB | Added `max_chunk_bytes` (16,000 default) |
| 2 | (mid-session, this repo) | Embeddings gateway `500` / `response_payload_too_large` after the provider switched Fireworks → DeepInfra | `INDEX_EMBED_BATCH_SIZE` (20) produced **response** payloads too large for the new provider — a limit on a completely different axis (output size, not input size) | Reduced batch size 20 → 5 |
| 3 | This session, `cratestack` | Indexing job killed by K8s after exactly 1 hour, 5 attempts, never finishing | Not a limit bug, but the *same root cause* — no config for "does this job actually fit in the time/throughput budget" for a repo this size | (separate report — infra deadline, not covered here) |
| 4 | This session, PR #636 | `embedding batch 62`: 422 "input sequence should have less than 131072 characters, Input length: 191026" | `openapi.json`'s one 191,026-byte line slipped through `max_chunk_bytes` — same *kind* of gap as #1, on the same knob, just a different code path within it | Split the oversized line instead of emitting it whole |

Incidents #1, #2, and #4 are all the same shape: **a number was picked once, for one provider, and
never revisited** — and the provider changed underneath it (confirmed: Fireworks → DeepInfra,
mid-session in this project's history). Each time, the symptom looked new (a different error
message, a different HTTP status, a different unit — tokens vs. characters vs. response bytes) so
each got its own investigation and its own patch, instead of being recognised as the same
architectural gap.

---

## 2. What's actually in `main` right now — every limit, scattered

```
services/agent-runner/src/indexer/chunker.rs:10    MAX_FILE_BYTES = 5 MiB           (hardcoded, not env-tunable)
services/agent-runner/src/indexer/mod.rs:40         max_chunk_bytes = 16,000          (INDEX_MAX_CHUNK_BYTES)
services/agent-runner/src/indexer/mod.rs:31         max_chunk_lines = 150             (INDEX_MAX_CHUNK_LINES)
services/agent-runner/src/indexer/mod.rs:?          embed_batch_size = 5              (INDEX_EMBED_BATCH_SIZE)
services/agent-runner/src/bootstrap/config/defaults.rs:12  DEFAULT_REQUEST_TIMEOUT_SECS = 180  (shared default)
services/agent-runner/src/bootstrap/config/embeddings.rs   request_timeout_secs = 180  (EMBEDDINGS_REQUEST_TIMEOUT_SECS)
services/agent-runner/src/bootstrap/config/runner.rs        request_timeout_secs = 180  (CONTROL_PLANE_REQUEST_TIMEOUT_SECS)
services/control-plane/src/integrations/k8s.rs:95   DEFAULT_JOB_DEADLINE_SECONDS = 3600 (AGENT_JOB_DEADLINE_SECONDS)
```

Every one of these is a **local, static guess**, set once by whoever was fixing the incident in
front of them, with no connection to the others and no connection to what the *current* embeddings
provider actually enforces. `max_chunk_bytes` (16,000 bytes) was sized as "roughly 4-5k tokens" —
a rough conversion for whichever model was in use at the time. It has no mechanism to know that
DeepInfra's real limit is `131,072` **characters** (not tokens, not bytes — yet another unit) for
the model currently configured, or that a future provider swap might drop that further.

This is the concrete evidence of the user's read on this: it's several independent low-level
byte/line/count checks, chosen once, defended locally, with nothing above them that understands
"what can the embeddings provider I'm talking to *right now* actually accept."

---

## 3. Why a bigger/smarter static number doesn't fix the actual problem

The instinct after each incident has been "raise the safety margin" — and that's necessary, but
it's treating the symptom. The real gap is structural: **the code that decides whether input is
too large (the chunker, at chunk-creation time) is three layers and one HTTP round-trip away from
the code that actually knows the true limit (the provider's response, at embed-call time).**

`services/agent-clients/src/embeddings.rs`'s own doc comment states this design explicitly:

```rust
/// Reactive retry bounds for embeddings, mirroring the chat client's defaults (ADR-0039). Retries fire
/// **only** on transient failures (connect/timeout, HTTP 429, HTTP 5xx); a 4xx other than 429 is
/// deterministic and never retried. The indexer already batches conservatively to stay under the
/// gateway's token-per-minute budget — this is the safety net for the bursts that still slip through.
const MAX_RETRIES: u32 = 3;
```

And in `embed_once`:

```rust
let transient =
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
```

A `422` (the exact status DeepInfra returns for "input too large") is **not** `429` and **not**
`5xx`, so `transient = false` — it is treated as a permanent, non-retryable failure and propagates
straight up through `index_checkout`'s `?` to fail the whole task. The system's own design
philosophy is "the pre-check should have already prevented this; if it didn't, give up." That
philosophy is exactly what keeps breaking: the pre-check is a guess, and guesses go stale the
moment the provider changes.

---

## 4. The clean fix: make the failure self-healing at the point where the real answer is known

Instead of only trying to get the static guess more accurate (which just delays the next
incident), add a second, adaptive layer: **when the provider itself says "too large," react to
that concretely instead of failing the task** — split the batch, or split the offending oversized
item, and retry. This is the one place in the system that actually knows the true limit, because
the provider just told it.

### 4.1 Classify "too large" as a distinct, actionable outcome — not a generic 4xx

```rust
/// A provider's own signal that this specific input was too large — as opposed to a transient
/// failure (retry the same request) or a genuinely fatal one (bad auth, unknown model).
struct TooLarge;

fn classify_422(body: &str) -> Option<TooLarge> {
    // DeepInfra: {"error":{"message":"Value error, The input sequence should have less than
    // 131072 characters...","code":null}}. Match on the shape, not a hardcoded number — the
    // number is exactly the thing that changes per provider/model.
    if body.contains("should have less than") && body.contains("characters") {
        Some(TooLarge)
    } else {
        None
    }
}
```

### 4.2 On that signal, split and retry instead of failing outright

```rust
pub async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    self.embed_with_splitting(texts, MAX_SPLIT_DEPTH).await
}

async fn embed_with_splitting(&self, texts: &[&str], depth: u32) -> anyhow::Result<Vec<Vec<f32>>> {
    match self.embed_retrying_transient(texts).await {
        Ok(vectors) => Ok(vectors),
        Err(err) if err.too_large && texts.len() > 1 && depth > 0 => {
            // The provider rejected this batch as too large — halve it and retry each half.
            // This adapts to whatever the current provider's real limit is, with no number to
            // keep in sync by hand.
            tracing::warn!(batch_len = texts.len(), "embeddings batch too large; splitting and retrying");
            let mid = texts.len() / 2;
            let (left, right) = texts.split_at(mid);
            let mut out = Box::pin(self.embed_with_splitting(left, depth - 1)).await?;
            out.extend(Box::pin(self.embed_with_splitting(right, depth - 1)).await?);
            Ok(out)
        }
        Err(err) if err.too_large && texts.len() == 1 => {
            // Down to a single oversized item and it's still rejected — this is a genuinely
            // pathological input (a giant single line, same shape as the CoopData incident).
            // Nothing left to split at this layer; surface it clearly rather than looping.
            Err(err.error.context("single input still exceeds provider's limit after splitting"))
        }
        Err(err) => Err(err.error),
    }
}
```

This composes cleanly with the *existing* transient-retry loop (429/5xx/timeout) rather than
replacing it — that mechanism is already correct and shouldn't change; this adds the one case it
deliberately excludes today.

### 4.3 What this buys, concretely

- **Incident #4 (this PR) would have self-healed without a code change.** The 191,026-byte chunk
  would have been rejected, the batch split, eventually isolated to that one chunk, and — with
  this layer alone — surfaced as a clear, isolated failure for *just that chunk* instead of
  failing the entire task. (The chunker fix in #636 is still the *right* fix for that specific
  case — a giant single line has no business being embedded whole — but this layer is what
  prevents the *next* unknown oversized-input shape from taking down a whole task.)
- **Incident #2 (response-payload-too-large after the DeepInfra switch)** is the same pattern in
  the opposite direction (response size, not request size) — the same split-and-retry shape
  applies: a batch whose *response* comes back rejected as too large gets halved and retried,
  instead of needing a human to notice, investigate Loki access logs, and manually tune
  `INDEX_EMBED_BATCH_SIZE` down.
- **A future provider swap** — smaller limit, different unit, different error shape — degrades
  gracefully (slower, more requests, but *finishes*) instead of failing outright the way it has
  three times now.

### 4.4 What doesn't change

- The static caps (`max_chunk_bytes`, `embed_batch_size`) stay as the **first line of defense** —
  they keep the common case efficient (one request per reasonably-sized batch) and keep this
  adaptive path a rare fallback, not the normal case. This isn't "remove the low-level checks," -
  it's "stop treating them as the *only* check."
- The transient-retry policy (429/5xx/timeout, exponential backoff) is unchanged and correct as-is.

---

## 5. A second, smaller gap worth closing: make a stale guess *visible* before it fails

Right now, nothing tells an operator "the configured `max_chunk_bytes` (16,000) is a lot bigger
than what the current provider actually accepts (131,072 chars ≈ conservative for most content,
but getting closer for some)" — the only signal is a production failure. Two cheap additions:

- **Log when the adaptive-split path in §4 actually fires.** A `tracing::warn!` there, on a
  metric/dashboard, tells operators "the static cap is being outgrown" *before* it becomes a hard
  failure — the same visibility principle already used elsewhere in this codebase (e.g. the
  gateway rate-limit budget warning in `embed_once`, `rate_limit.is_low(0.1)`).
- **Note the provider's real limit next to the static default**, sourced from whatever the last
  observed rejection said, not re-derived by hand each time. Doesn't need to be automatic
  discovery (that's a bigger change) — just keeping the comment next to `max_chunk_bytes`
  accurate the next time someone touches it after a provider swap is most of the value.

---

## 6. Summary

The recurring failure isn't "the number was wrong" — it's that **there is no layer between "a
static guess" and "the whole task fails"**. Every fix so far has adjusted the guess. The clean fix
adds the missing middle layer: when the provider's own response says an input was too large, split
and retry instead of failing outright, using the provider's real, current answer instead of a
number picked once for a provider that may no longer be the one running.

This is not proposed as part of PR #636 (which fixes the specific chunker gap, already
merged/mergeable on its own) — it's a follow-up worth its own ticket and PR, scoped to
`services/agent-clients/src/embeddings.rs`.
