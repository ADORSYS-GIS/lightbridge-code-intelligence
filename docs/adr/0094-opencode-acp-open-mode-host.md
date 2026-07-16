# ADR-0094: OpenCode-over-ACP hosts the `open`-mode agent loop

- **Status:** Proposed
- **Date:** 2026-07-16
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0088](0088-open-mode-autonomous-ticket-agent.md) recorded the `open` mode — the autonomous
ticket→PR agent — as a shape: the ADR-0088 sandbox spec, the credential-light `propose_pr`
boundary, and the mode×host rule (`open → run-once, always`) are all decided, but the mode has
never been activated (gate #365). Activating it on the native loop means building, in Rust, the
pieces a coding agent needs and the review loop never did: file editing, sandboxed command
execution, session/task decomposition, and subagent orchestration.

[RFC-0009](../rfc/0009-opencode-acp-agent-host.md) proposes hosting agent loops in **OpenCode**
driven over **ACP**. This ADR records the first concrete decision under that RFC: **which host
runs the `open` mode?**

## Decision Drivers

- **`open` needs exactly what OpenCode natively is:** edit + bash + sessions + subagents + MCP
  client. Building those in the native loop is re-implementation; in OpenCode they are the product.
- **No quality baseline to protect.** `review` has golden parity and a reputation; `open` is
  greenfield. It is the cheapest possible surface on which to accumulate production evidence for
  the RFC-0009 direction.
- **The ADR-0088 sandbox and trust boundary must survive unchanged** — the host swap must not move
  a single credential or widen the pod's reach.
- **The RFC-0009 non-negotiables:** full-fidelity recording and enforced (not honor-system) gates,
  via the [ADR-0095](0095-opencode-plugins-recording-and-gates.md) plugins.

## Considered Options

- **Option A — OpenCode subprocess over ACP inside the ADR-0088 sandbox** (this ADR).
- **Option B — extend the native Rust loop** with edit/exec tools and subagent support.
- **Option C — build `open` on Rig** ([ADR-0075](0075-rig-for-new-agent-surfaces.md)'s "new agent
  surfaces" clause).

## Decision Outcome

Chosen option: **Option A**. The agent-plane, launched with `mode=open`, becomes a *supervisor*:
it prepares the checkout, renders the per-task OpenCode config from
`integrations/opencode/config/`, spawns `opencode acp` **inside the same sandbox pod**, drives the
session over ACP (prompt, updates, permission policy, budgets), and reports lifecycle to the
control plane exactly as every mode does today.

### Embedded subprocess vs. sidecar — how the supervisor hosts OpenCode

`opencode acp` supports two transports (verified 2026-07-16 against the v1.18.2 binary): the default
**stdio** JSON-RPC (the editor-embed model) and a **network** mode (`--port` / `--hostname`, with
mDNS/CORS options). So both an embedded subprocess and a separate sidecar container are technically
possible. We choose **embedded — the supervisor spawns `opencode acp` as a child process and drives
it over stdio, in the one run-once pod:**

- **Lifecycle decides it.** OpenCode here is per-task, ephemeral, and 1:1 with the supervisor —
  spawned at task start, dead at task end. That is a subprocess, not a sidecar; sidecars earn their
  complexity only for concerns with an *independent* lifecycle (a shared proxy, a log shipper),
  which this is not.
- **Less attack surface in the ADR-0088 sandbox.** A stdio pipe opens no socket; `--port` would open
  a listening socket inside the pod and pull in mDNS/CORS/`0.0.0.0` knobs that all become things to
  lock down. Fewer network surfaces is strictly better in the pod that runs untrusted code.
- **Shared working tree for free** (one container, one filesystem — OpenCode edits and builds the
  checkout the supervisor prepared and reads back) and **clean reaping** (the supervisor already
  owns the wall-clock/turn budget; a process-group kill on timeout is standard).

**Deferred: the `--port` sidecar as a defense-in-depth hardening.** There is one real pro-sidecar
argument — *credential isolation*. Today OpenCode holds the runner token itself (it calls the
mediated MCP as `type: remote` with `Bearer {LCI_RUNNER_TOKEN}`), so attacker-controlled `bash`
code is co-resident with the token. The hardening is to run OpenCode in its own tighter, **no-token**
container and have it call a **localhost MCP proxy** the supervisor runs, which injects the token
when forwarding to the control plane; `--port 127.0.0.1` is how the supervisor would then reach it.
This is **deferred, not chosen**, because (a) the risk is already bounded — the token is task-scoped
([ADR-0092](0092-per-task-runner-tokens.md)) and egress is allowlisted (ADR-0088), so a stolen token
can only do what the task could, in its window; and (b) the isolation is bought by the **MCP proxy**,
which can run embedded too — the container split is not what earns it. Revisit only if the threat
model for prompt-injected code execution reaching the token justifies an in-pod boundary on top of
the pod boundary. If ever adopted, `--port` must bind `127.0.0.1` only, with mDNS and CORS off.

Invariants carried over verbatim from ADR-0088 — the host swap changes none of them:

- **Sandbox spec unchanged:** non-root, seccomp, read-only root, one writable `emptyDir`,
  egress-allowlist with default-deny cluster-internal, ephemeral, bounded. OpenCode and its Bun
  runtime live inside that box; OpenCode's own `bash` tool executes under the same containment
  that `run_command` would have.
- **Credential-light:** the pod holds the LLM-gateway key and the task-scoped runner token
  ([ADR-0092](0092-per-task-runner-tokens.md)) — nothing else. `propose_pr` remains a mediated MCP
  tool that hands the patch to the egress plane; OpenCode's config **denies** every built-in
  network/write path that could bypass it (`webfetch: deny`; git push impossible — no credential
  and no allowlisted authenticated remote).
- **Human PR gate + AI-usage declaration:** unchanged (ADR-0088 §Governance).
- **Durability posture:** **restart-on-failure** — no step replay
  ([RFC-0009](../rfc/0009-opencode-acp-agent-host.md) drops it as unexercised;
  [ADR-0087](0087-durable-replay-checkpoint-runtime.md) is *not* extended to OpenCode-hosted
  modes). The egress-plane dedup key on `propose_pr` `(task_id, run_epoch)` is what makes a
  restarted task safe, and it predates replay.

Explicit scope limit: **`review` and `index` stay on the native loop.** Their cutover is a future
ADR gated on the RFC-0009 Phase-2 eval harness — this ADR decides `open` only.

### Consequences

- **Good:** `open` activation stops being blocked on building edit/exec/subagent plumbing in Rust;
  the engineering surface shrinks to a supervisor, a config, and two plugins.
- **Good:** production evidence for the RFC-0009 direction accrues on a surface where a bad day
  costs a throwaway pod and a rejected PR, not review reputation.
- **Good:** subagents and the TypeScript plugin ecosystem (`vymalo/opencode-oauth2` lineage) come
  for free. The subagent structure is **capability tiers, not role-play**: one primary owns every
  write and both terminal tools (`propose_pr`/`abort`), and subagents are least-privilege read-only
  helpers for context isolation — starting with a single `explore` (customizing OpenCode's built-in)
  and adding more (e.g. a bash-only `verify` test-runner) on demonstrated need, not speculatively.
  The terminal path stays primary-only by construction so the gate-interlock and recorder
  ([ADR-0095](0095-opencode-plugins-recording-and-gates.md)) can key off it — enforced via the
  per-agent `tools` map, since a subagent's tool calls are unrecorded until RFC-0009 probe item (d)
  proves otherwise.
- **Bad / accepted:** Bun in the `open` image; OpenCode version pinning + probe re-run per upgrade
  becomes an operational chore; a loop bug upstream is a wait-or-plugin, not a same-day Rust fix.
- **Bad / accepted:** a long `open` run that crashes re-reasons from zero (restart-on-failure).
  Accepted for Phase 1; the first lever if it hurts is OpenCode session persistence, not replay.
- **Neutral:** ADR-0088's `apply_patch`/`run_command` tool designs are superseded *as
  implementations* by OpenCode's `edit`/`bash` under the same containment; their *constraints*
  (workdir-only writes, canonicalized paths, budget caps) transfer as config + supervisor policy.

## Pros and Cons of the Options

### Option A — OpenCode-over-ACP in the sandbox (chosen)

- Good: the needed capabilities are OpenCode's native feature set; zero loop code to write.
- Good: trust boundary provably unchanged (same pod spec, same credentials, same mediated egress).
- Bad: new supervisor code (ACP client) + the RFC-0009 drawbacks (Bun, churn, dependency latency).

### Option B — extend the native Rust loop

- Good: full ownership, no new runtime in the image, replay theoretically available.
- Bad: months of re-implementation (edit/exec tools, sessions, subagents) to reach OpenCode's
  baseline, all of it new maintenance of the #411 class — and replay, the one exclusive advantage,
  is exactly what RFC-0009 established we do not use.

### Option C — Rig

- Good: Rust-native, ADR-0075 already sanctions it for new surfaces.
- Bad: Rig is a provider library, not an agent host — the loop, subagents, edit/exec tools, and
  MCP client would still be ours to build (it solves the smallest piece of Option B's bill); and
  the ADR-0075 fidelity probe has still never passed on the eaig path.

## More Information

- [RFC-0009](../rfc/0009-opencode-acp-agent-host.md) — the program this ADR executes Phase 1 of;
  probe checklist (a)–(f) is its acceptance gate.
- [ADR-0095](0095-opencode-plugins-recording-and-gates.md) — the recorder + gate-interlock plugins
  this host requires.
- [ADR-0088](0088-open-mode-autonomous-ticket-agent.md) — the sandbox spec, trust boundary, and
  governance this ADR inherits unchanged.
- [ADR-0026](0026-native-review-agent.md) / ADR-0021 — the prior OpenCode arc; the structural
  answers to its objections are in RFC-0009 §Motivation.
- PoC: `integrations/opencode/` (config, plugins, probe).
