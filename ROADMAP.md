# Roadmap

A maintainer's at-a-glance view of what has shipped, what is in flight, and what is planned for
**Lightbridge Code Intelligence**. This is a living summary, not the source of truth: architectural
decisions live in [`docs/adr/`](docs/adr/) (index in [`docs/INDEX.md`](docs/INDEX.md)) and tracked work
lives in the GitHub Epics / User Stories / Dev Tickets. When those and this file disagree, they win —
open a PR to fix this file.

> **Keeping this current is part of "done."** When a PR meaningfully ships, unblocks, or retires an item
> here, update its status in the **same PR** (see [AGENTS.md](AGENTS.md)).

_Last updated: 2026-08-08._

## Recently shipped

- **One service, one domain, path-routed** (Epic #492) — `/a2a`, `/mcp` and `/api/v2` (including
  `/api/v2/webhook/{github,gitlab,bitbucket}`) are now served under
  `code-intelligence-api.ai.camer.digital`, replacing a flat unversioned route tree, a legacy
  `/github/webhook` alias, and the since-abandoned plan to give A2A its own separate domain. Four
  slices, all live in prod:
  - **`/api/v2` versioning** — every route moved under the prefix and the legacy `/github/webhook`
    alias removed in the same change (hard cutover, no dual-path window).
    ([ADR-0109](docs/adr/0109-api-v2-route-versioning.md), #506/#534)
  - **Per-forge webhook paths** — `/api/v2/webhook/{github,gitlab,bitbucket}`, plus GitLab
    `installation_id` validation. ([ADR-0110](docs/adr/0110-path-scoped-webhook-routes.md), #507/#547)
  - **`/mcp` exposed externally** — the control-plane's `mcp` role serves its tool surface over MCP
    Streamable HTTP for third-party clients (Claude Desktop, Cursor), OIDC-gated end-to-end: the auth
    layer wraps the whole transport and the role refuses to start without `OIDC_ISSUER`. Five tools,
    each with a declared `outputSchema`; `start_review` additionally requires a dedicated
    `review:trigger` permission and an atomic per-identity quota. Clients discover the realm via
    RFC 9728 protected-resource metadata at `/mcp/.well-known/oauth-protected-resource`, advertised
    from the 401 `WWW-Authenticate` challenge — MCP has no standard in-client OAuth flow, so without
    that a 401 is a dead end. (#508/#591)
  - **`/a2a` on the shared host** — A2A moved onto the unified domain, dropping the separate-domain
    plan from the closed #295. Note the topology is one control-plane *image*, role-selected into
    separate Deployments (`serve`/`a2a`/`mcp`) that Traefik path-routes behind one host — not a single
    process serving all three. (#509)

  Ingress/DNS/TLS for the domain landed in the companion repos (ADORSYS-GIS/ai-helm#922,
  ADORSYS-GIS/ai-helm-values#185 and #219; story #510). Still open: the `mcp` role sets
  `disable_allowed_hosts()` on its `0.0.0.0` listener with no NetworkPolicy restricting ingress to
  Traefik — the same posture `a2a` already has, worth closing before these surfaces carry heavy
  external traffic.

- **`lci-codegraph` extracted to its own repository** — the structural-graph engine now lives at
  [vymalo/lci-codegraph](https://github.com/vymalo/lci-codegraph) instead of `services/codegraph/`, and
  `agent-runner` consumes it as a git dependency pinned by exact commit (a plain crates.io version
  once it is published). The design is unchanged ([ADR-0086](docs/adr/0086-in-house-code-graph-crate.md)
  amended, not superseded); the move bought it a standalone test suite — 182 tests at 93.8% line
  coverage, including Docker-backed suites that round-trip the emitted graph through a real Neo4j and
  build the crate under both glibc and musl. Extraction surfaced three latent defects, **all now
  fixed upstream and consumed here**: a `lopdf` stack-overflow advisory
  ([RUSTSEC-2026-0187](https://rustsec.org/advisories/RUSTSEC-2026-0187) — an abort, so uncatchable by
  the crate's `catch_unwind`), plus two behaviour bugs
  ([lci-codegraph#1](https://github.com/vymalo/lci-codegraph/issues/1) — Rust trait methods with no
  default body were extracted by nothing, so trait interfaces contributed zero symbols to structural
  *or* semantic search; [lci-codegraph#2](https://github.com/vymalo/lci-codegraph/issues/2) — the
  binary/NUL guard was bypassed on the graph path, the one path production runs, letting raw NUL bytes
  into chunk content that PostgreSQL's `text` type rejects outright). Adversarial review of that fix
  then found the same declaration-vs-implementation ambiguity in the Java/TypeScript tags path
  ([lci-codegraph#5](https://github.com/vymalo/lci-codegraph/issues/5)) — also fixed and consumed.

- **Call-graph recall on Java, TypeScript and Python roughly doubled** — an instance call through a
  variable receiver (`accountService.findById(id)`, the most common call shape in idiomatic code)
  resolved to **nothing** on every tags-driven language. The qualifier was captured as the receiver
  *variable* name and compared textually against the callable's declaring *type*, so the resolver's
  single-candidate branch rejected the one correct answer — not the documented "ambiguous, so drop"
  policy, but a correct candidate being actively discarded. Measured on pinned real-world
  repositories: JSON-java 700 → 1014 `calls` edges (+45%), itsdangerous 68 → 77; Rust unaffected
  (different code path, already correct). ([lci-codegraph#8](https://github.com/vymalo/lci-codegraph/issues/8))

  Found independently and from opposite directions by an adversarial review and a Spring-support
  design spike. The reason it survived so long is worth recording: every call site in the upstream
  fixtures was type-qualified, `this`-qualified, or bare — not one was an instance call through a
  variable — and the real-world container test asserted *at least one* `calls` edge, which cannot
  distinguish "resolved most calls" from "resolved the easy third".

- **Per-repo review settings + review on new commits** (Epic #566) — check-run reporting, the
  automatic on-PR-open review, and a new review-on-new-commits trigger are now individually
  configurable per repo via a three-layer resolution (built-in default → repo config file → DB
  override, operator wins). `review_on_push` defaults off; when enabled, findings are suppressed
  PR-wide (not just within one commit) and a push storm is handled per the repo's chosen strategy —
  `supersede` (cancel the superseded run, default), `debounce` (delay into a quiet period), or
  `every` (one review per push). ([ADR-0111](docs/adr/0111-per-repo-review-settings-and-review-on-push.md),
  #567/#568/#569/#570/#572/#573/#574/#575/#576)

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
- **Repo-configurable OpenCode review presets** (Epic #491, closed) — replaces the fixed fast/deep tier
  model with per-repo preset selection, uniform tools/prompt, a full OpenCode fs-tool suite, GitHub MCP
  access via App-derived tokens, and an OpenCode fatal-session sentinel plugin; also carries per-identity
  (repo/org) model selection + ACL (the ADR-0038 upgrade, moved here from closed Epic #241). All 8 child
  stories shipped: `ReviewConfigs` generalized to named presets + the `.lightbridge-code-review.jsonc`
  reader (#494/#527/#528), every entry point (webhook/`@mention`/A2A) resolving its preset from repo
  config (#495/#529/#530), `ultra` joining `fast`/`deep` as the third platform-default preset name
  (#496), the full OpenCode fs-tool suite behind an explicit `fs_write` gate independent of the
  known-unenforced tool-allowlist mechanism (#497/#537), GitHub MCP wired in opt-in per preset via the
  App-derived installation token (#498/#538), the fatal-situation sentinel plugin
  (#499/#539), the lci TUI + apps/web preset-selection UI backed by a new narrow
  `CodePlatform::update_repo_file` forge-write capability (#500/#540), and repo/org model
  selection + ACL with a fail-closed operator allowlist (#501/#541).
  ([ADR-0103](docs/adr/0103-repo-configurable-opencode-review-presets.md)/
  [0104](docs/adr/0104-full-opencode-fs-tool-suite.md)/[0105](docs/adr/0105-github-mcp-via-app-derived-token.md)/
  [0106](docs/adr/0106-opencode-fatal-situation-sentinel-plugin.md)/
  [0109](docs/adr/0109-control-plane-forge-write-for-repo-review-config.md)/
  [0110](docs/adr/0110-identity-scoped-model-selection-and-acl.md))

## In progress / near-term follow-ups

- **Allowlist `run_sast` in ai-helm-values** — the single remaining blocker to SAST going live on the
  reviewer (the code is on both paths; the tool is just not offered until the values allowlist enables it).
- **Expose `config.review.opencode` in the ai-helm chart** — companion to the operator overlay above; the
  runner accepts the field, but no operator can set it until the chart surfaces it.
- **Remove the dead native review path** — now unblocked (SAST is ported); delete `run_native_agent` and
  its native-only modules.
- **`apps/web` full revamp** ([ADR-0112](docs/adr/0112-invest-in-apps-web-supersede-0063.md), supersedes
  the retirement plan) — new shared `Toggle`/`Select`/provenance-badge components, then a full
  `/dashboard/repositories/[id]` page exposing ADR-0111's per-repo settings (check-run reporting,
  review-on-open, review-on-push, push-storm strategy, dedup scope) and a model-override picker
  (ADR-0110), followed by a lower-priority consistency pass across Repositories/Runs/Overview/Settings.
  Sliced per-page, in progress.
- **A2A per-finding review streaming** — stream findings as they are confirmed at finalize.
  ([PR #458](https://github.com/vymalo/lightbridge-code-intelligence/pull/458) — open; no ADR yet)

## Planned — open epics

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
- **Review quality & reliability** (Epic #252) — the durable quality track: a fast-tier eval harness to
  catch calibration regressions (not started, being reframed around presets — see #491), the #285
  severity-stability watch, and the observability work above.

## Where the detail lives

- **Decisions & architecture:** [`docs/adr/`](docs/adr/) · [`docs/INDEX.md`](docs/INDEX.md)
- **How to contribute:** [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Tracked work:** GitHub Epics / User Stories / Dev Tickets (use the issue forms)
