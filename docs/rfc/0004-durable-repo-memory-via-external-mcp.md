# RFC-0004: Durable repo memory via an external, consolidating MCP memory service

- **Status:** Proposed
- **Author(s):** Stephane Segning (@stephane-segning)
- **Date:** 2026-07-07
- **Resulting ADRs:** (filled in on acceptance — expected to supersede ADR-0044's deferred "M2" direction)

## Summary

Give the review agent **durable, per-repo memory** — the conventions and learnings that make later
reviews of the same repo better — by connecting it to a **separate, self-hosted memory service** that
already provides memory **read + write tools** and **scheduled consolidation** ("dreaming"). Lightbridge
consumes it **purely through the existing [ADR-0066](../adr/0066-deep-tier-external-knowledge-tools.md)
knowledge-tools path**, so the review system does not change — the integration is a config entry plus a
per-tier allowlist selector. The existing reaction-derived memory ([ADR-0044](../adr/0044-feedback-memory-m1.md),
"M1") stays in force as the read-only floor.

## Motivation

Today the agent has exactly one form of cross-run memory: M1 ([ADR-0044](../adr/0044-feedback-memory-m1.md))
injects findings a human 👎'd so they aren't re-raised. It is essential but thin — **rejections only**,
**derived** (never written), matched by `(file, line)`. It cannot carry positive, durable repo knowledge:

- "generated code under `gen/` is out of scope here"
- "this team keeps Dart 3 null-aware-element syntax `?expr` — don't flag it"
- "prefer the `Result` alias from `errors.rs`"

Patterns differ per repo, so this knowledge has to be **learned per repo, curated, and human-gated** —
that is what lets the agent review *better* over time rather than relearning the same quirks every run.

Building that inside Lightbridge would mean growing a first-party store, a `remember`/`recall` tool, and
a **curation dashboard we don't have** (and shouldn't add — `apps/web` is sunsetting, epic
[#241](https://github.com/adorsys-gis/lightbridge-code-intelligence/issues/241)). Hard-coding a memory
tool also duplicates a **generic, reusable capability** that a memory MCP already provides. We already
plan to run a memory system that does exactly this — memory tools plus scheduled consolidation — so the
right move is to **integrate**, not rebuild.

Source of truth: epic [#252](https://github.com/adorsys-gis/lightbridge-code-intelligence/issues/252)
(Review quality & reliability).

## Guide-level explanation

A separate **memory service** runs on our cluster. It owns everything memory-specific: the store, a
**curation UI**, and a **scheduled consolidation loop** that merges duplicates, resolves contradictions,
and prunes stale notes between runs (the "dreaming" pattern). It exposes an **MCP** surface over
streamable-HTTP with (at least) two tools:

- `recall(repo, query, …)` → curated memories relevant to what the agent is reviewing.
- `remember(repo, note, …)` → record a candidate learning. Writes land **unverified** and are **not
  recalled** until a human curates them (or consolidation promotes them). This is the poisoning gate.

On the Lightbridge side, **nothing in the review system changes**. The control plane is already an MCP
client for external-knowledge tools ([ADR-0066](../adr/0066-deep-tier-external-knowledge-tools.md)). We:

1. Add the service to `knowledge_tools.mcp_servers` (ai-helm-values, GitOps): `{name: "memory", url:
   "http://memory.<ns>.svc.cluster.local:8080/mcp"}`.
2. Allowlist its tools per tier in `review.<tier>.tools`: `mcp__memory__recall` on both tiers;
   `mcp__memory__remember` gated to the **deep** tier (see Unresolved).

The agent then discovers `mcp__memory__recall` / `mcp__memory__remember` like any other knowledge tool,
and the operator prompt points it at them. Recalled text is framed as **untrusted, size-capped context**
by the control plane — same treatment as every other knowledge-tool result.

M1 is untouched and keeps running as the reaction-derived read-only floor. **Curation is decoupled**: the
memory service is curated through its *own* dashboard; the 👍/👎 signal on findings stays what it is
today. Bridging reactions into the service's trust state is a possible later addition, explicitly out of
scope here because it would add a webhook to Lightbridge.

## Reference-level explanation

### Integration contract (Lightbridge side — verified, config-only)

The control plane's MCP client is the official **rmcp v2 SDK over `StreamableHttpClientTransport`**
(`services/control-plane/src/mcp_client.rs`). Streamable-HTTP is the only transport it speaks, which is
exactly what the memory service must expose. Concretely the memory service must satisfy:

- **Streamable-HTTP MCP** at a single endpoint, e.g. `POST /mcp` (JSON-RPC 2.0, rmcp-compatible).
- **Server name has no `__`** — the routing prefix is `mcp__<name>__<tool>`, split on `__`; a name with
  `__` fails config load (`McpServerConfig` deserializer in `services/control-plane/src/config.rs`).
- **`recall` returns within the 20s per-call timeout** (`KNOWLEDGE_TOOL_TIMEOUT`,
  `services/control-plane/src/http/internal.rs`). Consolidation/dreaming runs on the service's **own**
  schedule — never inside a tool call.
- **In-cluster reachability, no connection auth.** The client sends no bearer/OAuth; `McpServerConfig` is
  just `{name, url}`. The service must therefore be network-isolated (in-cluster Service, not publicly
  exposed) and hold its own storage credentials.

Config shape (ai-helm-values, mirrors `brave-search` / `context7`). These are **two separate service
configs** — the control plane and the agent runner each load their own file, and both use
`deny_unknown_fields`, so they must **not** be merged into one document:

Control-plane config (`FileConfig`, `services/control-plane/src/config.rs`) — registers the server:

```json
{
  "knowledge_tools": {
    "mcp_servers": [
      { "name": "memory", "url": "http://memory.converse.svc.cluster.local:8080/mcp" }
    ]
  }
}
```

Agent-runner review config (`ReviewFile.<tier>.tools`, `services/agent-runner/src/bootstrap/config.rs`)
— allowlists the tools per tier (`recall` both tiers; `remember` deep-tier only):

```json
{
  "review": {
    "fast": { "tools": ["...", "mcp__memory__recall"] },
    "deep": { "tools": ["...", "mcp__memory__recall", "mcp__memory__remember"] }
  }
}
```

Dispatch path (unchanged, ADR-0066): agent → `POST /internal/tasks/{id}/knowledge/call` with
`{tool: "mcp__memory__recall", arguments: {...}}` → control plane routes to the `memory` server → text
result, framed and capped, back to the agent. Discovery via `GET /internal/tasks/{id}/knowledge/tools`.

### Memory service (contract only — its own repo/design/ADRs, out of scope here)

The service owns the parts that make agent writes safe and memory durable:

- **Trust gate:** `remember` writes are `unverified` and are **never** returned by `recall` until a human
  curates them (or consolidation promotes them). This relocates ADR-0044's poisoning concern out of
  Lightbridge and into the service, where the dashboard and consolidation live.
- **Consolidation ("dreaming"):** scheduled, off the review hot path — merge, de-dupe, resolve
  contradictions, expire stale notes; keeps the recalled set small enough for the prompt budget.
- **Curation UI:** operators approve/edit/expire entries; the dashboard Lightbridge deliberately doesn't grow.
- **Scoping:** per-repo at minimum; org/global scoping is the service's call.

### Budget interaction

Recalled memory shares the injected-context window under
[ADR-0070](../adr/0070-window-proportional-prompt-budgets.md). `recall` must return a **ranked, capped**
set; consolidation keeps total memory small so recall stays cheap and within the 20s ceiling.

## Drawbacks

- **A new service to operate** — its own pod, storage, dashboard, on-call surface, and upgrade cadence.
- **No auth on the in-cluster hop**, so **network isolation is load-bearing** — a misconfigured Service
  exposure leaks/writes memory. (ADR-0066 accepted this for `brave-search`/`context7`; same posture.)
- **Recall sits in the review turn budget** — it must stay under 20s and within the ADR-0070 window;
  slow or bloated recall directly taxes reviews.
- **Agent write re-opens the scratchpad-loop temptation** that dogfood run `7c15f9bb` showed. It is now
  bounded by the service's `unverified` gate rather than by the *absence* of a tool — so the guardrails
  (write only sensible notes, rate/size caps, deep-tier only) matter.
- **Learning depends on the service getting source material** — human curation alone is sparse;
  consolidation is only as good as what it can read (see Unresolved).

## Alternatives

- **First-party store + `remember`/`recall` tool inside Lightbridge** (ADR-0044's deferred "M2").
  Rejected: duplicates a generic MCP capability and forces a curation dashboard into a sunsetting surface.
- **Recall-only, no agent write.** Rejected by decision: without a write path the memory can only be
  populated by manual curation, which won't keep up — the agent's in-context learnings are the richest
  signal and must be capturable. Write + recall it is.
- **Bridge Lightbridge 👍/👎 reactions into the service's trust state.** Deferred: powerful, but adds a
  webhook/emitter to Lightbridge, breaking the "review system unchanged" property. Additive later.
- **Third-party hosted memory SaaS.** Rejected: repo context is customer code; self-hosting keeps it
  first-party. (Self-hosted-but-external already satisfies sovereignty.)
- **Do nothing.** The agent keeps relearning per-repo conventions every run; M1 covers only rejections.

## Unresolved questions

- **Which concrete memory system?** The service may be an existing/planned system outside this repo. This
  RFC fixes the *integration contract*, not the implementation.
- **How does consolidation get source material?** Manual curation is sparse. Options: the agent's
  `remember` writes are the primary feed; and/or the service pulls a read-only export/replica of review
  transcripts/findings (a pull *by* the service — still no Lightbridge code change). To be resolved with
  the service's design.
- **Write guardrails:** deep-tier-only vs both tiers; rate/size caps; dedupe-on-write; whether `remember`
  costs a meaningful slice of the turn budget in practice (measure in dogfood).
- **Auth if the hop ever leaves the cluster** — out of scope while in-cluster; would need ADR-0066 to
  grow auth first.
- **Out of scope:** the memory service's internal schema, dashboard, and consolidation algorithm; the
  reaction→trust bridge; any change to M1.
