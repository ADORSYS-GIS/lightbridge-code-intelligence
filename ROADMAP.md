# Roadmap

A maintainer's at-a-glance view of what has shipped, what is in flight, and what is planned for
**Lightbridge Code Intelligence**. This is a living summary, not the source of truth: architectural
decisions live in [`docs/adr/`](docs/adr/) (index in [`docs/INDEX.md`](docs/INDEX.md)) and tracked work
lives in the GitHub Epics / User Stories / Dev Tickets. When those and this file disagree, they win —
open a PR to fix this file.

> **Keeping this current is part of "done."** When a PR meaningfully ships, unblocks, or retires an item
> here, update its status in the **same PR** (see [AGENTS.md](AGENTS.md)).

_Last updated: 2026-07-17._

## Recently shipped

- **Review runs on OpenCode** — the live code-review path is hosted on OpenCode-over-ACP instead of the
  in-house agent loop; the tuned review gates and tools are reused, not reimplemented.
  ([ADR-0097](docs/adr/0097-review-runs-on-opencode.md), live in prod since 2026-07-17)
- **Customer / external MCP tools in review** — a repo owner can attach their own MCP server (declared in
  the control-plane config); the control plane mediates it, so the runner never talks to it directly.
  ([ADR-0066](docs/adr/0066-deep-tier-external-knowledge-tools.md), #455)
- **Operator OpenCode config overlay** — the review OpenCode config is now a readable, checked-in file
  that a trusted operator can override (custom sub-agents, models, per-agent access) via ai-helm-values.
  ([ADR-0099](docs/adr/0099-operator-opencode-config-overlay.md), #464)
- **SAST (`run_sast`) ported to the OpenCode review path** — opengrep-backed findings are available to the
  OpenCode reviewer, closing the parity gap with the old native path. ([ADR-0073](docs/adr/0073-sast-as-agent-tool.md), #456)
- **Restate egress removed** — forge egress runs on the single-writer reconciler drain again after the
  Restate pilot proved unnecessary at current scale. ([ADR-0093](docs/adr/0093-restate-egress-pilot-no-go.md))

## In progress / near-term follow-ups

- **Allowlist `run_sast` in ai-helm-values** — the single remaining blocker to SAST going live on the
  reviewer (the code is on both paths; the tool is just not offered until the values allowlist enables it).
- **Expose `config.review.opencode` in the ai-helm chart** — companion to the operator overlay above; the
  runner accepts the field, but no operator can set it until the chart surfaces it.
- **Remove the dead native review path** — now unblocked (SAST is ported); delete `run_native_agent` and
  its native-only modules.
- **A2A per-finding review streaming** — stream findings as they are confirmed at finalize.
  ([ADR-0098](docs/adr/), #458 — open)
- **OpenCode review observability** — capture the model's content and reasoning and tool I/O faithfully in
  the persisted transcript (part of the review-quality epic below).

## Planned — open epics

- **Retire `apps/web`** (Epic #241) — the web console is down to its last function (the repo approval
  gate); the `lci` admin TUI ([ADR-0063](docs/adr/0063-cli-only-repository-approval.md)) and Grafana absorb the rest,
  then `apps/web` is deleted. Also carries per-identity model selection + ACL (ADR-0038 upgrade).
- **Review quality & reliability** (Epic #252) — the durable quality track: a fast-tier eval harness to
  catch calibration regressions (not started), the #285 severity-stability watch, and the observability
  work above.

## Exploring / proposed

- **Graph-embedding retrieval** — semantic + structural search over the code graph.
  ([ADR-0089](docs/adr/0089-embeddings-on-the-code-graph.md) / [ADR-0090](docs/adr/0090-hybrid-retrieval-tools.md), Proposed)
- **Incremental indexing — overlay model** (#244) — index only what changed on a PR.
- **End-to-end tracing (OpenTelemetry)** (#419 / #431) — deployed but currently inert (endpoint unset).
- **GitLab support** (#414) — config-only to activate.
- **Step reproducibility** (#430).

## Where the detail lives

- **Decisions & architecture:** [`docs/adr/`](docs/adr/) · [`docs/INDEX.md`](docs/INDEX.md)
- **How to contribute:** [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Tracked work:** GitHub Epics / User Stories / Dev Tickets (use the issue forms)
