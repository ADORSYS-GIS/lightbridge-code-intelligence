### Type

Feature

### Summary

We need to **stream A2A review findings incrementally, as each confirmed finding lands, instead of one end-of-run artifact blob** — because today the A2A streaming surface only delivers findings as a single terminal artifact, and on the common posting path it closes *empty* (the `reviews` row is written async by the reconciler *after* `succeeded`, so the caller must fall back to a follow-up `GetTask`).

Expected result:
> An A2A subscriber receives one `artifactUpdate` per confirmed finding at finalize (while still `WORKING`), then a conclusion `review` artifact, then the terminal `COMPLETED` closes the stream — so a downstream consumer can act on finding #1 without waiting for the whole run, and the stream carries the findings on every path.

### Intent

Decouple *how findings reach an A2A consumer* (low-latency, per-finding, reversible stream) from *how they reach the PR* (one deduped grouped review — unchanged). The A2A caller is the consumer that actually feels the "wait for the whole run, then often nothing" pain; the forge post is fine batched. This is an **emit-only** change: confirmation timing (the end-of-run refute pass) and the PR posting path are deliberately untouched.

### Source of truth (links)

- **Design decision captured in [ADR-0098](https://github.com/vymalo/lightbridge-code-intelligence/blob/claude/streaming-findings-publication-9a6f27/docs/adr/0098-a2a-per-finding-review-streaming.md)** (this ticket's PR).
- Refines **[ADR-0077 — A2A streaming event log](https://github.com/vymalo/lightbridge-code-intelligence/blob/main/docs/adr/0077-a2a-streaming-event-log.md)** and **[RFC-0006 — A2A agent surface](https://github.com/vymalo/lightbridge-code-intelligence/blob/main/docs/rfc/0006-a2a-agent-surface.md)**.
- Composes with **[ADR-0097 — review runs on OpenCode](https://github.com/vymalo/lightbridge-code-intelligence/blob/main/docs/adr/0097-review-runs-on-opencode.md)** (host-agnostic: the hook is at the control-plane finalize boundary).

### Current Behavior

`append_transition_events` (`services/control-plane/src/a2a/events.rs`) emits **one** artifact-update on the `succeeded → COMPLETED` transition, loaded from the `reviews` row (`completed_artifact`). On a posted review the `reviews` row lands asynchronously via the reconciler *after* `succeeded`, so at the terminal transition the artifact usually isn't persisted yet — the stream closes with no findings and the caller must poll `GetTask`.

### Expected Behavior

At `finalize_review` (where the deduped, refute-survived findings and the verdict both exist synchronously), stream one `artifactUpdate` per confirmed finding (`artifactId` `finding-{file}:{line}`, ADR-0032 JSON), then a conclusion `review` artifact (summary + context, no findings blob), all before the terminal `COMPLETED` status-update — which stays the sole stream freeze/close. `GetTask` polling is unchanged (rebuilds the combined artifact from the `reviews` row).

### Acceptance Criteria

- [ ] Given an A2A-fronted review with N confirmed findings, when it finalizes, then the stream carries N `finding-*` `artifactUpdate` frames + one `review` conclusion frame, in order, before the terminal `COMPLETED`.
- [ ] Given a re-finalize (crash-after-finalize requeue), when `append_review_stream` runs again, then it appends nothing (idempotent via `has_artifact_update`).
- [ ] Given a non-A2A (webhook) run, when it finalizes, then no stream events are appended.
- [ ] Given a stream already frozen on a terminal event, when a late finalize runs, then it no-ops (`has_final`).
- [ ] The streamed per-finding `data` part is byte-identical to one element of the `findings` array `GetTask` returns (streaming ⟷ polling parity).
- [ ] Error cases are handled safely — a stream-append failure is best-effort and never fails the finalize.
- [ ] Existing behavior is not broken: PR posting (one grouped review) and the end-of-run refute pass are unchanged.
- [ ] Relevant tests are added; verification evidence is provided.

### Out of Scope

- Posting findings to the PR incrementally (still one grouped review through the outbox).
- Per-finding *confirmation* (running the refute pass incrementally so a finding streams the instant it is proven) — confirmation stays end-of-run; this changes only the emit side.
- Any change to the review agent loop or the OpenCode host.

### Technical Context

- `services/control-plane/src/a2a/events.rs` — new `append_review_stream`; removed the terminal `completed_artifact` emit.
- `services/control-plane/src/a2a/mapping.rs` — `finding_artifact` / `conclusion_artifact` pure builders.
- `services/control-plane/src/http/internal.rs` — one call in `finalize_review` after the summary is computed.
- `docs/a2a-review-skill.md` §5 — caller-facing streaming contract updated.
- `docs/adr/0098-a2a-per-finding-review-streaming.md` — the decision record.
- Preserves the ADR-0077 invariants: gap-free `seq`, `has_final` freeze, `FOR UPDATE` row-lock serialization.

### Risks

- **Stream/poll divergence** — mitigated: streamed set == persisted `reviews.findings` == `GetTask`, asserted by a parity test.
- **Duplicate/late events on re-finalize or after terminal** — mitigated: `has_artifact_update` + `has_final` guards, both tested.
- **Regressing the forge path** — mitigated: emit-only change; posting and refute untouched; full suite green.

### Test Plan

- Unit: `finding_artifact` / `conclusion_artifact` builders (shape, stable id, empty-summary placeholder).
- Integration (`#[sqlx::test]`): per-finding-then-conclusion ordering before terminal; idempotent re-finalize; non-A2A no-op; freeze-after-terminal; updated terminal-replay test (no artifact on the transition).
- Full `cargo test -p control-plane`, `cargo check --workspace`, `cargo fmt --all --check`, `cargo clippy -p control-plane --all-targets`.

### Verification evidence

Test output:
```text
$ cargo test -p control-plane
test result: ok. 307 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out

$ cargo test -p control-plane 'a2a::'
test result: ok. 106 passed; 0 failed; 0 ignored; 0 measured; 203 filtered out

$ cargo check --workspace   → Finished (clean)
$ cargo fmt --all -- --check → clean
$ cargo clippy -p control-plane --all-targets → 0 warnings/errors
```
Rebased onto current main (`b8d3223`); the A2A files applied cleanly, `internal.rs` auto-merged with the Restate-egress removal (no conflict). ADR renumbered 0094→0098 (main claimed 0094–0097).

Remaining limitations: the streamed conclusion's `reviewUrl` is `null` (the PR review posts after finalize); a caller needing the permalink does one follow-up `GetTask`.

### Human accountable owner

@leghadjeu-christian

### AI Usage Declaration

Understanding code, Proposing implementation, Generating code, Generating tests, Reviewing the diff, Writing documentation, Drafting the ticket

### Human verification completed

- [ ] I understood the intent
- [ ] I checked the source of truth
- [ ] I reviewed all AI-generated text/code
- [ ] I verified the implementation manually
- [ ] I verified the tests
- [ ] I checked for hallucinated assumptions
- [ ] I documented remaining risks
- [ ] I am the accountable owner and accept responsibility for this ticket.

_(Automated verification — build/tests/lints above — was run in-session by the AI agent. The human-accountability boxes are left for the assigned owner @leghadjeu-christian to complete after review.)_
