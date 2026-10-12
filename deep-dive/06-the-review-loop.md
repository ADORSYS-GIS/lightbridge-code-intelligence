# 06 — The review loop

This is the story of one review, start to finish: a Kubernetes Job wakes up, clones a repo, decides
whether it needs to re-index anything, runs an LLM-driven agent loop against the PR diff, and posts a
single grouped GitHub review — never touching GitHub directly itself. Every claim below is checked
against the actual source as of this writing; file:line citations point at the real functions, not a
paraphrase of them.

## 1. The entrypoint: `agent-runner`

Each task (an index job or a review job) runs as its own Kubernetes Job (ADR-0004 — see
`08-kubernetes-deployment.md`). The Job's binary is `agent-runner`, and its own module doc is a good
map of the pipeline:

```rust
//! The lifecycle: clone → semantic index (tree-sitter → pgvector, slice 2) → structural index
//! (in-house lci-codegraph → Neo4j, slice 3, ADR-0086) → review (the native agent loop, ADR-0026/0037, which acts via mediated
//! write tools the control plane flushes) → report. Indexing is required; the structural graph and the
//! review are best-effort and non-fatal.
```
(`services/agent-runner/src/main.rs:9-12`)

`main.rs` itself is three lines of logic — it pins the global allocator (mimalloc, chosen because the
indexing walk is allocation-heavy and musl's malloc regresses under multithreading) and calls into the
library:

```rust
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // `None` mode → infer index-vs-review from the task's `command`, today's behaviour unchanged.
    agent_runner::run_once(None).await
}
```
(`services/agent-runner/src/main.rs:24-28`)

The real orchestration lives in `run_once` / `run_once_body` / `run()` in
`services/agent-runner/src/run.rs`. `run_once` installs the crypto provider, starts the tracing/OTel
layer (see `09-observability.md`), opens a root span re-parented from the Job's `TRACEPARENT` env var,
and — critically — flushes the OTel guard **unconditionally** after the body returns, on every exit
path (`services/agent-runner/src/run.rs:43-61`). The doc comment is explicit about why this split
exists: the old code had `return`s inside the SIGTERM/cancellation arms of a `tokio::select!` that
would have skipped the flush on exactly the two paths a forcibly-terminated Job most needs it covered
(`services/agent-runner/src/run.rs:63-68`).

Inside `run_once_body`, the runner races the actual work against two stop signals:

```rust
let outcome = tokio::select! {
    result = run(mode, &config, &client, &embeddings_config, &review_configs, sast_config.as_ref()) => RunOnceOutcome::Ran(result),
    _ = terminated() => { /* SIGTERM */ }
    _ = cancelled_upstream(&client, config.task_id) => { /* the control plane says we're no longer active */ }
};
```
(`services/agent-runner/src/run.rs:141-151`)

`terminated()` is the Kubernetes pod-termination signal (SIGTERM, sent when the dispatcher's reaper
deletes the Job); `cancelled_upstream` polls the runner's own task status every 10 seconds so a task
whose PR closed or whose repo was removed can stop even if the reaper itself is down
(`services/agent-runner/src/run.rs:204-221`). Both arms deliberately report **nothing** back to the
control plane — the control plane already owns a `cancelled` row, and reporting would clobber it
(`services/agent-runner/src/run.rs:152-157`).

## 2. `run()`: clone → index-or-skip → review

`run()` is the actual per-task body. Four steps, in order:

1. **Report `running`** (`services/agent-runner/src/run.rs:253`) — a re-affirmation; the dispatcher
   already set this on claim.
2. **Fetch the task context** from the control plane (`client.get_context(config.task_id)`,
   `services/agent-runner/src/run.rs:257`) — repo coordinates, target type, head/base SHA, the
   resolved preset name, and (load-bearing for step 3) `context.repo_indexed`.
3. **Clone** the repo at the relevant commit using the short-lived installation token
   (`clone::checkout`, `services/agent-runner/src/run.rs:302`).
4. **Index** (`perform_indexing`) and then **review** (`perform_review`), each independently
   best-effort except that a review-finalize failure *is* fatal (more on that below).

The runner never holds a GitHub App key itself — only a per-task bearer and whatever installation
token the control plane handed it in the task context. The control plane "owns the trust boundary …
and validates findings + writes to GitHub" (`services/agent-runner/src/main.rs:6-7`); the runner's job
is strictly read + propose.

## 3. ADR-0025: a review does not always re-index

Before this decision, **every** task — review or index — ran the full pipeline: clone → embed the
whole repo into pgvector → rebuild the whole Neo4j graph → (if review) review. A PR review therefore
cost roughly the same as a full repo index, every time, re-embedding files the PR never touched.

The fix is `perform_indexing`'s `needs_index` check:

```rust
async fn perform_indexing(
    is_index: bool,
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    status: Option<&StatusHandle>,
) -> anyhow::Result<(usize, String)> {
    let needs_index = is_index || !context.repo_indexed;
    if !needs_index {
        tracing::info!(
            "repo already indexed — reusing the base index (skipping re-index for review)"
        );
        return Ok((0, "reused base index".to_string()));
    }
    ...
```
(`services/agent-runner/src/run.rs:380-394`)

`context.repo_indexed` is populated by the control plane from `db::repo_has_index` — "does the repo
have any `code_chunks`?" (ADR-0025's own reference list, `docs/adr/0025-review-reuses-base-index.md:51`).
The rule, from the ADR:

> A review reuses the existing base index; it does not re-index. … A review on an already-indexed
> repo skips both the semantic and structural index and goes straight to the review, searching the
> base index via the MCP tools. `repo_has_index` failing (DB hiccup) is treated as "not indexed" → the
> runner indexes — i.e. it fails **safe**, back to the old behavior.
(`docs/adr/0025-review-reuses-base-index.md:27-36`)

Indexing only happens for (a) an actual `index` task, or (b) a cold repo (`!context.repo_indexed`) —
so the very first review on a brand-new repo still self-indexes (it has to; there's nothing to reuse
yet), but every warm review after that skips straight to the review step. The accepted trade-off
(documented in the ADR) is that the reused base index reflects the **default branch**, not the PR
head — the agent still sees the PR's changed code because it's in the diff/prompt, but *search over*
brand-new PR-only symbols is limited until the next base re-index.

When indexing does run, it's one `lci-codegraph` walk producing both halves of the index — the
semantic chunks (pgvector) and the structural graph (Neo4j) — from a single parse (ADR-0086), and the
graph step is explicitly non-fatal:

```rust
// Best-effort: the graph store may be unconfigured (control plane returns 503). A graph failure
// is logged, not fatal — the task still succeeds with the semantic index alone, ...
```
(`services/agent-runner/src/run.rs:405-406`)

## 4. The review itself: OpenCode is the live path, the native loop is retiring

This is the one place where the code has moved past its own doc comments. `agent-runner`'s module doc
still describes "the native review agent loop" as the mechanism
(`services/agent-runner/src/lib.rs:13-16`), and `services/agent-runner/src/review/mod.rs` opens with:

```rust
//! Code review — the native in-process agent (ADR-0026 + ADR-0037), now a thin **host** over the
//! extracted `lci-review-agent` assembly (R1e).
```
(`services/agent-runner/src/review/mod.rs:1-2`)

But the function `perform_review` actually calls in `run.rs` is `review::opencode::run_opencode_agent`,
not `review::run_native_agent`:

```rust
let outcome = review::opencode::run_opencode_agent(
    review,
    &context.preset,
    &context.command,
    diff.as_ref(),
    repo_instructions.as_deref(),
    context.prior_reviews.as_deref(),
    context.repo_memory.as_deref(),
    repo_config_context.as_deref(),
    sast_config,
    attribution,
    &mcp_env,
    task_id,
    client,
)
.await;
```
(`services/agent-runner/src/run.rs:530-545`)

`review/opencode.rs` is explicit about this being the hard cutover:

```rust
//! The OpenCode review host (RFC-0009 / ADR-0094 review cutover, slice 3): the thin transport shell
//! over the proven `lci_review_agent::opencode` core.
...
//! All the review *logic* — reconstructing a turn from the recorder, the reused coverage/refute gates,
//! the drive loop, the transcript — lives in `lci-review-agent` and is unit-tested there.
```
(`services/agent-runner/src/review/opencode.rs:1-9`)

and `run_native_agent` (in `review/mod.rs`) is annotated as the retiring path: "The former OpenCode
subprocess was removed in #140 — this is the only review path" referred to the *opposite* cutover
earlier in the project's life, but the current state (confirmed by what `run.rs` actually calls) is
that **OpenCode is now the live review host**, and `run_native_agent` exists only "Retired/legacy path
(`run_native_agent`, removed in a follow-up)" per its own in-body comments
(`services/agent-runner/src/review/opencode.rs:185-187, 235-238`). Both hosts drive the *same*
underlying policy composition (`lci_review_agent::flows::run_review` for the native path;
`lci_review_agent::opencode::{ReviewGates, ReviewDriver, run_review_loop}` reusing the identical
`CoverageGate`/`RefuteGate` code for the OpenCode path) — so the turn-loop mechanics described below
apply either way; only *who drives the model* differs.

### 4a. The policy composition (`lci-review-agent::flows::run_review`)

This is the native-loop assembly, and it's the clearest place to see every policy in its fixed order:

```rust
// Policy order is a behavioural contract (registration order = evaluation order in the engine):
// context trim → wind-down → read budgets → turn budget → scratchpad guard → coverage gate →
// refute gate → SAST anchor gate → finding-finish nudge.
let policies: Vec<Box<dyn TurnPolicy>> = vec![
    Box::new(ContextWindowTrim::new(params.context_window)),
    Box::new(WindDown::new(max_turns, params.max_batches).with_filter(winddown_filter)),
    Box::new(ReadBudgets::new(params.max_files_read, params.max_searches)),
    Box::new(TurnBudget::new(max_turns)),
    Box::new(ScratchpadLoopGuard::new()),
    Box::new(coverage),
    Box::new(RefuteGate::new()),
    Box::new(SastAnchorGate::new(params.sast_leads)),
    Box::new(FindingFinishNudge::new()),
];
```
(`services/review-agent/src/flows.rs:120-139`)

What each one does (from `services/review-agent/src/policies/mod.rs:1-11`):

- **`CoverageGate`** — bounces a premature `finish` until the changed files were actually engaged
  (or discloses what wasn't).
- **`ScratchpadLoopGuard`** — breaks a same-line `add_review_comment` loop.
- **`RefuteGate`** — makes the model re-verify its own P0/P1 findings before finishing.
- **`SastAnchorGate`** — rejects a SAST triage verdict anchored to a line opengrep never flagged.
- **`FindingFinishNudge`** — steers the model toward `finish` once it has recorded something.

The module doc for `flows.rs` states the key ADR-0103 invariant: **every preset runs through the same
policy composition.** A "fast" preset is not structurally different from a "deep" one — it just has
smaller numeric budgets (`max_turns`, `max_files_read`, …):

```rust
//! ADR-0103: every preset (however an operator names it) runs through this SAME policy composition —
//! the same tools, the same gates, the same prompt text. A preset varies the review ONLY through the
//! numeric budgets in [`ReviewRunParams`] ... — the old FAST-tier structural narrowing —
//! `FastTierGuard`, a hardcoded 5-turn cap, and skipping the coverage/refute/SAST-anchor gates — was
//! retired for exactly this reason
```
(`services/review-agent/src/flows.rs:5-11`)

`ReviewRunParams` (`services/review-agent/src/flows.rs:38-64`) is the exact numeric envelope: `max_turns`,
`max_batch_size`, `max_batches`, `max_files_read`, `max_searches`, `max_coverage_bounces`,
`circuit_breaker_threshold`, `context_window`, `diff_present`, `diff_files`, and `sast_leads` — the
shared feed the `run_sast` tool pushes opengrep leads into, which `SastAnchorGate` later drains.

### 4b. The OpenCode path reuses the same gates, split by responsibility

When review runs on OpenCode, OpenCode owns its own internal loop — one `session/prompt` call runs
many internal model round-trips and tool calls and returns once. The supervisor can only observe and
re-drive, so the native policies split into two classes, per `services/review-agent/src/opencode/mod.rs:1-25`:

- **Loop mechanics** (context-trim, wind-down, read/turn budgets) — handed off entirely to OpenCode's
  own maintained loop; the supervisor cannot narrow OpenCode's tools mid-internal-loop.
- **Review-quality gates** (`CoverageGate` / `RefuteGate`) — kept as the exact same tuned Rust
  `TurnPolicy` code and run supervisor-side, reusing the coverage denominator / citation-crediting /
  absence-claim logic rather than reimplementing it in TypeScript.

The module boundaries: `config` renders the per-task OpenCode config; `recorder` reconstructs a
`TurnOutcome` from OpenCode's own JSONL recorder (seeing every tool call, including ones inside a
subagent the ACP client is never shown); `gates` drives the reused gates over each observed cycle;
`driver` is the pure re-prompt/finalize control loop; `drive` is the `ReviewSession`-abstracted loop
that makes the orchestration unit-testable with a scripted fake
(`services/review-agent/src/opencode/mod.rs:16-29`).

## 5. ADR-0037: the agent never writes to GitHub — it calls mediated tools that buffer

This is the architectural core of the whole review loop. The agent's "output" used to be one terminal
tool (`submit_findings`) that handed back a single structured payload the control plane validated as
a batch. ADR-0037 replaced that with **mediated action tools the agent calls *during* the run**, each
one an HTTP call straight to the control plane:

```rust
pub const ADD_REVIEW_COMMENT: &str = "add_review_comment";
pub const RETRACT_FINDING: &str = "retract_finding";
```
(`services/review-agent/src/tools/record.rs:11-12`)

```rust
pub const FINISH: &str = "finish";
pub const REPORT_PROGRESS: &str = "report_progress";
pub const ABORT: &str = "abort";
```
(`services/review-agent/src/tools/finish.rs:9-11`)

`add_review_comment`'s tool spec is worth reading in full because its description carries the actual
severity rubric the model is held to:

```rust
"priority":{"type":"string","enum":["P0","P1","P2"],"description":"P0 = blocking — security/data-loss: ...
P1 = blocking — a real defect: demonstrably wrong on a realistic input or precondition, ... decide by
IMPACT IF REAL, not by how confident you feel. P2 = non-blocking: cosmetic, needs an already-broken
precondition, or a claim you cannot verify from the diff alone. ..."
```
(`services/review-agent/src/tools/record.rs:64`, wrapped for readability)

Calling `add_review_comment` does **not** post anything. It calls the control plane's
`add_review_comment` client method (`services/review-agent/src/tools/record.rs:148-159`), which — on
the control-plane side — is a plain buffer upsert:

```rust
/// `POST /internal/tasks/{id}/review/inline` — buffer one inline finding (ADR-0037). Last write wins
/// per `(file, line)`; nothing is posted until [`finalize_review`].
pub async fn add_review_comment(...) -> Response {
    ...
    match crate::db::upsert_pending_inline(pool, id, &a.file, a.line, ...).await { ... }
}
```
(`services/control-plane/src/http/internal.rs:1324-1354`)

### 5a. The buffer: `pending_review_actions`

The accumulation table, created by migration 0012:

```sql
CREATE TABLE IF NOT EXISTS pending_review_actions (
    id         BIGSERIAL   PRIMARY KEY,
    task_id    UUID        NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    -- 'inline' (a diff-pinned finding), 'comment' (a plain thread reply), or 'summary' (the verdict).
    action     TEXT        NOT NULL CHECK (action IN ('inline', 'comment', 'summary')),
    file       TEXT, line INT, title TEXT, priority TEXT, category TEXT, suggestion TEXT,
    body       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT inline_has_location CHECK (action <> 'inline' OR (file IS NOT NULL AND line IS NOT NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS pending_review_inline_uniq
    ON pending_review_actions (task_id, file, line) WHERE action = 'inline';
```
(`services/control-plane/migrations/0012_pending_review_actions.sql:11-32`)

Three dedup/ordering rules, stated directly in the migration's own comment:

- **Inline findings** dedup on `(task_id, file, line)` — **last write wins**, not a content hash.
  Deliberately: an LLM re-run is non-deterministic and will reword the same finding, so a content
  hash would let a slightly-reworded near-duplicate slip through as "new". Keying on position keeps
  only the most-refined version at that line.
- **The summary** is single-valued per task (also last-write-wins).
- **Plain comments** (`add_comment`) are append-only, ordered by `id`, and get consolidated into one
  reply at flush time rather than fanning out into one notification per call.

`db::pending_actions` is the Rust side of this table: `upsert_pending_inline`, `add_pending_comment`,
`upsert_pending_summary`, `delete_pending_inline` (used by the refute pass retracting a finding before
it's ever posted), `clear_pending_action` (drops one action class, used as each part of a flush
succeeds), and `clear_pending_review` (wipes the whole buffer — called when a runner (re)starts a task,
so a retry begins empty rather than appending to a stale partial buffer)
(`services/control-plane/src/db/pending_actions.rs:1-213`).

### 5b. The flush: `finalize_review`

Nothing reaches GitHub until the agent calls `finish` (or `abort`), the runner tells the control plane
how the run ended, and `POST /internal/tasks/{id}/review/finalize` runs:

```rust
/// `POST /internal/tasks/{id}/review/finalize` — flush the accumulated buffer (ADR-0037). Posts the
/// inline findings + summary as **one grouped PR review** (re-validated against the diff here, the
/// authority), consolidates buffered replies into **one** thread comment, records the emergent run
/// kind, and clears the buffer. ...
pub async fn finalize_review(...) -> Response {
    ...
    let pending = crate::db::load_pending_review(pool, id).await?;
    ...
```
(`services/control-plane/src/http/internal.rs:1512-1547`)

Two policy points worth calling out:

1. **Re-validation is authoritative here**, not trusted from the runner. `crate::review::validate`
   (`services/control-plane/src/review.rs:630-700`) re-checks every buffered finding against the PR's
   actual diff lines (`diff_lines`, `nearest_added_line` for clamping a context-line finding to the
   nearest added line so GitLab's stricter API doesn't 400 it) and buckets each finding into
   **inline** (anchors to a changed/added line), **deferred** (changed file, unpinnable line — folds
   into the review body), or **out of scope** (file the PR doesn't touch — surfaced in a collapsible
   section, never silently dropped — this is the ADR-0033 "no silent drops" guarantee).
2. **ADR-0056**: when a PR review is going to post, buffered `add_comment` replies are **dropped**,
   not posted as a second message — the verdict belongs solely in the grouped review:

```rust
// 1) Buffered replies → ONE consolidated reply intent. **Policy (ADR-0056):** on a **pull request
// that is also posting a review**, the verdict belongs solely in the grouped review (step 2) — a
// separate issue-comment is the duplicate "2× messages" channel, ...
// So we DROP the buffered replies whenever a review is posted — gated on `post_pr_review`, NOT on
// "has inline findings": a CLEAN review (a verdict summary with zero findings) is still a review
```
(`services/control-plane/src/http/internal.rs:1607-1616`)

`posts_pr_review` is the pure policy function this gates on:

```rust
fn posts_pr_review(
    target_type: &str,
    has_inline: bool,
    has_summary: bool,
    buffer_empty: bool,
) -> bool {
    target_type == "pull_request" && (has_inline || has_summary || buffer_empty)
}
```
(`services/control-plane/src/http/internal.rs:1415-1422`)

Note `buffer_empty` is in the OR — an agent that calls **no** write tool at all still posts the
ADR-0037 empty-run backstop default review ("No issues found — the change looks good.",
`services/control-plane/src/http/internal.rs:1239`), so an `@mention`-triggered run can never look like
a silent hang.

### 5c. The run kind is emergent, not declared

There is no up-front "is this a review or a question" classifier any more. `set_task_kind`
(`services/control-plane/src/db/pending_actions.rs:175-182`) records, at flush time, whichever of
`review` / `ask` / `mixed` actually happened — derived from which tools fired, exactly as ADR-0037
specifies: "inline findings ⇒ review; only a reply ⇒ ask; both ⇒ both; neither ⇒ review with the
default clean summary" (`docs/adr/0037-agent-acts-via-mediated-tools.md:92-94`).

## 6. Why this shape: ADR-0037's own reasoning

The ADR's problem statement is worth quoting directly, because it explains *why* mediation matters,
not just *that* it exists:

> The native review agent ends a run by calling **one terminal tool** — `submit_findings` — that
> returns a single structured payload; the control plane then validates the whole batch against the
> PR diff and posts it as one review. … This shape has three problems. (1) It hard-codes **one output
> per run**… (2) The **keyword classifier is brittle**… (3) Findings are validated **in a batch at the
> end**, so an out-of-scope finding is discovered only after the fact
(`docs/adr/0037-agent-acts-via-mediated-tools.md:9-23`)

And the trust-boundary driver that constrains every option considered:

> **Trust boundary is non-negotiable** (ADR-0002): the untrusted per-task Job must never hold GitHub
> credentials; the control plane owns all writes.
(`docs/adr/0037-agent-acts-via-mediated-tools.md:35-36`)

This is why `add_review_comment`, `add_comment`, and `finish` are HTTP calls back to the control plane
over the exact same callback path the read-only MCP tools already use (`services/agent-runner/src/review/opencode.rs:313-321`
wires `LCI_MCP_CP_URL` / `LCI_MCP_RUNNER_TOKEN` for precisely this) — the Job never gets closer to
GitHub than "please buffer this," and the control plane is the only thing that ever calls GitHub's
API, from the reconciler role described in `07-a2a-protocol.md` and `08-kubernetes-deployment.md`.

## Verification notes

- Confirmed the review path is OpenCode, not the native loop, by reading the actual call site in
  `perform_review` (`services/agent-runner/src/run.rs:530`) rather than trusting `review/mod.rs`'s
  module doc, which still describes the native loop as primary.
- Confirmed `pending_review_actions` dedup semantics against both the migration SQL
  (`0012_pending_review_actions.sql`) and the Rust upsert queries in `db/pending_actions.rs`, which
  match exactly (`ON CONFLICT (task_id, file, line) WHERE action = 'inline' DO UPDATE`).
- Confirmed the reply-drop policy (`posts_pr_review`) with its own unit tests in
  `services/control-plane/src/http/internal.rs`, which assert the gate is NOT keyed on finding count
  (a zero-finding clean review still suppresses buffered replies) — read directly rather than inferred
  from the surrounding comment.
