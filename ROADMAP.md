# Roadmap

A maintainer's at-a-glance view of what has shipped, what is in flight, and what is planned for
**Lightbridge Code Intelligence**. This is a living summary, not the source of truth: architectural
decisions live in [`docs/adr/`](docs/adr/) (index in [`docs/INDEX.md`](docs/INDEX.md)) and tracked work
lives in the GitHub Epics / User Stories / Dev Tickets. When those and this file disagree, they win —
open a PR to fix this file.

> **Keeping this current is part of "done."** When a PR meaningfully ships, unblocks, or retires an item
> here, update its status in the **same PR** (see [AGENTS.md](AGENTS.md)).

_Last updated: 2026-07-26._

## Recently shipped

- **Review runs on OpenCode** — the live code-review path is hosted on OpenCode-over-ACP instead of the
  in-house agent loop; the tuned review gates and tools are reused, not reimplemented.
  ([ADR-0097](docs/adr/0097-review-runs-on-opencode.md), live in prod since 2026-07-17)
- **OpenCode review observability — logs-only** — the DB run transcript is retired and Loki is the single
  observability surface; the logger plugin emits leveled per-turn `agent.reasoning`/`agent.content`/
  `agent.part.unknown`/`tool.done`/`tool.start`/`tool.output` lines, with the `agent.*` narrative signals
  raised to info. ([ADR-0100](docs/adr/0100-retire-db-transcript-logs-as-observability.md), Epic #459 —
  #461 tore out the transcript subsystem, #462 landed the leveled lines, #463 hardened the capture, #474
  raised `agent.*` to info)
- **Deep-tier reasoning-effort parity** — deep reviews send `reasoning_effort: "high"` again; the OpenCode
  cutover had silently dropped it (rendering only a `reasoning` bool). Fixed by threading `review.extra`
  into the OpenCode reviewer model options. ([ADR-0069](docs/adr/0069-review-tier-minimum-model-capability.md), #475)
- **CI on ARC pod runners** — the `vymalo-vps` runner is now ARC pod runners (`ghcr.io/vymalo/arc-runners`,
  rootless podman, **no dind**) instead of a Docker VM; dind is now a rejected fix for any future
  Docker-needing CI job on this fleet. ([ADR-0101](docs/adr/0101-ci-arc-pod-runners-no-dind.md), #476)
- **Run logs: embedded Grafana/Loki panel replaces the native k8s log stream** — the run-detail page's
  "Logs" card no longer calls the Kubernetes API directly (`@kubernetes/client-node` removed); it embeds
  the generated `task-runs` dashboard's Loki panel via a `d-solo` iframe, gated on the new
  `NEXT_PUBLIC_GRAFANA_URL` env var with a graceful `kubectl logs` fallback when unset.
  ([ADR-0102](docs/adr/0102-grafana-loki-embedded-run-logs.md), #479, panel-id pinned for a
  deterministic embed URL in #480 — Grafana-side embed prerequisites still open, see the ADR)
- **Token/model dashboard panels rebuilt on Loki** — with the DB run transcript dropped
  ([ADR-0100](docs/adr/0100-retire-db-transcript-logs-as-observability.md)), the `review-cost` panel
  is re-sourced to the `gen_ai_request_model` Loki label and the `review-quality`/`overview` token/model
  panels are rebuilt on the AI-Gateway (eaig) Loki billing stream instead of the removed table
  (reasoning-token split omitted — the gateway stream carries only a single total). (#472, #478)
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
  ([ADR-0098](https://github.com/vymalo/lightbridge-code-intelligence/pull/458), #458 — open)

## Planned — open epics

- **Repo-configurable OpenCode review presets** (Epic #491) — replaces the fixed fast/deep tier model
  with per-repo preset selection (`fast`/`deep`/`ultra` defaults, uniform tools/prompt), a full
  OpenCode fs-tool suite, GitHub MCP access via App-derived tokens, and an OpenCode fatal-session
  sentinel plugin. Also carries per-identity model selection + ACL (ADR-0038 upgrade, moved here from
  Epic #241). ([ADR-0103](docs/adr/0103-repo-configurable-opencode-review-presets.md)/
  [0104](docs/adr/0104-full-opencode-fs-tool-suite.md)/[0105](docs/adr/0105-github-mcp-via-app-derived-token.md)/
  [0106](docs/adr/0106-opencode-fatal-situation-sentinel-plugin.md))
- **Control-plane v2 — three planes + the agent execution plane** (Epic #353) — the `StepRuntime`
  seam is now generalized (`Passthrough`-backed, zero behavior change) to webhook ingress, the
  dispatcher, the reconciler, and A2A's task lifecycle (#502); `CodePlatform` is fully wired into
  the webhook router (outbox/reconciler were already wired) and activated for GitHub/GitLab (#504),
  plus Bitbucket lands as a third implementation on the existing `/webhook` route (#505); error
  layering across control-plane/agent-* crates now uses typed `thiserror` enums at internal
  boundaries (#503, scoped to every hand-rolled error type + stringly-typed `Result<T, String>` site,
  not a full sweep of every `anyhow` call site — see the PR for the explicit scope note).
  ([RFC-0007](docs/rfc/0007-control-plane-v2-planes.md), [ADR-0107](docs/adr/0107-state-machine-backbone-all-backend-roles.md)/
  [0108](docs/adr/0108-codeplatform-github-gitlab-bitbucket.md)). Remaining: promoting
  `CheckpointRuntime` to the production default for any role stays gated on #363's open P1s; a
  per-role durable store (beyond `Passthrough`) is follow-up work, not done here.
- **Retire `apps/web`** (Epic #241) — the web console is down to its last function (the repo approval
  gate); the `lci` admin TUI ([ADR-0063](docs/adr/0063-cli-only-repository-approval.md)) and Grafana absorb the rest,
  then `apps/web` is deleted.
- **Review quality & reliability** (Epic #252) — the durable quality track: a fast-tier eval harness to
  catch calibration regressions (not started, being reframed around presets — see #491), the #285
  severity-stability watch, and the observability work above.

## Where the detail lives

- **Decisions & architecture:** [`docs/adr/`](docs/adr/) · [`docs/INDEX.md`](docs/INDEX.md)
- **How to contribute:** [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Tracked work:** GitHub Epics / User Stories / Dev Tickets (use the issue forms)
