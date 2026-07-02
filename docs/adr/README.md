# Architecture Decision Records

This directory records the significant architectural decisions for Lightbridge Code Intelligence,
using the [MADR](https://adr.github.io/madr/) format.

## Process

- An ADR captures **one** decision: its context, the chosen option, and the consequences.
- ADRs are **immutable once accepted**. We do **not** edit the substance of an accepted ADR.
  If a decision changes, write a **new** ADR that **supersedes** the old one, and update the
  superseded ADR's status to `Superseded by ADR-NNNN`.
- Status values: `Proposed`, `Accepted`, `Rejected`, `Deprecated`, `Superseded by ADR-NNNN`.
- New ADRs start from [the template](0000-adr-template.md) and are numbered sequentially with a
  kebab-case title, e.g. `0014-some-decision.md`.
- Substantial proposals are often socialized first as an [RFC](../rfc/README.md); an accepted RFC
  typically yields one or more ADRs.

## Index

| # | Title | Status |
|---|---|---|
| [0000](0000-adr-template.md) | ADR template | — |
| [0001](0001-use-github-app.md) | Use a GitHub App (not a PAT-backed bot) | Accepted |
| [0002](0002-rust-control-plane-trust-boundary.md) | The Rust control plane owns the trust boundary | Accepted |
| [0003](0003-dual-retrieval-neo4j-pgvector.md) | Dual retrieval: Neo4j + pgvector are complementary | Accepted |
| [0004](0004-one-k8s-job-per-task.md) | One Kubernetes Job per task for execution isolation | Accepted |
| [0005](0005-cratestack-schema-first-control-plane.md) | Adopt cratestack (schema-first Rust) for the control plane | Accepted |
| [0006](0006-nextjs-app-router-web-ui.md) | Next.js (App Router) for the web UI | Accepted |
| [0007](0007-better-auth-rust-backend-plugin.md) | better-auth for web auth via a rust-backend plugin | Superseded by ADR-0014 |
| [0008](0008-adopt-ai-governance-framework.md) | Adopt the ADORSYS-GIS AI Governance framework | Accepted |
| [0009](0009-pnpm-turborepo-monorepo.md) | pnpm + Turborepo monorepo layout | Accepted |
| [0010](0010-graphify-treesitter-indexing-baseline.md) | Graphify + tree-sitter as the indexing baseline | Accepted |
| [0011](0011-engineering-practices-xp-lean-devops.md) | Engineering practices: XP + Lean + DevOps + shift-left | Accepted |
| [0012](0012-rfc-process-alongside-adrs.md) | RFC process alongside ADRs | Accepted |
| [0013](0013-local-dev-and-build-tooling.md) | Local dev & build tooling (just, xtask, compose, nextest, wiremock) | Accepted |
| [0014](0014-keycloak-oidc-resource-server.md) | Keycloak OIDC — web client + resource server | Accepted |
| [0015](0015-web-console-design-language.md) | Web console design language & component system (shadcn/ui) | Superseded by ADR-0027 |
| [0016](0016-dashboard-information-architecture.md) | Dashboard information architecture & task-run views | Accepted |
| [0017](0017-agent-runner-control-plane-bootstrap.md) | Agent runner bootstraps from the control plane (no App key in the Job) | Accepted |
| [0018](0018-openai-compatible-embeddings.md) | OpenAI-compatible API for all embeddings (eaig/core-gateway; no bundled model) | Accepted |
| [0019](0019-graphify-cli-structural-graph.md) | Graphify (bundled CLI) extracts the structural graph; control plane writes Neo4j | Accepted |
| [0020](0020-mcp-servers-via-control-plane.md) | MCP servers are thin clients of the control-plane retrieval API (no datastore creds in the Job) | Accepted |
| [0021](0021-opencode-headless-review-agent.md) | OpenCode (headless `run`) is the review agent; generated opencode.json wires eaig + MCP | Superseded by 0026 |
| [0022](0022-review-writeback-control-plane.md) | Control plane validates findings against the PR diff and posts the review (no GitHub creds in the Job) | Accepted |
| [0023](0023-db-backed-rbac.md) | Permission-based authz: the token carries a permissions list under a configurable claim (aud still verified); enforce per-capability; no roles/DB | Accepted |
| [0024](0024-web-console-redesign-v2.md) | Web console redesign v2 — richer surfaces, grouped nav + ⌘K, runs table, insights (within the ADR-0015 contract) | Accepted |
| [0025](0025-review-reuses-base-index.md) | Review reuses the base index instead of re-indexing every run (perf) | Accepted |
| [0026](0026-native-review-agent.md) | Native Rust review agent with structured tool calls; drop OpenCode (robust output + control tools) | Accepted |
| [0027](0027-daisyui-design-system.md) | Adopt daisyUI (dracula theme) as the component layer; supersede ADR-0015's bespoke component/token mechanism | Accepted |
| [0028](0028-agent-job-control-sidecar.md) | Agent Job = control sidecar + single configurable main container (closed built-in pipeline) | Superseded by ADR-0026 |
| [0029](0029-focused-review-not-generic-runner.md) | Scope boundary: a focused code-review system, not a generic step/CI runner (reject arbitrary steps, CRD/operator, workflow engines) | Accepted |
| [0030](0030-repo-review-config.md) | Repo-level review configuration (`.lightbridge-code-review.jsonc`) — extend understanding, not execution | Proposed |
| [0031](0031-review-skills-commands.md) | Custom review skills/commands via the repo config (named prompts, `@mention`-invokable) | Proposed |
| [0032](0032-review-finding-priority-and-category.md) | Review findings carry a priority (P0–P2) + category (security always red), reviewed across all dimensions | Accepted |
| [0033](0033-inbound-command-parsing-and-run-kinds.md) | Parse the `@mention` comment body; run kinds (review / conversational `ask` / skill) + non-PR targets | Accepted |
| [0034](0034-agent-run-transcript-and-observability.md) | Persist the agent run transcript (tool calls, reasoning, tokens) and surface it in the dashboard | Accepted |
| [0035](0035-review-feedback-signal.md) | Capture 👍/👎 on posted reviews as a quality feedback signal (persist comment IDs, store + display) | Accepted |
| [0036](0036-auto-read-agent-instruction-files.md) | Auto-read conventional agent instruction files (AGENTS.md → CLAUDE.md → …) as review context, ranked, untrusted | Accepted |
| [0037](0037-agent-acts-via-mediated-tools.md) | The agent acts via mediated write tools (add_review_comment / add_comment / …); run kind is emergent, not classified up front | Accepted |
| [0038](0038-per-repo-review-model.md) | Per-repository review model selected in the admin UI from an operator allowlist (admin-owned, not the author-owned repo file) | Proposed (scope expanded → per-identity + ACL, see #241) |
| [0039](0039-agent-llm-resilience-and-observability.md) | Agent LLM resilience & observability: generous per-request timeout (180s, eaig reality), bounded retry/backoff on transient errors, per-run circuit breaker, optional model failover, captured HTTP error body, structured per-turn logging | Accepted |
| [0040](0040-re-review-reads-prior-findings.md) | A re-review reads the agent's own prior review as context (reconcile, don't contradict) | Accepted |
| [0041](0041-full-diff-coverage-gate.md) | Full-diff coverage gate: bounce an early finish while changed files are untouched | Accepted |
| [0042](0042-risk-first-review-and-parallel-batching.md) | Risk-first review strategy + parallel read-only tool batching + cumulative read budgets (Phase 1) | Accepted |
| [0043](0043-review-finding-verification.md) | Finding verification: evidence citation on every finding + a refute pass on P0/P1 (Phase 2) | Accepted |
| [0044](0044-feedback-memory-m1.md) | Feedback memory (M1): inject 👎-rejected findings as repo memory so the agent doesn't re-raise known false positives | Accepted |
| [0045](0045-context-window-budget.md) | Context-window budget: converge before overflow, trim consumed tool output, never discard buffered findings | Accepted |
| [0046](0046-observability-dashboard-deployment.md) | Observability dashboard deployment (most dashboards read Postgres, not Prometheus) | Accepted |
| [0047](0047-review-prompt-grounding-and-uncertainty.md) | Reviewer prompt — grounding & uncertainty calibration (empty retrieval ≠ proof of absence) | Accepted |
| [0048](0048-review-prompt-structure-and-technique.md) | Reviewer prompt — structure & technique adapted to a GLM / OpenAI-compatible model | Accepted |
| [0049](0049-eval-driven-reviewer-prompt-iteration.md) | Eval-driven reviewer-prompt iteration — golden cases before deploy | Accepted |
| [0050](0050-retrieval-pins-to-latest-indexed-snapshot.md) | Reviews reuse the latest indexed snapshot (pin retrieval + skip-check to it; no per-PR re-index) | Accepted |
| [0051](0051-per-model-config.md) | Per-model configuration blocks (primary / fallback / embeddings) | Accepted |
| [0052](0052-index-snapshot-pruning.md) | Index snapshot pruning — keep the latest + in-flight, sweep the rest | Accepted |
| [0053](0053-remove-review-fallback-model.md) | Remove the review fallback model (single model + retry/breaker; failover dropped) | Accepted |
| [0054](0054-review-model-and-provider-selection.md) | Stay on MiniMax-M2.7 (FP8) on DeepInfra for the review agent | Accepted |
| [0055](0055-review-waits-for-index-readiness.md) | A review waits for index readiness (the `WaitingForIndex` gate; RFC-0002 Phase 1.4) | Accepted |
| [0056](0056-control-plane-owns-the-posted-output.md) | The control plane owns what gets posted (PR review-only channel + failure notice) | Accepted |
| [0057](0057-poller-posts-failure-notice-on-uncatchable-kill.md) | The poller posts the failure notice for an uncatchable kill (closes the ADR-0056 gap) | Accepted (egress superseded by 0059) |
| [0058](0058-rename-poller-role-to-reconciler.md) | Rename the `poller` role to `reconciler` (bidirectional GitHub reconciliation) | Accepted |
| [0059](0059-reconciler-owns-all-github-egress.md) | The reconciler owns all GitHub egress via a transactional outbox (one writer) | Accepted |
| [0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md) | Capture the model's reasoning (`reasoning_content`) to logs + the GLM-5.2 latency finding (prod reverted to M2.7, realigning 0054) | Accepted |
| [0061](0061-sast-deterministic-finding-source.md) | SAST (opengrep) as a deterministic finding source — runs in the runner, rides the existing channel (not reviewdog), LLM-aware not LLM-gated | Accepted |
| [0062](0062-two-tier-review-fast-auto-deep-on-demand.md) | Two-tier review: fast auto pass (SAST + a lean diff-only LLM pass) on every PR; deep on-demand (`@mention`, full retrieval, 2h) — one model, two loop shapes | Accepted |
| [0063](0063-cli-only-repository-approval.md) | CLI-only repository approval (device-code over the existing OIDC-gated endpoints) — retire the web approval gate | Proposed |
| [0064](0064-observability-via-grafana-behind-caddy-oauth2.md) | Observability via Grafana behind a Caddy + oauth2 proxy — retire the bespoke web dashboards (transcript/feedback → Grafana panels) | Proposed |
| [0065](0065-re-review-dedup-and-reconciliation.md) | Re-review must not duplicate: short-circuit unchanged commits + dedup findings at finalize + re-derive-then-reconcile (refines ADR-0040) | Proposed |
| [0066](0066-deep-tier-external-knowledge-tools.md) | External-knowledge MCP tools, mediated by the control plane: a dynamically-discovered, config-driven registry of in-cluster MCP servers (`knowledge_tools.mcp_servers`, e.g. brave-search/context7) — no hardcoded per-provider tools, query/name not URL (keeps the Job sandboxed, no new secrets); any tier via the normal `review.<tier>.tools` allowlist, which picks discovered tools by `mcp__`-anchored regex selectors (not a blunt on/off), not deep-only | Accepted |
| [0067](0067-skip-auto-review-on-bot-authored-prs.md) | Skip the automatic fast review when the PR author is a bot (`user.type == "Bot"` + `[bot]` backstop, fail open); `@mention` deep review untouched; `review.skip_bot_authored_prs` defaults on | Accepted |
| [0068](0068-reaction-driven-review-lifecycle.md) | Reaction-driven review lifecycle: 👀 on work-started (not receipt), 👍 clean+silent (no comment), 👎 findings (❌→👎, GitHub has no ❌), 😕 failure — all on the trigger (PR body or the `@mention` comment via `tasks.trigger_comment_id`); silent-clean supersedes ADR-0056's never-silent for the clean case | Accepted |
