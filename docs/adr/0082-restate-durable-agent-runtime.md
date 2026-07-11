# ADR-0082: Restate Phase D — the durable agent runtime (the review loop resumes at the step it left)

- **Status:** Proposed (design-only — nothing in this ADR is implemented; gated, see §Gates)
- **Date:** 2026-07-11
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) adopts Restate as the durable-execution
substrate via a strangler migration. Its named phases are plumbing: Phase A made **egress delivery**
durable ([ADR-0074](0074-restate-egress-pilot.md), live in prod since 2026-07-11), Phase B makes the
**task lifecycle** a workflow ([ADR-0076](0076-restate-task-lifecycle-workflow.md), designed), Phase C
deletes the superseded scaffolding.

This ADR records the **stated end goal of adopting the engine in the first place**, which none of
those phases delivers: **the AI review agent itself becomes a durable program.** Today a deep-tier
review is a single in-memory loop —
[`run_native_agent`](../../services/agent-runner/src/review/native/agent.rs) (`for turn in
0..max_turns`, up to 150 turns over up to 2 h on a frontier-class model, ADR-0062/0069) inside a
one-shot Kubernetes Job ([ADR-0004](0004-one-k8s-job-per-task.md)). If the pod dies at turn 140 —
node eviction, OOM, deploy, spot reclaim — **everything is lost**: the Phase-B timeout branch requeues
the task and the new Job re-pays 140 turns of frontier-model tokens and up to 2 h of wall clock, and
re-executes every side-effectful tool call along the way. The desired behavior is what durable
execution exists for: **restart, replay the completed LLM turns and tool calls from the journal
without re-executing them, and continue live at the first step that never finished.**

One precision up front, because it shapes everything below: durable execution resumes at the last
completed **step boundary**, not "the exact line." The handler re-runs from the top; every completed
`ctx.run` returns its journaled result instead of re-executing; deterministic glue between steps
re-runs. "Resume at the tool it left" therefore requires that **every LLM call and every tool
dispatch is its own journaled step** — that granularity decision is the heart of this design.

## Decision Drivers

- **Crash-cost proportional to one step, not one run.** A deep review's crash cost today is
  O(turns-completed) in tokens + wall clock; it should be O(1 turn).
- **Stop hand-rolling durability** (RFC-0005's doctrine). The alternative — checkpointing the loop
  state to Postgres and rehydrating on requeue — is exactly the bespoke replay machinery the RFC
  exists to eliminate (Option B below).
- **Preserve the trust boundaries that still matter** ([ADR-0002](0002-rust-control-plane-trust-boundary.md)
  / [ADR-0017](0017-agent-runner-control-plane-bootstrap.md)): the agent surface holds no forge
  credentials; findings flow through the mediated internal API ([ADR-0037](0037-agent-acts-via-mediated-tools.md)).
- **Unlock mid-loop interaction.** A2A `input-required` ([ADR-0081](0081-a2a-input-required-and-list-tasks.md))
  ultimately wants the *agent* to park mid-review on a question to the caller and resume — only a
  durable loop can suspend for hours at zero cost.
- **Don't gold-plate the cheap tier.** A fast-tier pass is seconds long and diff-only; restarting it
  is cheaper than any machinery. Phase D is **deep-tier only**.

## Considered Options

- **Option A — the review loop becomes a Restate workflow, hosted by a new long-lived
  `agent-worker` role** (this ADR). Each LLM call and each tool batch is a journaled step; the
  per-task Job disappears for deep reviews.
- **Option B — keep the Job; checkpoint per-turn state to Postgres via the internal API; the
  Phase-B requeue branch rehydrates `messages` from the checkpoint and continues.** Delivers most of
  the resume value with no new runtime surface — but it *is* the hand-rolled journal: rehydration
  must reconstruct the conversation, the budget counters, the wind-down state, and the coverage
  tracker in exact lockstep with the loop's own evolution, and every loop change becomes a
  checkpoint-schema migration. It also yields no mid-loop awakeable (the A2A unlock) and no engine
  observability. This is the strongest competitor and the honest fallback if Option A's gates fail.
- **Option C — keep the per-task Job and register it as a per-task Restate endpoint.** Restate
  dispatches to *registered deployment revisions*, which are immutable and long-lived by design;
  per-task register/deregister churn fights the deployment model outright. Rejected on mechanism.

## Decision Outcome

Chosen option: **Option A — one `ReviewAgent` workflow per deep-tier task**, hosted by a dedicated
`agent-worker` role, **gated on Phase B shipping and soaking first** (§Gates). Fast-tier reviews and
index tasks keep the ADR-0004 Job unchanged.

The decision is deliberately **designed around a code revamp** (§The code revamp): the loop is
extracted into a runtime-agnostic `agent-core` crate behind trait seams (`StepRuntime`, `Tool`,
`TurnPolicy`, `ModelClient`, `Workspace`), so the durable runtime is a *swap*, not a rewrite — and
the extraction itself (R1) is a pure refactor that ships first, un-gated, on the existing Job path.

### Where it sits in the strangler sequence

**A (egress, live) → B (task lifecycle, designed) → D (this: the agent loop) — with C (delete
scaffolding) orthogonal after B.** Phase B is the natural invoker: its `run` handler's step 3–4
("launch Job, await completion awakeable") becomes, **for the deep tier only**, a durable
workflow-to-workflow call into `ReviewAgent`. Nothing else in ADR-0076 changes; the index gate,
timeout policy, finalize and egress hand-off are reused as designed.

### The `ReviewAgent` workflow

Workflow ID = the task's workflow ID (the ADR-0076 idempotency tuple + `run_epoch`) — one agent
instance per task run, exactly-one enforced by the engine. The handler is a faithful restructuring
of [`run_native_agent`](../../services/agent-runner/src/review/native/agent.rs) plus the Job
bootstrap in [`main.rs`](../../services/agent-runner/src/main.rs); each numbered item is a named
`ctx.run` journaled step unless noted:

1. **`bootstrap`** — resolve and journal the run's *inputs and config snapshot*: model + generation
   params + budgets (`review.*` as resolved today), the diff (fetched against the merge base, as today), prior reviews, repo memory, the SAST
   digest, and the pinned `head_sha` + index snapshot id ([ADR-0050](0050-retrieval-pins-to-latest-indexed-snapshot.md)).
   Journaling the config snapshot gives replay a property the Job never had: **a mid-run ConfigMap
   or model churn cannot change an in-flight run** — resume uses the journaled snapshot.
   Large inputs obey the offload rule below.
2. **`ensure_checkout`** (special — see §Local ephemeral state) — clone the repo at the journaled
   `head_sha` into a per-task scratch dir. **Not** a plain journaled step: its *product* (the working
   tree) is local ephemeral state a replay on a fresh pod does not recreate, so every FS-touching
   step guards on `ensure_checkout(head_sha)` (clone-if-missing) rather than trusting the journal.
3. **Per-turn loop** (`for turn in 0..max_turns`, the deterministic glue re-runs on replay):
   - **`llm_turn:{turn}`** (`ctx.run`) — the [`chat.rs`](../../services/agent-runner/src/review/native/chat.rs)
     completion call, streaming + per-chunk idle timeout intact *inside* the step; the journaled
     result is the completed assistant message (content, reasoning per [ADR-0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md),
     tool calls, token usage). Transport-level retries (429/5xx, [#203/#209](https://github.com/vymalo/lightbridge-code-intelligence/pull/209))
     stay inside the step; **process death** is what the engine's replay covers.
   - **`tools:{turn}`** (`ctx.run`) — the turn's read-only tool batch dispatched as **one journaled
     step** whose result is the vector of tool outputs. Today's intra-batch concurrency
     ([agent.rs:914](../../services/agent-runner/src/review/native/agent.rs), up to `max_batch_size`
     concurrent mediated calls) lives *inside* the step as ordinary `tokio` concurrency — **no
     concurrent `Context` fan-out**, so the loop takes zero new exposure to the R2 /
     [restatedev/sdk-rust#89](https://github.com/restatedev/sdk-rust/issues/89) class of risk and
     keeps its latency. Write/terminal tools (`add_review_comment`, `retract_finding`, `add_comment`,
     `finish`, `abort`) dispatch in order as their own steps (idempotency table below).
   - The conversation (`messages`), the budget counters (`batches`/`files_read`/`searches`), the
     wind-down and coverage state, and `trim_tool_history` are **derived deterministically from
     journaled step results** — they are the re-run glue, not journal entries. (`Instant::now()`
     per-turn latency logging moves inside steps or is dropped; wall-clock never steers control flow.)
4. **`finalize`** (`ctx.run`) — submit the transcript ([ADR-0034](0034-agent-run-transcript-and-observability.md))
   and call `finalize_review` with the outcome (`finished`/`exhausted`/`aborted`), which buffers the
   grouped review and hands egress intents to `PlatformEgress` (Phase A) exactly as today's
   [`main.rs`](../../services/agent-runner/src/main.rs) tail does. The transcript can additionally be
   flushed incrementally (it is already accumulated per turn), so even a *journal-lost* disaster
   degrades to today's behavior, never worse.

### The code revamp: one loop, trait seams, two runtimes

The step map above cannot be hand-carved into today's code. `run_native_agent` is a ~3,900-line
monolith ([`agent.rs`](../../services/agent-runner/src/review/native/agent.rs)) where the loop, the
budget/wind-down/coverage policies, and the prompt assembly are interleaved, and
[`tools.rs`](../../services/agent-runner/src/review/native/tools.rs) dispatches via a `match` over
string constants with classification spread across helper predicates (`is_read_only_tool`,
`is_retrieval_tool`) and inline branches. Sprinkling `ctx.run` into that directly would **fork the
loop** — a Job flavor and a Restate flavor drifting apart, with the fast tier and the Option-B
fallback each multiplying the fork. Phase D is therefore designed around a code revamp whose test
is: **the durable runtime is a swap, not a rewrite.**

> The fine-grained architecture — the crate DAG, concrete trait signatures with their dyn/static
> dispatch decisions, the `StepError` taxonomy, host wiring, the file-by-file migration map, the
> golden-transcript harness, and the R1 PR slicing — lives in the companion
> [Phase D `agent-core` architecture](../restate-phase-d-agent-core-architecture.md) (the same
> companion pattern as ADR-0076's implementation plan). This section records the decision-level
> shape; **the companion doc is normative for the code.**

A new **family of workspace library crates** — `agent-types` → `agent-step` / `agent-tools` /
`agent-clients` → `agent-loop` → `review-agent`, plus a dev-only `agent-testkit` ("agent-core" is
the family's working name, not one crate; the DAG and each crate's allowed/forbidden deps are in
the companion) — owns the agent programming model, **born on edition 2024, with the workspace bump
to edition 2024 / resolver v3 as R1 step 0**. Shared crates depend on serde/tokio/anyhow only —
**no `kube`, no `sqlx`, no `restate-sdk`** (runtime impls live with their host binaries, so the SDK
pin never leaks into shared crates; enforced by an `xtask` dependency-hygiene check, not
convention). The seams, trait-by-trait:

- **`StepRuntime` — the durability seam (the load-bearing one).** Every effect the loop performs
  goes through `step(name, f) -> T`, plus `sleep(name, d)` and an awakeable/promise hook for the
  future `input-required` slice. Three implementations, one per execution substrate:
  `Passthrough` (the Job path — awaits the future, ignores the name; lives in `agent-step`),
  `RestateRuntime` (wraps `ctx.run`/`ctx.sleep`/awakeables; lives in the `agent-worker` binary with
  the pinned SDK), and — only if the gates fail — `CheckpointRuntime` (Option B, persisting via the
  internal API). **The Option-B fallback collapses from "a second system" to "a third impl of the
  same trait."** Step *names* (`llm_turn:{n}`, `tools:{n}`, `tool:{n}:{call_id}`, …) are constants
  in `agent-types` with a stability test, turning the R14 journal-contract rule from a review
  convention into code.
- **`Tool` — spec + classification + replay contract, then a registry.** Each tool implements
  `spec()` (today's `ToolDef` JSON schema), `kind()` (`ReadOnly{Retrieval|File|Knowledge}` /
  `Write` / `Terminal` / `Progress` — replacing the string-predicate helpers), `replay()`
  (`ReadOnly` / `Idempotent` / `NeedsDedupKey` — the §idempotency table becomes machine-checkable:
  a runtime without dedup support can *refuse to register* a `NeedsDedupKey` tool instead of
  silently double-posting), and `call(cx, args)`. A `ToolRegistry` owns the set, and everything
  that is an inline branch today becomes a per-turn registry *view*: the tier allowlist (builtin +
  `mcp__` selectors, ADR-0062/0066), the diff-absent gate, wind-down narrowing, per-category budget
  drops, and the fast-tier refusal. MCP-discovered tools are just another `Tool` impl over the
  control-plane proxy.
- **`ToolCx` / `Workspace` — the checkout becomes a trait.** Today's `Tools` struct fields
  (`client`, `embedder`, `task_id`, `checkout_root`) become the tool context, with the filesystem
  behind `Workspace::root()`. The Job impl clones eagerly at bootstrap (today's behavior); the
  worker impl **is** the `ensure_checkout` lazy guard from §Local ephemeral state — the replay trap
  gets a type, not a convention.
- **`ModelClient` — the LLM seam.** `complete(req) -> AssistantTurn`; `chat.rs`'s native client is
  the sole impl (streaming, per-chunk idle timeout, transport retries stay inside — ADR-0075 stands,
  no Rig). The loop stops knowing about SSE or retry policy.
- **`TurnPolicy` — the loop's inline logic, named and composable.** `before_turn(&mut self, state)
  -> actions` (narrow tools / inject a nudge / force-finish / refuse a call) and `after_turn`
  observation. Today's interleaved blocks become one policy each: `TurnBudget`, `WindDown`,
  `ReadBudgets` (ADR-0042), `ContextWindowTrim` (ADR-0045), `CoverageGate` (ADR-0069),
  `FastTierGuard` (ADR-0062), `ScratchpadLoopGuard`. Each becomes independently unit-testable —
  today they are testable only by driving the whole loop.
- **`AgentLoop<R: StepRuntime>` — the engine.** Owns the conversation, drives the turn loop,
  journals through `R`, records through a `TranscriptSink` (ADR-0034/0060). Deliberately **not
  review-specific**: the review agent is `AgentLoop` + the review tool set + review policies + the
  review prompt builder, so a future agent surface (e.g. RFC-0006's `ask`) composes the same crate
  instead of copying the loop.
- **Flows are functions, not a trait.** `bootstrap`, `finalize`, and the Phase-B handler steps
  compose as plain async fns over `&R` — dynamic dispatch earns a trait only where a registry needs
  it (tools, policies, runtimes). Naming a `Flow` abstraction today would be speculative generality;
  this ADR explicitly declines it until a second real flow exists.

**Sequencing — the revamp ships first, un-gated.** The extraction (call it **R1**) is a pure,
behavior-identical refactor of the Job path: `agent-core` extracted, tools ported onto `Tool` +
registry, the loop onto `AgentLoop<Passthrough>`, `agent-runner` reduced to a thin host. It needs
no Restate, predates the gates, and is independently valuable — it retires the monolith, makes the
policies unit-testable, and gives the eval harness a seam. Behavior-identity is enforced by golden
transcripts (same fixtures in → same transcript out) plus the existing unit/sqlx tests moving over.
Phase D proper (**R2**) then binds `AgentLoop<RestateRuntime>` in the `agent-worker` binary — and
the gates G0–G4 test *the runtime implementation*, not the loop. One friction is named now: the
generic `step(name, f)` signature must be reconciled with the pinned SDK's `ctx.run` closure
lifetimes — possibly via a boxed/erased step form; **G1 additionally validates that the
`StepRuntime` seam compiles and replays against the pinned SDK** before R2 begins.

### The journal-size and offload rule

A 150-turn deep review produces ~300+ journal entries. Entries stay **bounded and small**:

- Journaled verbatim: assistant messages, per-call tool outputs *up to a per-entry cap* (tool
  outputs are already truncated for the conversation; the cap aligns with that truncation).
- Over-cap payloads (the 300 KB-class diff from step 1, oversized tool outputs) are **offloaded to
  Postgres via the existing internal API**, keyed `(task_id, step_name)`; the journal holds the key +
  a content hash so replay can verify it rehydrates the same bytes. This extends the
  `DeliverOutcome` smallness discipline ([ADR-0074](0074-restate-egress-pilot.md)) from "an enum"
  to "a bounded blob or a verified pointer."
- The entry-size ceiling, total-journal ceiling, and **replay time** for a synthetic 300-entry
  journal are measured by gate G1 before any production code.

### Local ephemeral state (the replay trap this design must not fall into)

A journaled step is **skipped** on replay — so any step whose *product* lives on the pod's
filesystem or in process memory (the checkout, the `ChatClient`, the in-memory rate-limiter bucket)
must either be rebuilt unconditionally (clients — cheap, deterministic from the journaled config
snapshot) or guarded lazily (`ensure_checkout`). The review is **read-only over the checkout at a
pinned SHA**, so a lazy re-clone yields byte-identical content and `read_file` steps stay
deterministic. This rule gets its own review checklist line; it is the Phase-D analogue of
ADR-0076's "no `Context` in `ctx.run`."

### Idempotency of side-effectful steps (the at-least-once seam)

`ctx.run` journals the *result*, not the effect: die after the effect but before the journal ack,
and replay re-executes it. Per tool:

| Step | Effect | Replay behavior | Verdict |
|---|---|---|---|
| `llm_turn` | Paid gateway call | Re-pays **one** turn's tokens (vs. all turns today) | Accepted window |
| retrieval / `read_file` / MCP | Read-only | Harmless | Safe |
| `add_review_comment` | Buffers finding via internal API | Buffer is last-write-wins per `(file, line)` — already idempotent | Safe as-is |
| `retract_finding` | Deletes from buffer | Idempotent delete | Safe as-is |
| `add_comment` (reply) | Appends a reply row | **Duplicates on replay** — needs a dedup key `(task_id, turn, call_id)` on the internal API | Small change, required |
| `finish` / `abort` | Sets summary/terminal state | Set-once/last-write | Safe as-is |
| `finalize` | Groups + enqueues egress | Outbox `dedup_key` already dedups (ADR-0059/0074) | Safe as-is |

### The `agent-worker` role, and what happens to the sandbox

Deep-tier compute moves from a per-task Job to a **new long-lived `agent-worker` Deployment**
(same one-binary-role-selected pattern; **not** the existing `restate-worker`, which holds forge
credentials for `PlatformEgress` — the agent surface must never share a pod with those). It holds
exactly what the Job's runner holds today — the LLM gateway key and the internal-API runner token —
and, like the Job, **no forge and no Restate-admin credentials**. Horizontal scale is native: the
engine distributes workflow instances across replicas, and per-workflow-ID exclusivity means one
loop instance per task regardless of replica count.

This deliberately relaxes ADR-0004's *per-task pod* isolation for deep reviews. Why that is
acceptable — and where it is not:

- **Reliability isolation is what durable execution replaces.** Today an OOM kills the run; under
  Phase D it costs a replay-and-resume measured in seconds. The isolation argument inverts: crashes
  become cheap, so per-task pods stop paying for reliability.
- **The loop never executes repository code.** Review is read-only: clone, diff, parse, embed-search,
  read files. The one code-execution-adjacent stage is the **opengrep SAST scan**
  ([ADR-0061](0061-sast-deterministic-finding-source.md)/[ADR-0073](0073-sast-as-agent-tool.md) — a parser over untrusted
  input, historically crash-prone). SAST therefore **stays in a sandboxed short-lived Job**: the
  Phase-B task workflow runs it as its own step before invoking `ReviewAgent`, and the digest
  arrives as a journaled bootstrap input. The long-lived worker never runs opengrep.
- **Untrusted *content* on a multi-task pod** (checkout scratch dirs): per-task scratch under a
  dedicated `emptyDir`, wiped in `finalize` and by a lazy-clean guard in `ensure_checkout`;
  read-only-root elsewhere; the existing non-root/no-caps posture carries over.
- **The runner token becomes standing** instead of per-Job. Flagged as the honest security delta
  (it aligns with the standing-token surface [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243)
  already tracks); mitigation is the same task-scoping the internal API already enforces per
  ADR-0017 — a token can only act on the task it authenticated for.

### Deploys during a 2 h run (R4 sharpened)

A 300-entry journal over 2 h makes journal-vs-code evolution the top operational risk: reordering
the step sequence in a deploy breaks every in-flight deep review. The mechanism that makes this
survivable is Restate's **immutable deployment revisions**: in-flight invocations stay pinned to
the revision they started on; a deploy registers the new revision and the old one drains — meaning
**two `agent-worker` revisions run side by side for up to 2 h on every deploy**. That is a rollout
rule (register-new → drain-old, never kill-old) and a capacity line-item, not optional hygiene.
The "never edit a journaled step sequence in a patch release" rule from ADR-0076 applies verbatim,
and the per-turn step names (`llm_turn:{turn}`) are part of the journal contract.

### What this unlocks (and explicitly defers)

- **Mid-loop `input-required`** (A2A, [ADR-0081](0081-a2a-input-required-and-list-tasks.md)): the
  loop can expose a durable promise mid-review ("which of these two behaviors is intended?"), park
  at zero cost, and resume on the caller's answer — the agent-level version of what ADR-0081 designs
  at the task level. Deferred to its own slice after Phase D lands.
- **Per-turn observability for free:** the engine's introspection shows the live step
  (`llm_turn:37`), replacing log-archaeology for "where is this 2 h run?". The A2A event log
  ([ADR-0077](0077-a2a-streaming-event-log.md)) can later mirror per-turn progress.
- **Deferred:** fast tier (restart is cheaper), index tasks (Graphify/embedding jobs are
  restartable batch work), token-stream-level durability (the journaled unit is the completed
  message; streaming stays a transport concern inside the step).

## Gates (all must pass before implementation)

The **R1 extraction is not gated** — it is a pure refactor of the Job path (§The code revamp) and
can begin immediately; its own merge bar is behavior-identity (golden transcripts + existing tests).
Phase D proper (R2, the Restate runtime) does not start until:

- **G0 — Phase B live and soaked** per ADR-0076's own gate: the task workflow owns new tasks in
  prod, the legacy backlog is drained, and its R2 (`ctx.select` crash/replay verification on the
  pinned SDK) passed — Phase D reuses that verdict.
- **G1 — journal-scale + seam spike:** a synthetic 150-turn/300-entry workflow on the prod server:
  per-entry and total journal sizes within server limits with the offload rule applied,
  **replay-to-resume time in single-digit seconds**, measured under `kill -9` at turns 10/75/149 —
  and the `StepRuntime` seam demonstrated to compile and replay against the pinned SDK's `ctx.run`
  closure lifetimes (boxed/erased step form allowed).
- **G2 — local-state resume spike:** kill mid-run on one pod, resume lands on another replica;
  `ensure_checkout` lazily restores the tree; a post-resume `read_file` returns byte-identical
  content; the completed prefix is not re-executed (assert zero duplicate gateway calls via the
  eaig ledger, [#281](https://github.com/vymalo/lightbridge-code-intelligence/issues/281)).
- **G3 — revision-drain rehearsal:** deploy a new `agent-worker` revision while a synthetic 2 h run
  is in flight; the old revision drains it to completion; the new revision takes new work.
- **G4 — the `add_comment` dedup key** exists on the internal API (the one hard prerequisite in the
  idempotency table).

Failing G1 or G2 falls back to **Option B** (transcript-checkpoint rehydration) as the recorded
alternative — the goal (resume, not restart) survives even if the engine-native shape does not.

## Consequences

- **Good:** a deep review's crash cost drops from O(run) to O(one step) in both tokens and wall
  clock; requeue-from-zero disappears for the deep tier.
- **Good:** config/model churn can no longer change an in-flight run (journaled snapshot) — a
  live-ops footgun (the review model/config churns routinely in `ai-helm-values`) closed as a side
  effect.
- **Good:** the engine's introspection makes a 2 h run legible step-by-step, and the mid-loop
  awakeable path to A2A `input-required` opens.
- **Bad:** ADR-0004's per-task pod isolation is relaxed for deep reviews; the compensating controls
  (no repo-code execution on the worker, SAST stays in a Job, scratch hygiene, role-level credential
  separation, standing-token scoping) are real but are controls, not a boundary. Security review of
  the role spec is a merge condition.
- **Bad:** every `agent-worker` deploy runs two revisions for up to 2 h, and journaled-step
  evolution becomes a hard compatibility contract on a fast-moving file (`agent.rs`). The loop's
  step map must stay boring even when the loop's *logic* iterates.
- **Bad:** the LLM step's at-least-once window means a crash can re-pay one turn's tokens; accepted
  and bounded, but nonzero.
- **Good (from the revamp, even if Phase D never lands):** the ~3,900-line loop monolith becomes a
  small engine + named, unit-testable policies + a typed tool registry; the replay-safety of every
  tool is declared metadata instead of tribal knowledge; the eval harness gains a seam; and any
  future agent surface (RFC-0006 `ask`) composes `agent-core` instead of copying the loop.
- **Bad (from the revamp):** R1 churns the hottest file in the repo (`agent.rs` iterates with every
  review-quality lesson) and trades inline readability for trait indirection. Mitigated by
  move-don't-change discipline under golden transcripts, and by the "flows are functions, not a
  trait" line against over-abstraction — but the migration window will be noisy for concurrent
  loop work.
- **Neutral:** the `agent-runner` crate splits: the loop + tools move to `agent-core` behind the
  workflow; the Job path remains for fast/index (and as the Option-B fallback), so the binary
  surface grows before Phase C-style cleanup can shrink it.

### Risk register (extends RFC-0005's; IDs continue)

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R12 | Journal growth: 300+ entries × KB-scale results exceeds server limits or makes replay slow | Medium | High (unresumable or slow-resume runs) | Offload rule + per-entry cap; G1 measures both ceilings and replay time before any code |
| R13 | Local-ephemeral-state bugs: a step trusts the journal for FS/process state a fresh pod lacks | Medium | High (wrong-content reads after resume) | `ensure_checkout` guard pattern + review-checklist rule; G2 asserts byte-identical post-resume reads |
| R14 | Step-map churn on `agent.rs` breaks in-flight 2 h journals on deploy | High | Medium | Immutable revisions + drain rule (G3); step names are a reviewed contract; "no step-sequence edits in patch releases" |
| R15 | Shared worker as blast radius: one task's pathology (a poisoned diff, a pathological repo) degrades co-resident runs | Medium | Medium | No repo-code execution on the worker; SAST stays in a sandboxed Job; per-task scratch quotas; crashes are cheap by construction (replay) |
| R16 | Standing runner token on a long-lived pod widens #243's surface | Medium | Medium | Task-scoped internal-API authz (ADR-0017) unchanged; fold into #243's token-hardening work; no forge/Restate-admin creds on the role |
| R17 | LLM-step replay double-pays a turn (crash in the ack window) | Low | Low ($ bounded to one turn) | Accepted; eaig billing ledger (#281) makes occurrences visible |
| R18 | R1 extraction silently changes loop behavior (a policy fires a turn earlier, a tool-narrowing branch inverts) — review quality regresses without a crash to notice | Medium | Medium | Move-don't-change discipline; golden-transcript equivalence on fixtures as the R1 merge bar; existing unit/sqlx tests move with the code; the dogfood repos catch drift in days |
| R19 | The `StepRuntime` generic seam fights the pinned SDK's `ctx.run` lifetimes and erodes into SDK types leaking through `agent-core` | Medium | Medium | G1 validates the seam against the pin before R2; a boxed/erased step signature is the sanctioned escape hatch; `restate-sdk` stays a host-binary dep, never an `agent-core` dep — enforced in review |

## Alternatives considered

- **Option B — transcript-checkpoint rehydration on the existing Job path.** Most of the resume
  value, none of the new runtime — but it re-implements replay by hand (conversation, counters,
  wind-down, coverage state must rehydrate in lockstep with loop evolution forever), yields no
  mid-loop awakeable and no engine observability, and is precisely the pattern RFC-0005 set out to
  stop building. **Retained as the explicit fallback if G1/G2 fail**, which keeps this ADR honest:
  the goal is resume-not-restart; the engine is the preferred means, not the point.
- **Option C — per-task Restate endpoints inside the Job.** Fights immutable deployment
  registration; rejected on mechanism (see Considered Options).
- **Journal the token stream, not the completed message.** Would shrink the LLM-step replay window
  to near zero, but explodes journal entry counts by orders of magnitude and couples the journal to
  SSE framing. Rejected; the one-turn window is the accepted cost (R17).
- **Run the loop in the existing `restate-worker`.** One fewer Deployment, but it would co-locate
  the untrusted-content agent surface with the forge App key that `PlatformEgress` holds — undoing
  the ADR-0002 credential separation that survived every phase so far. Rejected outright.
- **Delegate the agent core to an external agent CLI (OpenCode driven over ACP)** *(owner-raised,
  2026-07-11)*. ACP gives the client per-tool-call **visibility and gating** (tool-call events +
  `request_permission`), but not per-step **ownership**: the LLM calls and conversation state live
  inside the external process, there is no replay verb to fast-forward it through completed steps
  from *our* journal, and its own session persistence is pod-local disk with no exactly-once
  semantics. The granularity Phase D journaling requires ("resume at the tool it left") structurally
  does not exist across ACP — and driving one prompt per turn degenerates the CLI into a worse
  chat-completions proxy while re-inheriting loop ownership anyway. It would also re-run the
  ADR-0026 reversal (runner-*enforced* quality gates would become prompt-level asks in someone
  else's loop — the ADR-0069 honor-system lesson) and re-open the ADR-0075 provider-fidelity risk
  class. **Rejected for the core.** The legitimate need it pointed at — reading project agent
  conventions (`.skills/*`, `SKILL.md`, `.claude/skills/`, `.cursor/rules`) — is a native loader
  slice on the ADR-0036/0030/0031 lineage, not a runtime decision.

## More Information

- [Phase D `agent-core` architecture](../restate-phase-d-agent-core-architecture.md) — the
  **normative companion** for the code revamp: crate DAG, trait signatures + dispatch decisions,
  error taxonomy, host wiring, migration map, golden harness, R1 PR slicing.
- [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) — the strangler proposal this extends
  with **Phase D**; determinism rules and the base risk register live there.
- [ADR-0074](0074-restate-egress-pilot.md) (Phase A, live) / [ADR-0076](0076-restate-task-lifecycle-workflow.md)
  (Phase B, the invoker and gate G0) / the [Phase B implementation plan](../restate-phase-b-implementation-plan.md).
- The loop being made durable: [`agent.rs`](../../services/agent-runner/src/review/native/agent.rs)
  (`run_native_agent`, the turn loop, wind-down/budget/coverage glue),
  [`chat.rs`](../../services/agent-runner/src/review/native/chat.rs) (streaming chat + retries),
  [`tools.rs`](../../services/agent-runner/src/review/native/tools.rs) (mediated dispatch),
  [`main.rs`](../../services/agent-runner/src/main.rs) (bootstrap: clone → diff → SAST → agent →
  finalize).
- Trust/isolation model being partially relaxed and preserved:
  [ADR-0004](0004-one-k8s-job-per-task.md), [ADR-0002](0002-rust-control-plane-trust-boundary.md),
  [ADR-0017](0017-agent-runner-control-plane-bootstrap.md), [ADR-0037](0037-agent-acts-via-mediated-tools.md);
  standing-token hardening [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243).
- What the durable loop unlocks: [ADR-0081](0081-a2a-input-required-and-list-tasks.md) (A2A
  `input-required`), [ADR-0077](0077-a2a-streaming-event-log.md) (per-turn progress mirroring).
- Two-tier review and the deep tier this targets: [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md),
  [ADR-0069](0069-review-tier-minimum-model-capability.md); reasoning/transcript capture
  [ADR-0034](0034-agent-run-transcript-and-observability.md) / [ADR-0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md).
