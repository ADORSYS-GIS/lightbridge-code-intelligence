# ADR-0098: A2A per-finding review streaming (incremental artifacts at finalize)

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** @stephane-segning

## Context and Problem Statement

The A2A streaming surface ([ADR-0077](0077-a2a-streaming-event-log.md), RFC-0006 Phase 2) delivers a
review's result as **one** terminal artifact carrying the entire findings set. `append_transition_events`
(`services/control-plane/src/a2a/events.rs`) rode the `succeeded → COMPLETED` transition: it loaded the
whole review from the `reviews` row (`completed_artifact`) and appended a single `artifact-update`
immediately before the terminal status-update. An A2A caller therefore saw *nothing* about the review
until the run was completely done — and, on the common posting path, not even then: the `reviews` row is
written asynchronously by the reconciler (`upsert_review`) *after* `succeeded`, so at the terminal
transition the artifact usually wasn't persisted yet, the stream closed empty, and the caller had to fall
back to a follow-up `GetTask` poll to retrieve the findings (ADR-0077's "mix streaming and polling"
contract).

Two problems compound: the caller waits for the whole run before receiving any finding, and on the
majority path the stream carries no findings at all. For a downstream A2A consumer that acts on findings
(e.g. the ADR-0088 `open` agent), "one blob, at the very end, often absent" is the worst shape — it can't
begin on finding #1 while the reviewer is still deriving finding #5, and it can't rely on the stream
alone.

## Decision Drivers

- **Stream findings incrementally, as confirmed.** A consumer should receive each finding as its own
  event, not one end-of-run bundle.
- **Do not weaken review quality.** The end-of-run refute pass ([ADR-0091](0091-refute-pass-outward-disconfirmation-search.md),
  the `RefuteGate`) is the confirmation gate; findings must still survive it before they are streamed. No
  premature/unverified finding may ever be emitted. (Per-finding *confirmation* — running the refute pass
  incrementally so a finding streams the instant it is proven, rather than in one burst at finalize — is a
  larger loop change, deliberately out of scope here; this ADR changes only the emit side.)
- **Do not touch the forge.** The PR review still posts as one grouped review through the outbox (ADR-0037).
  Streaming is an **additional** output to the A2A caller, never a second posting channel.
- **Preserve the ADR-0077 stream invariants.** Gap-free monotonic `seq`, the `has_final` freeze, and
  per-`tasks`-row serialization must all still hold; streaming and polling must still agree on *content*.
- **Emit where the data actually exists synchronously.** The terminal transition is the wrong hook — the
  data isn't there yet. `finalize_review` is the right one: the deduped, refute-survived findings and the
  verdict summary are both in hand there, in one place, before the run goes terminal.

## Considered Options

- **Per-finding artifacts emitted at finalize (chosen).** Move artifact emission off the terminal
  transition and into `finalize_review` (`http/internal.rs`), where the confirmed findings and the
  conclusion both exist synchronously. Emit one `artifact-update` per finding, then a conclusion
  `artifact-update`, all **before** the run goes terminal — so they carry lower `seq` than the terminal
  `COMPLETED` status-update, which stays the stream's freeze/close. `GetTask` polling is untouched: it
  keeps rebuilding the full combined artifact from the `reviews` row, independently of the event log.
- **Keep the terminal blob, add per-finding events too.** Rejected: on the silent-clean path the
  `reviews` row is persisted before `succeeded`, so the terminal transition *would* re-emit the whole
  findings set — duplicating every finding the finalize path already streamed. One emit site, not two.
- **Also post each finding to the PR incrementally.** Rejected (for now). A posted PR comment is
  ~irreversible (edit/delete a live comment vs. a cheap pre-post buffer retraction), it loses the single
  deduped grouped review, and it re-introduces the false-positive-that-needs-retraction failure mode the
  batched design (ADR-0037/0043) exists to avoid. Streaming is reversible for a programmatic consumer; a
  PR comment is not. The forge stays batched.
- **Move confirmation per-finding (stream the instant each is proven).** Deferred. That means running the
  refute pass incrementally instead of once at `finish` — more turns, more cost, and a more myopic
  confirmation than the holistic end-of-run pass. This ADR keeps confirmation end-of-run; the win here is
  *granularity*, not lower latency.

## Decision Outcome

Chosen option: **per-finding artifacts, emitted at finalize.**

### The contract

- **`append_review_stream`** (`events.rs`) is the new producer. `finalize_review` calls it on the
  `post_pr_review` path, right after the deduped `findings` and the `effective_summary` are computed, for
  every A2A task fronting the run. For each front it appends: one `artifact-update` per finding
  (`finding_artifact`, `mapping.rs`), then the conclusion (`conclusion_artifact` — summary + review
  context, **no** findings blob, since the findings were just streamed individually).
- **A finding artifact** carries the finding's ADR-0032 JSON in a single `data` part — byte-identical to
  one element of the `findings` array `GetTask` returns — under a stable `artifact_id` of
  `finding-{file}:{line}` (distinct per finding; the finalize buffer is last-write-wins per `(file, line)`
  so it's unique within a run). The **conclusion** reuses `artifact_id` `"review"` (aligning with the
  polling artifact's id) and carries the verdict summary + context; its `reviewUrl` is null because the
  review has not posted yet at finalize (the caller reads the permalink from `GetTask` afterward).
- **The terminal status-update closes the stream, not a `final` artifact.** The conclusion is a non-final
  `artifact-update`; the existing `succeeded → COMPLETED` status-update remains the sole `final = true`
  event. A subscriber reads: `WORKING` → finding chunks → conclusion → terminal `COMPLETED`, then closes.
- **`append_transition_events` no longer emits an artifact.** The `completed_artifact` helper (and its
  read of the `reviews` row on the transition) is removed; the terminal transition carries only the
  status-update.

### Invariants preserved

- **Ordering & freeze.** `append_review_stream` locks the `tasks` row `FOR UPDATE` first (the same
  discipline as `append_terminal_status`), so its appends serialize against a concurrent `set_task_status`
  transition on the same run and can't interleave `seq`s. It respects `has_final` (a stream that already
  froze on a terminal event no-ops) and is idempotent across a re-finalize via `has_artifact_update` (any
  existing `artifact-update` row means the stream already ran — it re-appends nothing rather than
  duplicating every chunk).
- **Streaming ⟷ polling parity.** The union of the streamed per-finding chunks is the *same* deduped set
  persisted to `reviews.findings` and returned by `GetTask` — same content, different shape. A unit test
  asserts the per-finding `data` part equals `serde_json::to_value(finding)`.
- **Host-agnostic.** The hook is in `finalize_review` (the control-plane trust boundary), which flushes the
  findings buffer regardless of *where* the review agent ran. It is unchanged by the OpenCode cutover
  ([ADR-0097](0097-review-runs-on-opencode.md)): whether findings were produced by the native agent-loop
  or by an OpenCode host over the MCP review tools, they still arrive as buffered mediated-tool calls and
  finalize the same way — so this change composes with that one without touching either agent host.

### Consequences

- **Good:** an A2A consumer receives each confirmed finding as its own event, as soon as the run
  finalizes, and can act on finding #1 without waiting for the rest. On the common posting path the stream
  now carries the findings at all (previously it closed empty and forced a `GetTask` poll).
- **Good:** the forge behaviour is byte-for-byte unchanged — same single grouped PR review through the
  outbox, same refute gate, same dedup. Streaming is purely additional and best-effort: a stream-append
  failure logs a warning and never fails the finalize (the posted review is the primary product). A
  non-A2A (webhook) run has no fronting `a2a_tasks` row, so the new call no-ops after one indexed lookup.
- **Neutral / accepted:** confirmation is still end-of-run, so all chunks emit in one burst at finalize —
  the win is *granularity and reliability of delivery*, not lower latency. True incremental confirmation
  (stream each finding the instant its own refute pass proves it) is a follow-up on the runner loop, not
  this change.
- **Neutral:** the conclusion's `reviewUrl` is null on the stream (the review hasn't posted at finalize).
  A caller that needs the permalink reads it from `GetTask` after completion — the polling artifact still
  carries it.

## References

- [ADR-0077](0077-a2a-streaming-event-log.md) — the streaming event log this refines (the terminal-blob
  emission it replaces, and the `seq`/freeze/serialization invariants it preserves).
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — the mediated-tool contract whose buffered-then-
  flushed finalize the forge path keeps; streaming does not add a second posting channel.
- [ADR-0091](0091-refute-pass-outward-disconfirmation-search.md) — the refute pass that remains the
  end-of-run confirmation gate; only findings that survive it are streamed.
- [ADR-0032](0032-review-finding-priority-and-category.md) — the finding JSON shape each per-finding
  `data` part carries.
- [ADR-0088](0088-open-mode-autonomous-ticket-agent.md) — a downstream A2A consumer that benefits from
  acting on findings incrementally.
