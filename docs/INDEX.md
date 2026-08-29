# Documentation Index

This directory contains the complete documentation set for Lightbridge Code Intelligence.

> **Active architecture redesign — control-plane v2 ([RFC-0007](rfc/0007-control-plane-v2-planes.md)).**
> The system is being restructured into **two binaries, three planes**: a `control-plane`
> (ingress → orchestration → egress; DB + forge creds; no checkout) and a new **`agent-plane`**
> ([ADR-0085](adr/0085-agent-execution-plane.md)) selected by **mode × host** (`{index, review, open} ×
> {run-once, serve}`). Enabling decisions: an in-house code-graph crate retires Graphify
> ([ADR-0086](adr/0086-in-house-code-graph-crate.md)); `CheckpointRuntime` gives the loop replay
> without Restate ([ADR-0087](adr/0087-durable-replay-checkpoint-runtime.md)); the `open` autonomous
> ticket→PR agent ([ADR-0088](adr/0088-open-mode-autonomous-ticket-agent.md)) is the first new
> capability. The "Core docs" below describe the **currently running** system; each carries a pointer
> to the v2 target.

## Table of contents

### Core docs
- [Executive summary](executive-summary.md)
- [Architecture overview](architecture.md)
- [Components and data models](components-and-data-models.md)
- [GitHub App and Rust control plane](github-app-and-control-plane.md)
- [Control-plane roles & GitHub egress](control-plane-roles-and-github-egress.md) — the three roles (serve / dispatcher / reconciler) and the single-egress outbox (ADR-0058/0059)
- [Jobs and task lifecycle](jobs-and-lifecycle.md) — the two job kinds, state machine, cancellation + purge (with diagrams)
- [Review pipeline](review-pipeline.md) — the whole review subsystem end to end: preset resolution per entry point (repo-configurable, platform defaults `fast`/`deep` shipped so far — [ADR-0103](adr/0103-repo-configurable-opencode-review-presets.md), superseding the old fixed two-tier model, [ADR-0062](adr/0062-two-tier-review-fast-auto-deep-on-demand.md)), the opengrep SAST tool (`run_sast`, [ADR-0073](adr/0073-sast-as-agent-tool.md)), the OpenCode-hosted mediated-tools agent loop (live since 2026-07-17, [ADR-0097](adr/0097-review-runs-on-opencode.md); the native loop is the fallback), control-plane finalize/shaping, and egress.
- [Indexing and storage](indexing-and-storage.md)
- [Kubernetes and deployment](kubernetes-deployment.md)
- [Security, observability, testing, rollout](security-observability-testing-rollout.md)
- [FAQ](faq.md)
- [OpenCode ACP and MCP integration](opencode-acp-mcp.md) — **historical / superseded** by the native agent ([ADR-0026](adr/0026-native-review-agent.md)/[ADR-0037](adr/0037-agent-acts-via-mediated-tools.md)); see [review-pipeline.md](review-pipeline.md) for the running system

### Run it
- [Local setup guide](local-setup.md) — compose deps, GitHub App + webhook proxy, manual trigger, multipass + k3s
- [Runbook: setting up and demoing the Bitbucket `CodePlatform`](runbooks/bitbucket-platform-setup.md) — configure one Bitbucket repo end to end (API token, webhook, approval) and prove a webhook produces a review (ADR-0072/0108)
- [Runbook: activating the Restate egress pilot](runbooks/restate-egress-pilot-activation.md) — turning on (and safely rolling back) the Restate-backed egress path (RFC-0005 Phase A / ADR-0074)
- [**P0** — `webhook_deliveries` grows without bound](runbooks/webhook-deliveries-unbounded-growth.md) — the unretained table that exhausted the shared Postgres volume on 2026-08-29 and took every lightbridge service down, including all logins; measurements, the three constraints a naive DELETE breaks, and the fix that fits them

### Decisions and process
- [Architecture Decision Records (ADRs)](adr/README.md)
- [Requests for Comments (RFCs)](rfc/README.md)

### Ways of working
- [Engineering practices](ways-of-working/engineering-practices.md)
- [OKRs](ways-of-working/okrs.md)

## Reading paths

### Stakeholder path
1. [README](../README.md)
2. [Executive summary](executive-summary.md)
3. [Architecture overview](architecture.md)
4. [FAQ](faq.md)

### Backend engineer path
1. [Architecture overview](architecture.md)
2. [Components and data models](components-and-data-models.md)
3. [GitHub App and Rust control plane](github-app-and-control-plane.md)
4. [Control-plane roles & GitHub egress](control-plane-roles-and-github-egress.md) — the serve/dispatcher/reconciler split and how every GitHub write flows through one outbox ([ADR-0058](adr/0058-rename-poller-role-to-reconciler.md)/[ADR-0059](adr/0059-reconciler-owns-all-github-egress.md)).
5. [Indexing and storage](indexing-and-storage.md) — reviews reuse the base index ([ADR-0025](adr/0025-review-reuses-base-index.md)) by pinning retrieval + the skip-check to the latest indexed snapshot ([ADR-0050](adr/0050-retrieval-pins-to-latest-indexed-snapshot.md)), so a PR review doesn't re-index from scratch.
6. [Jobs and task lifecycle](jobs-and-lifecycle.md)
7. [Review pipeline](review-pipeline.md) — the canonical end-to-end review-subsystem reference: repo-configurable preset selection ([ADR-0103](adr/0103-repo-configurable-opencode-review-presets.md), superseding the fixed two-tier model of [ADR-0062](adr/0062-two-tier-review-fast-auto-deep-on-demand.md)), the per-preset tool allowlist, the opengrep SAST tool ([ADR-0061](adr/0061-sast-deterministic-finding-source.md) + [ADR-0073](adr/0073-sast-as-agent-tool.md)), the OpenCode-hosted agent loop (live since 2026-07-17, [ADR-0097](adr/0097-review-runs-on-opencode.md); the native loop is the fallback), finalize/shaping, and egress.
8. The review agent ADRs — [ADR-0026](adr/0026-native-review-agent.md) (native loop) + [ADR-0037](adr/0037-agent-acts-via-mediated-tools.md) (mediated tools) + [ADR-0020](adr/0020-mcp-servers-via-control-plane.md) (retrieval tools) + [ADR-0039](adr/0039-agent-llm-resilience-and-observability.md) (LLM resilience: timeout/retry/circuit-breaker + structured logging; the fallback model was removed in [ADR-0053](adr/0053-remove-review-fallback-model.md)). Prompt engineering (epic #177): [ADR-0047](adr/0047-review-prompt-grounding-and-uncertainty.md) (grounding & uncertainty — empty retrieval ≠ absence), [ADR-0048](adr/0048-review-prompt-structure-and-technique.md) (prompt structure & technique) + the [revised-prompt draft](drafts/review-system-prompt.md), [ADR-0049](adr/0049-eval-driven-reviewer-prompt-iteration.md) (eval-driven prompt iteration). Historical: [OpenCode ACP/MCP](opencode-acp-mcp.md).

### Platform engineer path
1. [Architecture overview](architecture.md)
2. [Kubernetes and deployment](kubernetes-deployment.md)
3. [Security, observability, testing, rollout](security-observability-testing-rollout.md) + [ADR-0046](adr/0046-observability-dashboard-deployment.md) (how the Grafana dashboards deploy; most read Postgres, not Prometheus)
4. [ADR-0101: CI moves to ARC pod runners; dind is rejected as a Docker-in-CI fix](adr/0101-ci-arc-pod-runners-no-dind.md) — the canonical reference for `vymalo-vps` self-hosted CI topology (no CI-runner section exists elsewhere in these docs)

### Web & auth path
1. [Architecture overview — Web & auth tier](architecture.md#web--auth-tier)
2. [ADR-0006: Next.js (App Router) for the web UI](adr/0006-nextjs-app-router-web-ui.md)
3. [ADR-0014: Keycloak OIDC — web client + control-plane resource server](adr/0014-keycloak-oidc-resource-server.md) (supersedes the better-auth/rust-backend idea in [ADR-0007](adr/0007-better-auth-rust-backend-plugin.md))
4. [ADR-0023: permission-based authz (permissions claim, per-capability)](adr/0023-db-backed-rbac.md)
5. [ADR-0027: daisyUI (dracula) design system](adr/0027-daisyui-design-system.md)
6. [ADR-0102: run logs move from a native k8s pod-log stream to an embedded Grafana/Loki panel](adr/0102-grafana-loki-embedded-run-logs.md)
7. [FAQ — authN vs authZ](faq.md#how-does-authentication-authn-differ-from-authorization-authz)

## Design principles

- GitHub App, not a PAT-backed bot
- Rust control plane owns trust boundaries
- Graph + vector retrieval are complementary
- Agent execution is isolated per task
- All write actions are controller-validated
- Security posture depends on trust level of source branch / fork
- Authentication (authN) is **Keycloak OIDC** — the web console is an OIDC client and the control
  plane a resource server ([ADR-0014](adr/0014-keycloak-oidc-resource-server.md), which supersedes the
  earlier better-auth/rust-backend plugin idea in ADR-0007). Authorization (authZ) is
  **permission-based**: the token carries a `permissions` list under a configurable claim, enforced
  per-capability ([ADR-0023](adr/0023-db-backed-rbac.md))
