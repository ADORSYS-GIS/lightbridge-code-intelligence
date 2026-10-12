# 12 — The OpenCode integration

File `06` already told the review-loop story end to end and covered, in its §4/§4a/§4b, what OpenCode
*does* inside that story: it's the live review host, the native loop is retiring, and the policy split
between "OpenCode's loop mechanics" and "our reused Rust quality gates." This file does not repeat that
— go there for the turn-loop narrative. This file goes one level deeper and broader: what OpenCode *is*
as a vendored piece of software, how it's packaged into this repo's images, and the full mechanics of
`services/review-agent/src/opencode/` (the core) and `services/review-mcp/` (the tool-exposure server)
as subjects in their own right, plus the ACP transport and the ADRs that actually govern this area.

Every claim below is checked against the source as of this writing. The repo's own
`system_reports/10-opencode-integration.md` is used only as a lead — it gets three things wrong, called
out precisely in §7.

## 1. What OpenCode actually is, per this repo's own code

This repo never builds OpenCode from source; it vendors a pinned upstream release. From the fetch stage
shared (with different pins tracked independently) by both Dockerfiles:

```dockerfile
ARG OPENCODE_VERSION=v1.18.2
ARG OPENCODE_SHA256_AMD64=36cb91ee3b6493f38bc50aa49bb241f3dbc620fcea938eb0776069213d0bb5ee
...
curl -fsSL -o /tmp/opencode.tar.gz \
  "https://github.com/anomalyco/opencode/releases/download/${OPENCODE_VERSION}/${asset}"; \
echo "${sha}  /tmp/opencode.tar.gz" | sha256sum -c -; \
```
(`services/agent-runner/Dockerfile.review-opencode:26-29,38-40`, identically in
`integrations/opencode/Dockerfile:20-39`)

So, strictly from what this repo's own build files assert: OpenCode is a release artifact published by
`anomalyco/opencode` on GitHub, fetched as a `.tar.gz`, SHA-256-verified against a hardcoded pin before
extraction. RFC-0009 (§2) describes it functionally — "OpenCode owns the LLM loop, provider quirks, MCP
client, and subagent orchestration" over the Vercel AI SDK's provider layer, with a TypeScript plugin
system (`tool.execute.before/after`, `permission.ask`, `chat.params`, `event`) as the extension point —
and the repo's build comments independently confirm it's a Bun-hosted binary: "OpenCode's bundled Bun
transpiles the .ts in place" (RFC-0009's resolved unresolved-question note, `docs/rfc/0009-opencode-acp-agent-host.md:259`).
I'm not asserting anything about OpenCode's maintainers, license, or history beyond what these files
state — the repo's code gives no further provenance to check against.

One concrete, verifiable, and non-obvious fact about the binary: **it is musl-dynamic, not static.**
The Dockerfile's own comment is explicit about this, and it matters for the base-image choice:

```dockerfile
# BASE = Alpine (deliberate divergence from the repo's debian house base, like integrations/opencode /
# ADR-0094): the OpenCode release binary is musl-DYNAMIC (`interpreter /lib/ld-musl-x86_64.so.1`), so it
# needs the musl loader + libgcc/libstdc++ and cannot run on glibc/debian. The agent-runner +
# lci-review-mcp binaries are fully-STATIC musl (ADR-0080), so they run on Alpine unchanged.
```
(`services/agent-runner/Dockerfile.review-opencode:9-12`)

That is the opposite of "statically-linked," which is the claim `system_reports/10-opencode-integration.md`
makes (see §7.1) — the OpenCode binary needs `libgcc`/`libstdc++` at runtime, which is exactly why the
runtime stage installs them (`services/agent-runner/Dockerfile.review-opencode:109-110`:
`apk add --no-cache libgcc libstdc++ ripgrep git ca-certificates`). What *is* static in this image is
this project's own Rust output (`agent-runner`, `lci-review-mcp`) — different binaries, different
linkage, and the report conflates them.

## 2. Two images, two modes — and the one that's actually live

There are **two** Dockerfiles that vendor OpenCode, for two different modes, and only one of them is
on the live task-dispatch path:

- **`integrations/opencode/Dockerfile`** — the `open`-mode image (RFC-0009 Phase 1 / ADR-0094). Its own
  comment says the supervisor binary "is not built yet... Until then the entrypoint is opencode itself"
  (`integrations/opencode/Dockerfile:64-67`) — i.e. this image still describes a state from before the
  ACP host existed. `open` mode is a separate surface from `review`; nothing in this deep dive's grep of
  `agent-runner`/`control-plane` wires live task dispatch to it, and file `06` doesn't mention it either.
- **`services/agent-runner/Dockerfile.review-opencode`** — the one that matters for review. Its header
  states plainly what it assembles: "the combined `lightbridge-agent-review` image AFTER the cutover:
  the agent-runner Job binary (carrying the ADR-0097 `perform_review` → `run_opencode_agent` swap)
  spawns `opencode acp`, which in turn spawns the stdio `lci-review-mcp`... and loads the first-party
  recorder/gate/logger plugins" (`services/agent-runner/Dockerfile.review-opencode:4-7`).

This second image is assembly-only (ADR-0080 — "nothing is compiled here; the Rust binaries arrive
prebuilt... in dist/"), and it vendors **four** separately-pinned, SHA-verified third-party pieces, not
just OpenCode:

| what | version | why pinned here | source line |
|---|---|---|---|
| `opencode` | `v1.18.2` | the agent-loop host itself | `Dockerfile.review-opencode:26-29` |
| `opengrep` (SAST) | `v1.23.0` | the `run_sast` tool's scan engine (ADR-0073), now reachable from the OpenCode path too | `:51-54` |
| `opengrep-rules` | shallow-cloned HEAD | the ruleset `opengrep --config` points at | `:74-78` |
| `github-mcp-server` | `v1.7.0` | opt-in GitHub MCP server for the reviewer (ADR-0105) | `:88-91` |

The runtime stage copies in the prebuilt Rust binaries plus the plugin tree, and sets the entrypoint to
the Rust supervisor, not to `opencode`:

```dockerfile
COPY agent-runner /usr/local/bin/agent-runner
COPY lci-review-mcp /usr/local/bin/lci-review-mcp
COPY opencode-plugins /opt/lightbridge/opencode/plugins
...
ENTRYPOINT ["/usr/local/bin/agent-runner"]
```
(`services/agent-runner/Dockerfile.review-opencode:132-134,148`)

So the pod's actual process tree, confirmed by the ENTRYPOINT plus the ACP-host code in §4, is:
`agent-runner` (Rust, the Job's PID 1) → spawns `opencode acp` (Bun, a child) → which, per its own
rendered config, spawns `lci-review-mcp` (Rust, a grandchild, stdio) and loads four TypeScript plugins
*in-process inside the opencode runtime*, not as separate children. The plugins never run their own
process — they're `import type`-only `.ts` files OpenCode's bundled Bun transpiles and loads into its
own Node/Bun process (per RFC-0009's resolved note, `docs/rfc/0009-opencode-acp-agent-host.md:256-259`).

The opengrep + github-mcp-server stages exist because `lci-review-mcp` is the one dispatching
`run_sast` and (opt-in) proxying a second, independently-spawned `github-mcp-server` MCP child — see
§5 and §6.

## 3. The ACP transport: a real JSON-RPC client, not a CLI wrapper

The system report calls this "Agent Control Protocol"; the actual name, confirmed by the crate's own
doc comment, is **Agent Client Protocol**:

```rust
//! ACP (Agent Client Protocol) client for driving an `opencode acp` subprocess — the foundation of
//! the agent-plane supervisor ([ADR-0094](../../docs/adr/0094-opencode-acp-open-mode-host.md)).
//!
//! The supervisor spawns OpenCode as a **child process** and drives it over newline-delimited
//! JSON-RPC on stdio (the embedded-stdio decision in ADR-0094 — not a `--port` sidecar).
```
(`services/acp-host/src/lib.rs:1-5`)

`lci-acp-host` (crate `services/acp-host`, package `lci-acp-host`) is a small, dependency-light crate
(`anyhow`, `serde`/`serde_json`, `tokio`, `tracing` — nothing else, per `services/acp-host/Cargo.toml:21-26`)
built around one type, `AcpClient`. It's a real bidirectional JSON-RPC client, not a thin CLI wrapper:

- **Spawn**: `Command::new(bin).arg("acp")...stdin(piped).stdout(piped)...kill_on_drop(true)`
  (`services/acp-host/src/lib.rs:95-106`) — `kill_on_drop` matters because tokio does not reap an OS
  child on `Drop` otherwise, which would orphan the opencode process if the client were ever dropped
  without an explicit `shutdown()`.
- **Request/response correlation**: each outbound call gets a monotonic `i64` id and a `oneshot`
  channel registered in a `Pending` map (`HashMap<i64, oneshot::Sender<Value>>` behind an `Arc<Mutex<_>>>`,
  `services/acp-host/src/lib.rs:38,146-168`); a background `reader_loop` reads newline-delimited JSON
  off stdout, and a bare response (no `method`) resolves the matching pending sender.
- **Three outbound methods implemented**: `initialize` (`:171-180`), `session/new` (`:184-196`,
  passing `mcpServers` verbatim — more on why that's usually empty in §4), and `session/prompt`
  (`:199-205`, one text block per call — this is the exact method the "one `session/prompt` runs
  OpenCode's whole internal loop" framing in file `06` §4b refers to).
- **Inbound method the agent itself can call**: `session/request_permission`, answered synchronously
  from a `PermissionPolicy` (`AllowFirst` or `Cancel`) without ever going interactive
  (`decide_permission`, `:310-333`). The review host always passes `PermissionPolicy::Cancel`
  (`services/agent-runner/src/review/opencode.rs:487`) — a review is read-only by config already, so a
  permission request arriving at all would be unexpected, and the safe answer is to refuse it.
- **Unknown inbound methods** get a JSON-RPC `-32601` error reply so the agent is never left hanging on
  an unanswered request (`services/acp-host/src/lib.rs:290-300`).
- **Reader exit**: when the child's stdout hits EOF, every still-pending sender is dropped, so an
  in-flight `request()` wakes with a channel-closed error rather than awaiting forever
  (`:304-307`).

There's also a tiny smoke binary, `acp-smoke` (`services/acp-host/src/bin/acp-smoke.rs`), described in
its own Cargo.toml entry as "Verification/smoke binary... Not a deployed entrypoint" — it spawns a real
`opencode acp`, does `initialize` + `session/new`, optionally one `session/prompt`, and prints the
handshake. It's the Rust-side manual-verification tool for the ACP wire, independent of any review
plumbing.

One ACP detail worth being precise about, because it's easy to get backwards: **stdio MCP servers are
NOT wired through `session/new.mcpServers`.** `new_session` passes that parameter through, but ADR-0097
decision 4 states plainly: "`session/new.mcpServers` is a JSON array; stdio MCP is wired via the config
`mcp` block... The mediated review tools (`lci-review-mcp`...) are stdio MCP declared in the rendered
config, not via `session/new` (which honours only http/sse there)." The review host's own call site
confirms this is why it's empty: `serde_json::json!([])` with the comment "the stdio review MCP is
wired via the config `mcp` block, not here" (`services/agent-runner/src/review/opencode.rs:500-507`).

## 4. `services/review-agent/src/opencode/` — the parity-critical core

This is the module file 06 cites by section but doesn't walk file-by-file. Its module doc (46 lines,
not the "1–25" file 06 was quoting from an earlier revision) is the map:

```rust
//! OpenCode review host — the parity-critical core (RFC-0009 / ADR-0094/0095 review cutover, slice 3).
//! ...
//! - **Loop mechanics** (context-trim, wind-down, read/turn budgets — ADR-0042 batching): handed to
//!   OpenCode's own maintained loop. The supervisor cannot narrow OpenCode's tools mid-internal-loop.
//! - **Review-quality gates** ([`CoverageGate`](crate::policies::CoverageGate) /
//!   [`RefuteGate`](crate::policies::RefuteGate)): kept as Rust and run here, reusing the exact tuned
//!   `TurnPolicy` code — no TypeScript reimplementation of the coverage denominator, citation
//!   crediting, or the ADR-0091 absence-claim directive.
```
(`services/review-agent/src/opencode/mod.rs:1-14`)

It names five sibling files by responsibility (`:17-25`) and re-exports everything so callers use
`lci_review_agent::opencode::{…}` unchanged (`:40-46`). Taking each in turn:

### 4a. `config.rs` (866 lines) — three-layer config rendering

`render_review_config` builds the per-task `OPENCODE_CONFIG` JSON by merging three layers, documented
in the file's own module doc as "last-writer-wins, merged host-side" (`config.rs:1-3`):

1. **Base** — the checked-in `integrations/opencode/config/review.jsonc`, baked in at *compile time*
   via `include_str!("../../../../integrations/opencode/config/review.jsonc")` (`config.rs:41-42`).
   Because it's `include_str!`, the running binary needs no copy of the file on disk — it's compiled
   in. The base is `.jsonc` specifically because OpenCode's config schema "REJECTS unrecognized keys...
   even a `\"//\"` comment KEY fails" (`review.jsonc:47-50`), so `strip_jsonc_comments` — a
   string-literal-aware stripper that respects escapes so a `\"` inside a string value doesn't
   terminate early (`config.rs:384-388` onward) — removes the comments before `serde_json::from_str`.
2. **Runtime injection** (`inject_runtime`, `config.rs:155-193`) — patches four things the base can't
   express statically: the reasoning capability flag (`reviewer["reasoning"] = extra.contains_key("reasoning_effort")`,
   `:165`, ADR-0103's "no structural tier flag" rule — reasoning turns on purely because the preset's
   `review.extra` carries a `reasoning_effort` key), the whole `review.extra` passthrough map merged
   into `reviewer["options"]` (`:174-181`, this is literally what makes `reasoning_effort: "high"`
   reach the eaig gateway for a deep review — the ADR-0097 update note in §8 is about exactly this),
   `temperature` when the tier sets one (`:183-185`, applied *after* `extra` so a tier's explicit
   temperature wins over one an operator snuck into `extra`), and the per-project billing attribution
   headers (`:188-192`, dynamic map keys, so they can't be a static `{env:*}` placeholder).
3. **Operator overlay** — the trusted `review.opencode` object from ai-helm-values, deep-merged last
   with full override: "two objects merge recursively; for any other pair... the overlay value
   REPLACES the base value" (`deep_merge`, `config.rs:201-218`). It's applied "ONLY when it is a
   non-empty object" (`:127-133`) — a malformed overlay (a string or array where an object belongs)
   is ignored rather than allowed to replace the whole config root with a scalar.

The `Floor` struct (`config.rs:222-231`) snapshots four invariants from the *injected base, before the
overlay* — disabled built-ins, denied permissions, the `lightbridge` MCP entry, and the plugin list —
and `Floor::diff` (`:275-381`) compares them against the post-overlay config to produce `FloorBreach`es:
a re-enabled built-in, an opened permission (checked both top-level *and* per-agent, since an overlay
could narrow the relaxation to one sub-agent — `:322-360`), a dropped/replaced `lightbridge` MCP entry,
or a dropped required plugin. None of these ever blocks the run — `RenderedReviewConfig::disclosure_note`
(`:68-88`) turns any breaches into one sentence appended to the review's coverage disclosure, and the
host logs each as a `tracing::warn!` (`services/agent-runner/src/review/opencode.rs:224-230`). This is
ADR-0099's stated policy: the overlay is trusted and always wins, but a relaxation is made *visible*,
never *impossible*.

### 4b. `driver.rs` (208 lines) — the pure re-prompt/finalize control loop

`ReviewDriver::on_cycle` (`driver.rs:68-91`) is the whole state machine, and it's small enough to state
exactly: an `abort_reason` on the cycle short-circuits straight to `ReviewResolution::Aborted` without
even consulting the gates (`:70-72`, "an aborted run is unverified"); otherwise it asks `ReviewGates` to
`observe_cycle`, and a `GateDecision::Accept` always wins over the cycle budget — "never turn a valid
finish into exhaustion" (`:74-77`); any other decision is checked against `max_cycles` first, a hard
ceiling enforced for *both* a gate bounce and a keep-going nudge, with the comment explicitly citing a
PR review finding (gemini #446) that the driver's own budget — not the gates' internal bounce counters —
must be the authoritative backstop against an unbounded loop (`:78-87`). Only once under budget does it
distinguish a gate bounce (`RejectFinish`, re-prompt with the gate's own nudge text) from a plain
not-finished cycle (re-prompt with the fixed `CONTINUE_NUDGE` string, `:15-17`, "the OpenCode-cycle
analogue of the native loop re-prompting a turn that produced no terminal signal").

### 4c. `drive.rs` (162 lines) — the host-independent loop, abstracted over `ReviewSession`

`run_review_loop` (`drive.rs:31-45`) is genuinely tiny: prompt the session, reconstruct a `TurnOutcome`
from the returned events via `cycle_turn_outcome`, feed it to the driver, and either loop with a new
prompt or return. The entire point of this file is the one-method trait it's built on:

```rust
pub trait ReviewSession {
    async fn prompt(&mut self, text: &str) -> anyhow::Result<Vec<RecorderEvent>>;
}
```
(`drive.rs:23-25`)

That's the full seam between "pure, unit-testable orchestration" and "a live `opencode acp` child plus
a recorder-file tail." The test module implements it with a scripted `FakeSession` that pops
pre-baked event batches off a `VecDeque` (`:54-75`) and exercises coverage-bounce-then-finish,
never-finishes-until-exhausted, and transport-error-propagates scenarios without ever touching a real
process — the production implementor, `OpencodeReviewSession`, lives in the agent-runner host (see §4e).

### 4d. `recorder.rs` (335 lines) — reconstructing a `TurnOutcome` from OpenCode's own JSONL

This answers the task's specific question precisely: `cycle_turn_outcome` (`recorder.rs:144-236`)
reassembles one `TurnOutcome` from the recorder's raw events for a single `session/prompt` cycle.

The recorder plugin (TypeScript, §6) writes one JSON object per line for every `tool.execute.before` /
`tool.execute.after` hook firing, tagged `"kind": "tool.before"` / `"tool.after"`. `RecorderEvent`
(`recorder.rs:74-88`) deserializes only the fields the adapter needs — `kind`, `tool`, `call_id`
(`callID` in the JSON), `args`, `result` — and is "deliberately lenient (`Option` everywhere) so a
half-written or unexpected line degrades to 'no data' rather than failing the parse" (`:72-73`).
`parse_recorder` (`:93-99`) skips blank or unparseable lines outright for the same reason.

The reconstruction walks events in order, building a `Vec<Pending>` of in-flight calls matched by
`call_id` — a `before` pushes a new pending slot, an `after` attaches to "the most recent same-id call
still awaiting its result" (`:161-168`), and an orphaned `after` with no matching `before` (a cycle
boundary cutting a pair) still gets kept so its result counts (`:170-181`). Each finished call becomes
one `ToolCallResult`; a call normalized to `FINISH` sets `finish_requested`, one normalized to `ABORT`
sets `abort_reason` (preferring the `"reason"` arg, falling back to the result text, `:204-213`), and
everything else becomes `ToolOutcome::Continue(result_text)`.

Two subtleties that make this adapter non-trivial, both load-bearing for coverage accuracy:

- **Name normalization** (`normalize_tool_name`, `:56-69`). OpenCode prefixes every MCP tool with its
  server config key — `lightbridge` — so a call the model makes to `read_file` arrives at the recorder
  as `lightbridge_read_file`. But a few tools are *already* natively `lightbridge_`-prefixed
  (`lightbridge_vector_semantic_search`), so a naive strip would either double-strip or miss them.
  The function instead matches the *longest* known canonical name that is a suffix of the raw id at a
  non-identifier boundary — so `lightbridge_lightbridge_vector_semantic_search` (a doubled prefix) and
  `lightbridge_vector_semantic_search` both normalize to the same canonical name, while `spread_file`
  does *not* spuriously match `read_file` (the preceding char `p` is alphanumeric, not a boundary). The
  test module proves both directions explicitly (`:244-273`).
- **Result-text extraction** (`result_text`, `:105-127`). The mediated review tools return the MCP
  shape (`{content:[{type,text}],isError}`); built-ins would return `{title,output,...}`; and a bare
  JSON string is unwrapped unquoted so "downstream substring matching (e.g. the refute gate's 'recorded
  finding' check) isn't tripped by literal quotes" (`:121-123`, citing gemini #442). Getting this wrong
  silently breaks every gate that matches on the dispatch message's text.

`KNOWN_REVIEW_TOOLS` (`:28-47`) is the complete list the gates can recognize, including a forward-looking
entry: the ADR-0104 fs-tool trio (`WRITE_FILE`, `EDIT_FILE`, `LIST_DIRECTORY`) is listed even though "a
future consumer (`open` mode) gets uniform recorder normalization for free" is the only reason it's
there today — `tools.rs`'s `fs_write` gate is always false in production review, per the comment.

### 4e. `gates.rs` (452 lines) — driving the reused gates over OpenCode cycles, with a process-boundary SAST recovery trick worth calling out

`ReviewGates` composes the identical `CoverageGate` + `RefuteGate` the native flow uses (file 06 §4a
already covers what those two do and their registration-order contract — not repeated here), plus a
third gate the native flow also carries, `SastAnchorGate`, composed unconditionally: "it stays inert (no
leads) unless the model actually calls `run_sast`" (`gates.rs:80-82`).

The one genuinely new mechanism here, beyond what file 06 describes for the native path, is how the SAST
anchor gate gets fed across a process boundary. Natively, `run_sast` runs in-process and can push
directly into a shared `SastLeadSink`. On the OpenCode path, `run_sast` runs inside the *separate*
`lci-review-mcp` process (§5) — there's no shared memory to push into. So `ReviewGates::ingest_sast_leads`
(`gates.rs:105-129`) recovers the leads from the tool's own *result text*: the tool returns a digest
string (`lci_agent_sast::digest`), and `lci_agent_sast::parse_digest_leads` — "pinned as a true inverse
of `digest`" (`:114-115`) — parses that same digest back into `SastLead`s, which are then pushed into the
sink the `SastAnchorGate` reads. The test `sast_anchor_bounces_a_misanchored_verdict_recovered_from_the_run_sast_digest`
(`:349-408`) proves this round-trip concretely: a `run_sast` digest flags `.env:216`, the model records a
"false positive" at the *wrong* line `.env:60`, and the recovered lead is still enough for the anchor
gate to bounce the finish and name the real line — exactly the native gate's behavior, reconstructed
from text alone.

`observe_cycle` (`:135-175`) always feeds the outcome to all three gates (so engagement accumulates
every cycle, not only finish cycles), and only evaluates a verdict when the cycle actually requested
finish: "Native registration order = evaluation order (flows.rs): coverage → refute → SAST anchor"
(`:155`) — the three gates' `RejectFinish` nudges, if any, are joined with blank lines into one combined
re-prompt (`:156-167`). An accepted finish runs the coverage gate's `finish_actions` a second time
purely to flush any disclosure note into `coverage_state` (`:170-174`).

### 4f. `test_support.rs` (34 lines)

Two tiny constructors, `before`/`after`, building `RecorderEvent`s in the MCP result shape the mediated
tools actually return at runtime — shared by the `recorder`/`gates`/`driver`/`drive` test modules so
none of them hand-roll the same JSONL shapes repeatedly. Mirrors `crate::policies::test_support`'s role
for the native-path tests.

## 5. `services/review-mcp` — the stdio MCP server OpenCode spawns

`lci-review-mcp` (binary + package name both, `services/review-mcp/Cargo.toml:1-10`) answers the task's
question about tool provisioning directly: yes, it's a dedicated MCP server, and its own module doc
states its purpose without ambiguity:

```rust
//! This is the load-bearing missing piece for running review on OpenCode: the review tools
//! (`add_review_comment`, `finish`, retrieval, graph, `read_file`, …) live as in-process Rust in
//! `lci-review-agent` and proxy to the control plane's `/internal/tasks/{id}/…` API. OpenCode can't
//! call in-process Rust — so this binary re-exposes them over MCP by REUSING
//! `lci_review_agent::tools::Tools` verbatim (its `specs()` for `tools/list`, its `dispatch()` for
//! `tools/call`).
```
(`services/review-mcp/src/main.rs:4-10`)

That dependency is explicit in the Cargo manifest — `lci-review-mcp` depends on `lci-review-agent` (the
same crate §4 lives in), plus `lci-agent-clients`, `lci-agent-types`, `lci-agent-sast`
(`services/review-mcp/Cargo.toml:18-25`). It is not a re-implementation of the tools; it's a thin
protocol adapter over the exact same `Tools` registry the native loop would call in-process.

**Discovery.** OpenCode discovers this server because the rendered config's `mcp.lightbridge` entry
names it as a `"local"` (stdio) server with `"command": ["lci-review-mcp"]` (`review.jsonc:116-121`,
§4a layer 1). OpenCode spawns it as a child, and the `lci-review-mcp` binary speaks plain JSON-RPC
over stdin/stdout: `initialize`, `tools/list`, `tools/call`, `ping` (`services/review-mcp/src/main.rs:304-346`).
It reads one line at a time from stdin, spawns each request as its own `tokio::spawn`'d task so
"batched tool calls (the review does parallel read-only tool batching, ADR-0042) don't serialize behind
each other," and a single writer task owns stdout so concurrent responses never interleave — JSON-RPC
correlates by `id`, so out-of-order responses are fine (`:359-396`).

**Calling.** `tools/call` arrives with the *advertised* name (what `tools/list` offered), which is not
always the canonical registry name. OpenCode prefixes every tool with its MCP server key (`lightbridge`),
so a bare canonical name like `read_file` is offered to the model as `lightbridge_read_file`; a handful
of tools are *already* natively `lightbridge_`-prefixed (`lightbridge_vector_semantic_search`), so the
server strips one leading `lightbridge_` before advertising (`advertised_name`, `:71-79`) — "so OpenCode
adds exactly one server prefix and every tool reads `lightbridge_<tool>`." An `advertised_to_canonical`
map built once at startup (`:87-101`) reverses this for dispatch, and — because a future rename could
introduce a genuine collision — it "fails fast at startup (the server won't run) rather than silently
misrouting dispatch in production reviews" if two canonical names would ever advertise under the same
bare name.

**Configuration is entirely environment-variable-driven**, because the process boundary means it can't
share the supervisor's control-plane client or in-memory config. `build_tools()` (`:215-272`) reads a
required set (`LCI_MCP_CP_URL`, `LCI_MCP_RUNNER_TOKEN`, `LCI_MCP_TASK_ID`, `LCI_MCP_CHECKOUT`,
`LCI_MCP_EMBED_URL`/`_KEY`/`_MODEL`) plus several optional groups: `LCI_MCP_MIN_PRIORITY` (ADR-0030
severity filtering), the SAST group (`SastConfig::from_env` + a changed-files list path, gating whether
`run_sast` is even registered), `LCI_MCP_DISCOVERED_TOOLS` (a path to a JSON file of ADR-0066
external-knowledge tool specs the *supervisor* already discovered and narrowed — "the MCP server no
longer performs its own discovery... so the offer decision and the registered set can't diverge"), and
`LCI_MCP_OFFERED_TOOLS` / `LCI_MCP_OFFERED_TOOLS_SET` (the resolved per-preset allowlist, ADR-0062/0103
— present-but-empty is a deliberately *closed* surface, distinct from absent, which means "full
registry"). This env-as-IPC pattern — resolve a decision supervisor-side, where the control-plane client
already lives, and hand the *result* across the process boundary as env or a file — repeats for every
one of these concerns (SAST, discovery, the allowlist, and the GitHub MCP credential in §6); it's the
single idiom this whole integration uses instead of re-deriving policy inside the spawned process.

## 6. The GitHub MCP server — a second, independent stdio MCP, not something `lci-review-mcp` proxies

The task asked specifically whether `lci-review-mcp` is "a dedicated MCP server exposing tools
specifically to the OpenCode agent" — yes, confirmed above — but it's worth being precise that it is
not the *only* MCP server in the picture. ADR-0105 adds a second, opt-in one: the upstream
`github-mcp-server` binary, registered as its own `"local"` entry alongside `lightbridge`, not proxied
through it:

```rust
if github_mcp_offered {
    config["mcp"]["github"] = serde_json::json!({
        "type": "local",
        "command": ["github-mcp-server", "stdio", "--read-only"],
        "enabled": true,
    });
}
```
(`services/agent-runner/src/review/opencode.rs:243-252`)

It's gated on two conditions together: the preset's `review.tools` must explicitly list a
`mcp__github__…` selector, *and* the task has a GitHub installation token
(`github_mcp_offered = tool_surface::github_mcp_explicitly_listed(review) && mcp_env.github_token.is_some()`,
`:241-242`). The `--read-only` flag is load-bearing, not incidental — the comment is explicit that
"review must never get a write-capable GitHub MCP" (`:246-249`). Its credential
(`GITHUB_PERSONAL_ACCESS_TOKEN`) rides the *whole opencode process* env rather than a server-specific
config field, because — per the code's own comment — "a `type: \"local\"` MCP server is a child of
opencode and inherits its parent's env, and this codebase's own `review.jsonc` comment confirms that's
the verified mechanism opencode-over-ACP actually honors for stdio MCP servers" (`:352-357`). This is
the one credential genuinely new to a task process — unlike the ADR-0066 customer-knowledge tools
(§5), which stay mediated through the control plane precisely *to avoid* putting a third party's
transport/credentials inside the review pod (ADR-0097's own rationale, quoted in §8).

## 7. Corrections to `system_reports/10-opencode-integration.md`

Per this project's practice, the existing report is a lead, verified against source, not a source of
truth. Three claims in it don't hold up:

### 7.1 "installed as a pre-compiled, **statically-linked** binary"

Wrong, and the Dockerfile's own comment says so directly: the OpenCode release binary is
**musl-dynamic** — `file` reports `interpreter /lib/ld-musl-x86_64.so.1` — and "needs the musl loader +
libgcc/libstdc++ and cannot run on glibc/debian" (`services/agent-runner/Dockerfile.review-opencode:9-12`).
It's the project's *own* Rust binaries (`agent-runner`, `lci-review-mcp`) that are fully static musl
(ADR-0080) — the report has attributed the project's own linkage property to the vendored binary.

### 7.2 "A plugin intercepts the `finish` tool call. If the LLM hasn't reviewed all changed files, the
plugin rejects the tool call and forces the LLM to continue" / "A plugin enforces the requirement that
high-severity findings must be explicitly verified"

Wrong mechanism. The coverage gate and refute gate are **not** enforced by a TypeScript plugin inside
OpenCode's loop for the live review path — they are the reused Rust `CoverageGate`/`RefuteGate` running
**supervisor-side**, outside OpenCode entirely, as described in §4e and ADR-0097 decision 1: "The
gate-interlock plugin... is an in-process backstop only." The actual TypeScript gate-interlock plugin
does exist and *can* throw to block a tool call (`integrations/opencode/plugins/gate-interlock/src/index.ts:40-54`),
but for review it's configured to do essentially nothing: the review host sets

```rust
("LCI_GATE_TERMINAL_TOOL".into(), "lightbridge_finish".into()),
("LCI_GATE_REQUIRED_TOOLS".into(), String::new()),
```
(`services/agent-runner/src/review/opencode.rs:335-336`)

with the comment "the supervisor-side gates are authoritative, so no hard required tool is imposed
in-process." Compare that to the plugin's *own* defaults, which the config-reference docs also describe
— `LCI_GATE_TERMINAL_TOOL` defaults to `lightbridge_submit_findings`, `LCI_GATE_REQUIRED_TOOLS` defaults
to `lightbridge_refute_finding` (`integrations/opencode/plugins/gate-interlock/src/index.ts:13-14,18-19`,
matching `deploy/config-reference.md:343-344`) — those defaults are simply never the values the review
host actually passes. The real enforcement is the Rust `ReviewDriver` re-prompting OpenCode between
`session/prompt` cycles with a nudge (§4b), not a thrown exception inside OpenCode's own loop. (The
gate-interlock plugin is still live infrastructure — it's what `open` mode or a future mode could lean
on — it's just not what's doing the work on the review path today.)

### 7.3 Minor: the report's phrase "This architecture was introduced in ADR-0097 and RFC-0009" elides
which ADR actually says what

Not wrong exactly, but worth sharpening (see §8) — ADR-0097 is specifically the *review*-mode cutover
ADR; ADR-0094 is a different, earlier ADR (the `open`-mode ACP host). The code's own module docs
sometimes cite them together loosely (`services/review-agent/src/opencode/mod.rs:1`: "RFC-0009 /
ADR-0094/0095 review cutover, slice 3") in a way that could suggest ADR-0094 is *the* review-cutover
record; it isn't. §8 untangles this.

## 8. The ADRs, untangled: RFC-0009 → ADR-0094 → ADR-0097

The module doc comments across this code (e.g. `services/agent-runner/src/review/opencode.rs:1`: "The
OpenCode review host (RFC-0009 / ADR-0094 review cutover, slice 3)") cite "ADR-0094" as shorthand for
"the review cutover," but reading the actual ADR chain shows ADR-0094 is not that document — it's one
step earlier in a three-stage lineage:

- **RFC-0009** (`docs/rfc/0009-opencode-acp-agent-host.md`) — the proposal to host the agent loop in
  OpenCode-over-ACP at all, for any mode. Its own **Phasing** section originally gated a `review`
  cutover on Phase 2, an eval harness (the then-open issue #252), with Phase 1 (`open` mode) shipping
  first as the place "production evidence accumulates." The RFC carries a visible amendment at the top:
  "⚠️ Amended 2026-07-16: the Phasing below is superseded by ADR-0097... The owner reversed Phase 2/3."
- **ADR-0094** (`docs/adr/0094-opencode-acp-open-mode-host.md`) — ships RFC-0009's Phase 1: the ACP
  host for **`open` mode only**. It is a real, resulting ADR of the RFC, but it is about `open` mode,
  not review.
- **ADR-0097** (`docs/adr/0097-review-runs-on-opencode.md`) — the actual review-cutover ADR, and the
  one the task asked me to find and summarize rather than re-quoting file 06's slice-3 framing.

ADR-0097's real rationale is a documented **reversal**, stated candidly in its own Context section: "On
2026-07-16 the owner reversed that sequencing. The eval harness does not exist and the native loop
keeps generating maintenance load; rather than block the cutover on building a harness first, the
directive was: cut the live review path to OpenCode now, and gate it on a **shadow parity run**... a
lighter, sufficient bar than a full eval harness" (`docs/adr/0097-review-runs-on-opencode.md:24-27`).
It's explicit that this is a deviation from plan, not a vindication of it: "This ADR records the
architecture that decision produced, and the fact that it is a **reversal of RFC-0009's Phase-2/3
phasing**, not a fulfilment of it" (`:29-30`).

Six "load-bearing decisions, all verified against OpenCode 1.18.3" structure the rest of the ADR
(`:60-103`) — the first two (reuse the Rust gates supervisor-side; feed them from the recorder JSONL,
not ACP `session/update`) are exactly what §4e's code implements. The third and sixth are worth
surfacing because they're easy to get backwards and genuinely surprising:

- **Built-in tools must be disabled at the config top level, never per-agent**, because "OpenCode-over-ACP
  runs its default `build` agent, not a custom `review` agent, so a per-agent `tools` block is ignored"
  (`:71-75`) — this was *verified against the real binary*, not assumed; the e2e test in
  `services/agent-runner/src/review/opencode.rs` (`mod e2e`) asserts the advertised-tools set directly.
  The same ADR later documents (2026-07-22 update, `:76-86`) that the identical mechanism silently made
  `agent.review.prompt` inert too — fixed by moving the per-task prompt to `agent.build.prompt` instead,
  which is exactly why `review.jsonc`'s own `agent` block today targets `"build"`, not `"review"`
  (`integrations/opencode/config/review.jsonc:127-138`).
- **Config isolation is a security decision, not a convenience one.** OpenCode merges config from its
  *cwd's* `opencode.json` and from `HOME`/`XDG` over `OPENCODE_CONFIG`; since the checkout is an
  untrusted PR, a malicious `opencode.json` committed to a fork could re-enable `bash`, flip every
  `permission` to `allow`, inject a command-running MCP server, or swap the model — and `opencode debug
  config` confirmed such a file "fully overrides ours" (`:94-103`). The fix is structural: opencode is
  spawned with a neutral workdir as cwd (never the checkout) and empty, freshly-created `HOME`/`XDG`
  directories, visible directly in the host's spawn sequence
  (`services/agent-runner/src/review/opencode.rs:272-280`).

ADR-0097 also settles where a *customer's own* MCP server goes, and the answer is deliberately not "add
it to OpenCode's config": "Injecting customer MCP servers into the rendered OpenCode config would (a)
run the customer's transport/credentials *inside our review Job pod* next to the eaig key, internal CA,
and git token, and (b) reopen exactly the config surface decision #6 closes" (`:154-158`). Customer tools
stay on the ADR-0066 mediated path — discovered and narrowed by the supervisor, handed to
`lci-review-mcp` as a file (§5), dispatched back through the control plane's
`/internal/tasks/{id}/knowledge/{tools,call}` — never a direct connection from inside the pod.

The ADR's own build-slices table (`:137-144`) is a useful ledger of what shipped when: slice 1 is
`lci-review-mcp` (#440); slices 2–3 are the review core (§4) plus the transport host (#442–#447); slice
4 is the shadow-parity gate (§9, #450); slice 5 is the actual dispatch cutover — swapping
`run_native_agent` for `run_opencode_agent` in `perform_review`, which file 06 §4 already quotes — merged
as #452 and live since 2026-07-17; slice 6, explicitly marked "this ADR's follow-up," is restoring the
ADR-0066 customer-MCP discovery inside `lci-review-mcp`, which slice 1 had stubbed to
`std::iter::empty()`.

Companion ADRs worth knowing exist, without re-deriving them here: **ADR-0095**
(`docs/adr/0095-opencode-plugins-recording-and-gates.md`, 141 lines) covers the recorder/gate-interlock
plugin *design* generally (not review-specific); **ADR-0099**
(`docs/adr/0099-operator-opencode-config-overlay.md`, 162 lines) is the operator-overlay mechanism §4a
implements; **ADR-0103** (`docs/adr/0103-repo-configurable-opencode-review-presets.md`) is the
preset-uniformity rule file 06 §4a already quotes; **ADR-0105**
(`docs/adr/0105-github-mcp-via-app-derived-token.md`) is the GitHub MCP decision in §6; **ADR-0106**
(`docs/adr/0106-opencode-fatal-situation-sentinel-plugin.md`) is the sentinel plugin in §9.

## 9. The other two plugins, and the host's own telemetry

`review.jsonc` loads four plugins, in this order: `recorder`, `gate-interlock`, `logger`, `sentinel`
(`integrations/opencode/config/review.jsonc:80-85`). §4d covers what the recorder produces and how it's
consumed; §7.2 covers why gate-interlock is present but inert for review. The other two:

- **`logger`** (328 lines, `integrations/opencode/plugins/logger/src/index.ts`) — the Loki-observability
  path the system report's "Logging" bullet describes reasonably accurately: it reads model
  reasoning/content/tool-args/tool-output off the plugin bus and emits them as structured stderr lines
  (captured by the pod → Loki pipeline in production), capped per-field by `LCI_LOG_REASONING_CHARS` /
  `_CONTENT_CHARS` / `_TOOL_ARGS_CHARS` / `_TOOL_OUTPUT_CHARS` (`deploy/config-reference.md:347`). The
  agent-runner's own e2e test (`agent_runner::review::opencode::e2e::drives_a_real_opencode_review_to_finished`,
  `services/agent-runner/src/review/opencode.rs:749-947`) asserts against this plugin's *real* captured
  stderr that a model's reasoning delta actually reaches an `agent.reasoning`-tagged log line on the
  live wire — not a synthetic event, a genuine wire-shape-drift guard.
- **`sentinel`** (168 lines, ADR-0106) — diagnostic-only, never throws into the loop. It distinguishes
  three fatal-shaped signals — `provider_error` (an OpenCode `session.error` bus event),
  `uncaught_exception` (a Node exception in the opencode process itself), and `exit_without_terminal`
  (the process exited without ever completing a `lightbridge_finish`/`lightbridge_abort` call) — and
  writes the *last* one observed to a marker file the host reads after shutdown
  (`services/agent-runner/src/review/opencode.rs:611-620`). The plugin's own doc comment is careful to
  say `exit_without_terminal` "is diagnostic ONLY, not inherently fatal: a normal budget-exhausted
  review ALSO exits without ever calling finish/abort" — the host only escalates it to an error when
  the review loop *also* returned a transport `Err` rather than a clean resolution
  (`services/agent-runner/src/review/opencode.rs:526-554`).

Separately from the plugins, the host fires one piece of run-start telemetry per review — the *actual*
turn-0 tool surface it offered (not what it was configured to offer), posted fire-and-forget so it never
adds a control-plane round-trip to startup (`services/agent-runner/src/review/opencode.rs:458-475`).

## 10. The shadow-parity gate (`cargo xtask shadow diff`)

ADR-0097's cutover bar — mentioned but not detailed in file 06 — is implemented as an `xtask`
subcommand:

```rust
/// OpenCode↔native review shadow parity: diff two engines' findings (RFC-0009 slice 4 gate).
Shadow {
    #[command(subcommand)]
    action: commands::shadow::Action,
},
```
(`xtask/src/main.rs:42-46`)

backed by `xtask/src/commands/shadow.rs` (247 lines). The procedure, documented in
`integrations/opencode/sim/SHADOW.md`, is an **operator runbook**, not an automated CI gate — it can't
be hermetic because it needs a real model, the real control plane, and a real PR, and model output
isn't deterministic (`SHADOW.md:5-8`). An operator runs both engines against the *same* PR with the
*same* `ReviewConfig`, exports each engine's buffered findings to JSON, and diffs them:
`cargo xtask shadow diff --native native.json --opencode opencode.json`. Findings land in one of four
buckets — `matched`, `only-native` (OpenCode missed something native caught — "**BLOCKS cutover** —
this is the regression"), `only-opencode` (new signal or noise, a human call), `severity-diverged`
(same issue, different priority) — and the diff tool "exits non-zero if `only-native` is non-empty"
(`SHADOW.md:10-17`). This is the "lighter, sufficient bar than a full eval harness" ADR-0097 chose
instead of waiting on the still-unbuilt #252 eval harness (§8).

## 11. The native path's retirement, briefly — see file 06 §4 for the rest

File 06 §4 already establishes, with its own citations, that `perform_review` in `run.rs` calls
`review::opencode::run_opencode_agent` and not `review::run_native_agent` — i.e. **OpenCode is the live
review host and the native in-process loop is the retiring/legacy path**, despite `agent-runner`'s own
module doc and `review/mod.rs`'s own module doc still describing the native loop as primary. That
finding, the exact call site, and the "both hosts drive the same underlying policy composition" nuance
are all covered there and not repeated here. What this file adds on top is the *load-bearing mechanism*
behind that cutover (§4–§6, the whole `opencode/` core and `review-mcp`) and the ADR chain that actually
authorized it (§8), which file 06 cites by name but doesn't unpack.

## Verification notes

- Confirmed the OpenCode binary is musl-*dynamic*, not static, directly from the Dockerfile's own
  comment (`services/agent-runner/Dockerfile.review-opencode:9-12`) and the runtime stage's
  `libgcc`/`libstdc++` install (`:109-110`) — this directly contradicts
  `system_reports/10-opencode-integration.md`'s "statically-linked binary" claim (§7.1).
- Confirmed the coverage/refute gates are enforced supervisor-side in Rust, not by the TypeScript
  gate-interlock plugin, by reading both sides of the wire: the plugin's own defaults
  (`integrations/opencode/plugins/gate-interlock/src/index.ts:18-19`) versus what the review host
  actually sets at spawn time (`services/agent-runner/src/review/opencode.rs:335-336`, an empty
  required-tools list) — the two disagree, and the host's values are what's live. This directly
  contradicts the system report's "Coverage Gate"/"Refute Pass" plugin-enforcement claims (§7.2).
- Confirmed ACP is "Agent Client Protocol" (not "Agent Control Protocol" as the system report has it)
  from the crate's own module doc (`services/acp-host/src/lib.rs:1`), and verified the transport is
  real newline-delimited JSON-RPC with request/response correlation by reading `AcpClient::request`
  and `reader_loop` directly (`services/acp-host/src/lib.rs:146-168,246-308`), not inferring it from
  the name.
- Confirmed `lci-review-mcp` reuses `lci_review_agent::tools::Tools` verbatim rather than
  reimplementing the tool schemas, from both the Cargo dependency graph
  (`services/review-mcp/Cargo.toml:18-25`) and the module doc's explicit claim
  (`services/review-mcp/src/main.rs:7-10`), cross-checked against the actual `tools/list`/`tools/call`
  handlers (`:304-346`) which call `tools.specs()`/`tools.dispatch()` directly.
- Confirmed `session/new.mcpServers` is NOT how the mediated review MCP is wired, despite being passed
  through the ACP client (`services/acp-host/src/lib.rs:184-196`) — the actual empty-array call site
  (`services/agent-runner/src/review/opencode.rs:500-507`) and ADR-0097 decision 4 both state stdio MCP
  goes through the rendered config's `mcp` block instead.
- Traced the RFC-0009 → ADR-0094 → ADR-0097 chain by reading all three documents directly rather than
  trusting the code comments' loose "RFC-0009 / ADR-0094 review cutover" shorthand, and found the RFC's
  own amendment notice confirming ADR-0097 is a stated *reversal* of the RFC's original phasing, not a
  fulfilment of it (`docs/rfc/0009-opencode-acp-agent-host.md:10-13,199-204`).
