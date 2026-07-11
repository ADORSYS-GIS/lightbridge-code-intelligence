# Restate Phase D — the `agent-core` architecture (companion to ADR-0082)

> **Status: design-only.** This is the fine-grained code architecture for
> [ADR-0082](adr/0082-restate-durable-agent-runtime.md)'s revamp — crate layout, trait signatures,
> dispatch decisions, error taxonomy, wiring, and the migration map. **R1 (the extraction) may be
> built once ADR-0082 is accepted; R2 (the Restate runtime) stays behind ADR-0082's gates G0–G4.**
> Companion pattern: same relationship as the
> [Phase B implementation plan](restate-phase-b-implementation-plan.md) to ADR-0076.

---

## 0. Toolchain target (R1 step 0)

The agent crates are born on **edition 2024** with the workspace on **`resolver = "3"`**.
As of this writing, `main`'s workspace manifest still says `edition = "2021"` / `resolver = "2"` —
the bump is **R1 step 0** (one PR: flip `workspace.package.edition = "2024"`,
`workspace.resolver = "3"`, `cargo fix --edition` across members, clippy clean). Nothing below
depends on 2021 semantics; three 2024-era capabilities are load-bearing:

- **Native `async fn` in traits (AFIT/RPITIT)** — `StepRuntime` and `ModelClient` are async traits
  with *no* `async-trait` macro and no boxing, because they are statically dispatched (see §3 for
  which traits are `dyn` and why).
- **`AsyncFnOnce` bounds** — `StepRuntime::step` takes an async closure, which is the natural shape
  for "a journaled block" and what the Restate SDK's `ctx.run` itself takes.
- **Resolver v3** — per-target feature resolution keeps host-only heavyweights (`restate-sdk`,
  `kube`) from unifying features into the shared crates.

Dependency-hygiene rule, enforced mechanically (an `xtask` check, same spirit as the existing
CI lint): **`restate-sdk` may appear in exactly one manifest (`agent-worker`); `kube` in exactly
one (`control-plane`); `sqlx` in exactly one (`control-plane`). No agent crate links any of them.**

## 1. Crate DAG

Seven new workspace members plus one reshaped existing one. Package names take the **`lci-*`
prefix** (LCI = Lightbridge Code Intelligence — short, and already the name of the TUI client
package `clients/lci`, which stays bare `lci`). The one existing `lightbridge-*` package,
`lightbridge-config`, is renamed **`lci-config`** in the R1a mechanical slice (manifest `name` +
every `[dependencies]` reference updated in the same commit). Directories stay flat under
`services/` like every current member.

```
                       ┌──────────────────┐
                       │   agent-types    │  data only: messages, specs, outcomes, ids
                       └───┬────┬────┬────┘
              ┌────────────┘    │    └────────────┐
      ┌───────▼──────┐  ┌──────▼───────┐  ┌───────▼──────┐
      │  agent-step  │  │ agent-tools  │  │agent-clients │  HTTP clients (control plane, embeddings)
      │ StepRuntime, │  │ Tool, Registry│ └───────┬──────┘
      │ Passthrough  │  │ Workspace,Cx │          │
      └───────┬──────┘  └──────┬───────┘          │
              └────────┬───────┘                  │
                ┌──────▼───────┐                  │
                │  agent-loop  │  AgentLoop<R,M>, TurnPolicy, ModelClient, TranscriptSink
                └──────┬───────┘                  │
                       └───────────┬──────────────┘
                           ┌───────▼────────┐
                           │  review-agent  │  the assembly: prompt, tool impls, review policies, flows
                           └───┬────────┬───┘
                     ┌─────────▼──┐  ┌──▼───────────┐
                     │agent-runner│  │ agent-worker │
                     │ (Job host, │  │ (Restate host│
                     │Passthrough)│  │  bin, R2)    │
                     └────────────┘  └──────────────┘
          agent-testkit ──(dev-dep only)──> loop / tools / review-agent
```

| Crate (`lci-…`) | Responsibility | May depend on | Must never depend on |
|---|---|---|---|
| `agent-types` | Pure data: `ChatMessage`, `AssistantTurn`, `ToolCallReq`, `ToolSpec`, `ToolOutcome`, `LoopOutcome`, `StepName`, `TranscriptEntry`, error enums | serde, uuid | tokio, reqwest, anything async |
| `agent-step` | The durability seam: `StepRuntime`, `StepError`, `Passthrough`, step-name stability testing | agent-types, tokio | restate-sdk, sqlx, kube |
| `agent-tools` | `Tool` (dyn), `ToolKind`, `ReplaySafety`, `ToolRegistry` + per-turn views, `ToolCx`, `Workspace` | agent-types, tokio | reqwest (impls live elsewhere) |
| `agent-clients` | `ControlPlaneClient`, `EmbeddingsClient` + their DTOs (moved verbatim from `agent-runner/src/bootstrap/client.rs`) | agent-types, reqwest | kube, sqlx |
| `agent-loop` | `AgentLoop<R, M>`, `TurnPolicy` + the *generic* policies, `ModelClient`, `TranscriptSink`, `TurnState`/`TurnOutcome` | agent-types, agent-step, agent-tools | reqwest, restate-sdk |
| `agent-testkit` | `ScriptedModel`, `StaticTool`, `CapturingSink`, `FailingRuntime`, the golden-transcript harness | all agent crates | (dev-dependency only — never a `[dependencies]` entry) |
| `review-agent` | The **review** assembly: prompt builder, the concrete tool impls over `agent-clients`, review-specific policies, the bootstrap/finalize flows, `ChatClient: ModelClient` | all of the above | kube, restate-sdk, sqlx |
| `agent-runner` (existing, shrinks) | Job host: CLI/bootstrap config, clone, indexer, SAST, assembles `review-agent` with `Passthrough` + `EagerWorkspace` | review-agent + its deps | restate-sdk |
| `agent-worker` (new, R2) | Restate host bin: `RestateRuntime` impl, the `ReviewAgent` workflow handler, h2c serve (mirrors `restate_worker.rs`) | review-agent + restate-sdk | kube, sqlx |

Why this many and not fewer: `agent-clients` exists because **two** consumers need it (`review-agent`
and `agent-runner`'s indexer, which calls `submit_chunks`/`submit_graph` today); `agent-testkit`
exists so golden-transcript fixtures aren't copy-pasted into three crates' `tests/`; the
`types`/`step`/`tools`/`loop` split follows the dependency arrows above — each lower crate is
usable without the ones above it (a future non-LLM durable flow can use `agent-step` alone).
Why not more: a `Flow` crate/trait is **declined** (ADR-0082 §revamp — flows are plain `async fn`s
over `&R` until a second real flow exists), and splitting policies into their own crate would
separate them from the `TurnState` they observe for no consumer's benefit.

## 2. Error taxonomy — one contract across every seam

Everything a step, tool, or model call returns is classified at the source into the engine's two
retry classes. Today this knowledge is implicit in `anyhow` chains and string matching
(`is_context_overflow`); it becomes the type every seam speaks:

```rust
// agent-types::error
pub enum StepError {
    /// Worth retrying: transport failures, 5xx, 429 (with optional server hint), timeouts.
    Transient { source: anyhow::Error, retry_after: Option<Duration> },
    /// Retrying cannot help: malformed args, unknown tool, refused call, exhausted budget,
    /// context overflow after trim. Maps to Restate's `TerminalError` in R2.
    Terminal { reason: String },
}
```

Mapping duties: `agent-clients` classifies HTTP results (5xx/429/timeouts → `Transient`, 4xx →
`Terminal`); the `ChatClient` keeps its in-step transport retries and rate-limit handling
([`ratelimit.rs`](../services/agent-runner/src/ratelimit.rs)) but surfaces what escapes them as
`StepError`; `Passthrough` retries nothing (the Job's behavior today); `RestateRuntime` maps
`Transient` → retryable `HandlerError` (engine backoff policy per step) and `Terminal` →
`TerminalError` — the same split `restate_worker.rs`'s `deliver_step` already implements for egress.

## 3. The traits — signatures and the dyn/static dispatch decisions

The rule: **static dispatch (AFIT, generics) where there is one impl per host; dynamic dispatch
(boxed futures) where a heterogeneous registry is the point.** Each trait states its choice.

### 3.1 `StepRuntime` — static, generic method, deliberately not dyn-compatible

```rust
// agent-step
pub trait StepRuntime: Send + Sync {
    /// Run `f` as the named journaled step. On replay, a completed step returns its
    /// journaled value without executing `f`.
    async fn step<T, F>(&self, name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send;

    /// Durable timer. Passthrough: `tokio::time::sleep`.
    async fn sleep(&self, name: StepName, after: Duration) -> Result<(), StepError>;

    /// A durable promise resolvable from outside the run (the internal API resolves it).
    /// Passthrough: an in-process oneshot. Reserved for `input-required` (ADR-0081); the
    /// review loop does not call it in R1/R2.
    async fn awaitable<T>(&self, name: StepName) -> Result<(AwaitableId, impl Future<Output = Result<T, StepError>>), StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static;
}
```

The generic `step` makes this trait **not dyn-compatible — on purpose**. `AgentLoop<R: StepRuntime>`
is monomorphized per host; there is never a `Box<dyn StepRuntime>`, never a runtime chosen at
runtime. A host binary compiles exactly one runtime in. (R19's sanctioned escape hatch, if the
pinned SDK's `ctx.run` closure lifetimes refuse this shape: an erased
`step_boxed(name, BoxFuture<…>)` *added* to the trait with a default forwarding impl — measured in
the G1 spike, not adopted preemptively.)

Implementations:

```rust
pub struct Passthrough;                    // agent-step: awaits f, ignores names — the Job path
pub struct RestateRuntime<'ctx> { /* wraps WorkflowContext */ }   // agent-worker (R2), never in agent-step
pub struct CheckpointRuntime { /* internal-API-backed; Option-B fallback, built only if G1/G2 fail */ }
```

### 3.2 `StepName` — the journal contract as a type

```rust
// agent-types
pub struct StepName(Cow<'static, str>);
pub mod step_names {
    pub const BOOTSTRAP: &str = "bootstrap";
    pub const FINALIZE: &str = "finalize";
    pub fn llm_turn(n: usize) -> StepName;   // "llm_turn:{n}"
    pub fn tools(n: usize) -> StepName;      // "tools:{n}"
    pub fn write_tool(n: usize, call_id: &str) -> StepName; // "tool:{n}:{call_id}"
}
```

One unit test freezes the full list and the formats; changing it fails the build with a pointer to
ADR-0082's R14 rule ("never edit a journaled step sequence in a patch release"). Evolution happens
by *adding* names, never renaming, with deployment-revision drain covering removal.

### 3.3 `Tool` — dyn, boxed future (a registry is the point)

```rust
// agent-tools
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;          // name, description, JSON schema — today's ToolDef
    fn kind(&self) -> ToolKind;
    fn replay(&self) -> ReplaySafety;
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, args: &'a str) -> BoxFuture<'a, ToolOutcome>;
}

pub enum ToolKind {
    ReadOnly(ReadKind),          // Retrieval | File | Knowledge — replaces is_retrieval_tool/is_read_only_tool
    Write,                       // add_review_comment, retract_finding, add_comment
    Terminal,                    // finish, abort
    Progress,                    // report_progress
}

pub enum ReplaySafety {
    ReadOnly,                    // replay harmless
    Idempotent,                  // last-write-wins server-side (add_review_comment, retract, finish)
    NeedsDedupKey,               // must be journaled per-call with a dedup key (add_comment until G4)
}
```

`call` returns a boxed future because the registry stores `Arc<dyn Tool>` — heterogeneous dispatch
is the reason the trait exists, and one allocation per tool call is noise next to the network I/O
every tool performs. `ToolOutcome` keeps today's shape (`Continue(result)` / `Finish` / `Abort` /
recoverable-error-as-message).

**The registry and per-turn views** — everything that is an inline branch in `agent.rs` today
becomes a pure filter over specs, computed by policies and applied by the view:

```rust
pub struct ToolRegistry { tools: Vec<Arc<dyn Tool>> }

impl ToolRegistry {
    /// Registration asserts the runtime can honor the tool's replay contract:
    /// a NeedsDedupKey tool on a host without per-call dedup is a *startup* error, not a runtime double-post.
    pub fn register(&mut self, t: Arc<dyn Tool>, caps: RuntimeCaps) -> Result<(), RegistryError>;
    /// The turn's offered set: allowlist ∩ kind-filters ∩ budget-drops, in policy order.
    pub fn view(&self, filter: &TurnFilter) -> TurnView<'_>;
}

pub struct TurnView<'r> { /* offered specs for the chat request + guarded dispatch */ }
impl TurnView<'_> {
    pub fn specs(&self) -> &[ToolSpec];
    /// Dispatch one call; a call to a non-offered tool returns the refusal steer
    /// (today's fast-tier refusal synthesis) instead of executing.
    pub async fn dispatch(&self, cx: &ToolCx<'_>, call: &ToolCallReq) -> ToolOutcome;
}
```

### 3.4 `Workspace` — dyn, the `ensure_checkout` guard as a type

```rust
// agent-tools
pub trait Workspace: Send + Sync {
    /// The checkout root, materializing it if this pod has never had it (or lost it).
    async fn root(&self) -> Result<&Path, StepError>;
}
// impls: EagerWorkspace (agent-runner: cloned at bootstrap, today's behavior)
//        LazyWorkspace  (agent-worker: clone-if-missing at the journaled head_sha — ADR-0082 §local state)
```

Small and dyn (`Arc<dyn Workspace>` inside `ToolCx`): two impls coexist and tools must not know
which they got. `ToolCx` carries what today's `Tools` struct carries, re-typed:

```rust
pub struct ToolCx<'a> {
    pub task_id: Uuid,
    pub cp: &'a ControlPlaneClient,      // agent-clients
    pub embedder: &'a EmbeddingsClient,  // agent-clients
    pub workspace: &'a dyn Workspace,
}
```

### 3.5 `ModelClient` — static (one impl per assembly), AFIT

```rust
// agent-loop
pub trait ModelClient: Send + Sync {
    async fn complete(&self, req: ChatRequest<'_>) -> Result<AssistantTurn, StepError>;
}
```

`review-agent::model::ChatClient` (today's `chat.rs` + `ratelimit.rs`, moved) is the sole impl:
streaming, per-chunk idle timeout, transport retries, provider `extra` passthrough, reasoning
capture — all *inside* `complete`, invisible to the loop. ADR-0075 stands: native transport, no Rig.

### 3.6 `TurnPolicy` — dyn, sync methods, ordered composition

```rust
// agent-loop
pub trait TurnPolicy: Send {
    fn name(&self) -> &'static str;                       // shows up in tracing + transcripts
    fn before_turn(&mut self, s: &TurnState<'_>) -> Vec<PolicyAction>;
    fn after_turn(&mut self, _s: &TurnState<'_>, _o: &TurnOutcome) {}
}

pub enum PolicyAction {
    Narrow(TurnFilter),          // tighten the offered tool set (monotonic: only ever narrows)
    Inject(Nudge),               // a system message appended this turn (wind-down, coverage bounce)
    TrimHistory { target_tokens: usize },
    ForceFinish { reason: &'static str },   // the Exhausted backstop
}
```

Merge semantics are fixed and simple: actions apply in policy-registration order; `Narrow` only
intersects (a later policy cannot re-widen); `Inject`s concatenate; any `ForceFinish` ends the loop
after the current turn. Policies are `&mut self` state machines — exactly what the loop's ad-hoc
`bool` flags (`winddown_announced`, `suppress_record`) are today, but named, isolated, and
unit-testable with a synthetic `TurnState`.

Where each lives:

| Policy | Crate | Replaces (today) |
|---|---|---|
| `TurnBudget` | agent-loop | `winddown_turn` / halfway + exhausted backstop ([agent.rs:110-130,470-485](../services/agent-runner/src/review/native/agent.rs)) |
| `ContextWindowTrim` | agent-loop | `estimate_tokens` / `trim_tool_history` / `WINDDOWN_TOKEN_FRACTION` (agent.rs:177-257,566-585) |
| `WindDown` | agent-loop | reduced tool set + converge nudge (agent.rs:559-604) |
| `ReadBudgets` | agent-loop | per-category drops: files/searches/batches, ADR-0042 (agent.rs:528-534,586-622) |
| `FastTierGuard` | review-agent | fast-tier offered-set enforcement + refusal (ADR-0062; agent.rs:593-649) |
| `CoverageGate` | review-agent | changed-files coverage nudges/bounces/disclosure, ADR-0069 (agent.rs:1434-1536) |
| `ScratchpadLoopGuard` | review-agent | `suppress_record` one-turn suppression (agent.rs:605-651) |

Generic vs review-specific is the split test: a policy that reads only `TurnState` (turn index,
token estimate, tool-use counters) is generic; one that knows about diffs, changed files, or tiers
is review flavor.

### 3.7 `TranscriptSink` — dyn, the ADR-0034/0060 capture seam

```rust
pub trait TranscriptSink: Send {
    fn record(&mut self, entry: TranscriptEntry);        // assistant turns, tool results, policy events
}
```

The Job host keeps today's end-of-run `submit_transcript`; the worker host flushes incrementally
(ADR-0082 §finalize) — same trait, host-chosen flushing.

## 4. `AgentLoop` — the engine, and the step map it owns

```rust
// agent-loop
pub struct AgentLoop<R, M> {
    runtime: R,                              // StepRuntime — monomorphized per host
    model: M,                                // ModelClient
    tools: ToolRegistry,
    policies: Vec<Box<dyn TurnPolicy>>,
    sink: Box<dyn TranscriptSink>,
    limits: LoopLimits,                      // max_turns + max_batch_size (ADR-0042 clamps)
}

impl<R: StepRuntime, M: ModelClient> AgentLoop<R, M> {
    pub async fn run(&mut self, seed: Conversation) -> Result<LoopOutcome, StepError> { /* … */ }
}
```

`run`'s turn body — the deterministic glue ADR-0082 requires to be replay-safe, with every effect
behind `self.runtime.step(..)`:

```text
for turn in 0..max_turns:
    actions  = policies.before_turn(state)            # pure over journaled history — replay-safe
    view     = tools.view(merge_narrowings(actions))
    request  = build_request(messages, view.specs(), injections(actions))
    turn_msg = runtime.step(llm_turn(turn), || model.complete(request)).await?     # journaled
    sink.record(assistant(turn_msg)); messages.push(turn_msg)
    (reads, writes, terminals) = partition(turn_msg.tool_calls, view)
    if reads:                                          # the whole batch = ONE journaled step
        results = runtime.step(tools(turn), || view.dispatch_batch(cx, reads)).await?
        messages.extend(results); sink.record_all(results)
    for w in writes ++ terminals:                      # each its own step — ReplaySafety honored
        out = runtime.step(write_tool(turn, w.call_id), || view.dispatch(cx, w)).await?
        …Finish/Abort → break with LoopOutcome…
    policies.after_turn(state, outcome)
force-finish backstop → LoopOutcome::Exhausted
```

Two invariants the engine enforces (not conventions): a `NeedsDedupKey` tool call's step name
embeds the `call_id` (the dedup key exists *because* the step is named per-call), and the batch
step's journaled value is the ordered vector of `(call_id, outcome)` so replay reconstructs
`messages` byte-identically.

## 5. Assembly — the two hosts differ only in what they inject

```rust
// agent-runner (Job host) — R1, behavior-identical to today
let cx      = ToolCx { task_id, cp: &cp_client, embedder: &embedder, workspace: &EagerWorkspace::cloned(&checkout) };
let tools   = review_agent::tools::registry(RuntimeCaps::passthrough())?;   // full built-in set + MCP discovery
let polys   = review_agent::policies::for_tier(&cfg);                       // table in §3.6
let mut loop_ = AgentLoop::new(Passthrough, ChatClient::from(&cfg), tools, polys, JobSink::new());
let outcome = review_agent::flows::run_review(&Passthrough, &mut loop_, &cx, inputs).await?;
```

```rust
// agent-worker (Restate host) — R2, gated
impl ReviewAgent for ReviewAgentImpl {
    async fn run(&self, ctx: WorkflowContext<'_>, task: TaskRef) -> Result<(), HandlerError> {
        let rt   = RestateRuntime::new(&ctx);                       // the ONLY line that knows Restate
        let cx   = ToolCx { workspace: &LazyWorkspace::at(journaled.head_sha), ..same };
        let mut loop_ = AgentLoop::new(rt, ChatClient::from(&journaled.config), same_tools, same_policies, IncrementalSink::new());
        review_agent::flows::run_review(&rt, &mut loop_, &cx, journaled.inputs).await?
    }
}
```

`flows::run_review` is a plain `async fn(&impl StepRuntime, …)` composing `bootstrap` →
(SAST digest arrives as an *input* — the scan itself stays in a sandboxed Job, ADR-0082 §worker) →
`loop_.run(seed)` → `finalize`. No `Flow` trait.

## 6. Migration map (R1) — every current module has exactly one destination

| Today | Lines/anchor | Destination |
|---|---|---|
| `review/native/agent.rs` `ReviewOutcome` | 47-101 | `agent-types::LoopOutcome` (generic) + review flavor in `review-agent` |
| `agent.rs` wind-down helpers, `estimate_tokens`, `trim_tool_history` | 103-257 | `agent-loop::policy::{WindDown, ContextWindowTrim}` |
| `agent.rs` `run_native_agent` — loop core | 258-1190 | `agent-loop::AgentLoop::run` (engine) + `review-agent::flows` (assembly) |
| `agent.rs` `PromptBudgets`, `build_messages`, `cap_prompt_block` | 1191-1397 | `review-agent::prompt` |
| `agent.rs` `is_read_only_tool` / `is_retrieval_tool` / `arg_field` | 1398-1433 | deleted — `ToolKind` metadata + typed args |
| `agent.rs` coverage fns (`coverage_*`, `amend_summary_with_coverage`) | 1434-1536 | `review-agent::policy::CoverageGate` |
| `review/native/chat.rs` + `ratelimit.rs` | whole | `review-agent::model::ChatClient` (impl `ModelClient`) |
| `review/native/tools.rs` — trait-shaped surface | consts, `dispatch` 342-… | trait/registry → `agent-tools`; each tool one module under `review-agent::tools::{vector, graph, read_file, record, reply, finish, mcp}` |
| `bootstrap/client.rs` (`ControlPlaneClient`, `EmbeddingsClient`) | whole | `agent-clients` (verbatim move; indexer keeps using it) |
| `bootstrap/config.rs`, `clone.rs`, `indexer/`, `sast/`, `main.rs` host glue | whole | stay in `agent-runner` (the Job host) |
| `control-plane/src/restate_worker.rs` | pattern only | `agent-worker` mirrors its h2c serve + Health service shape (R2) |

R1 lands as five PRs, each green and behavior-identical (goldens run from R1c on):
**R1a** edition-2024/resolver-3 bump + empty crate skeletons with the dependency-hygiene xtask
check; **R1b** `agent-clients` extraction (mechanical move, importers updated); **R1c**
`agent-tools` + the tool port + `agent-testkit` with the golden-transcript harness; **R1d**
`agent-loop` + policy extraction under goldens; **R1e** `review-agent` assembly, `agent-runner`
reduced to host glue, old modules deleted.

## 7. Golden-transcript harness (the R1 merge bar, mechanically)

`agent-testkit` provides `ScriptedModel` (a `ModelClient` returning a recorded sequence of
assistant turns) and `StaticTool`s with canned outputs. A golden test drives **today's loop** and
**the extracted loop** with the same script and asserts the *full transcript* — message sequence,
offered-tool sets per turn, policy events, final outcome — is byte-identical. Fixtures cover: a
plain converge-and-finish run, a wind-down entry, a context-trim trigger, a fast-tier refusal, a
coverage bounce, and the exhausted backstop. These goldens are the R18 mitigation and stay as
regression tests after R1e deletes the old loop.

## 8. Open questions (tracked, not blocking acceptance)

1. **`awaitable` shape** — resolved-from-outside promises need an id the internal API can route;
   whether the id is minted by the runtime (Restate awakeable id) or by us (task-scoped name) is
   settled in the ADR-0081 slice, not R1.
2. **`agent-worker` image** — own image vs. a second binary in the existing runner image family
   (the #337 shared release pipeline makes either cheap); decide at R2 with the Helm chart change.
3. **Config plumbing** — `review-agent` currently reads the runner's file config
   (`bootstrap/config.rs`); whether the worker host reuses that file shape or takes the journaled
   snapshot as its only source is an R2 detail (ADR-0082 §bootstrap prescribes the snapshot).
