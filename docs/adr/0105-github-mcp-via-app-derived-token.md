# ADR-0105: GitHub MCP access via App-derived installation token

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Extends:** [ADR-0001](0001-use-github-app.md), [ADR-0017](0017-agent-runner-control-plane-bootstrap.md), [ADR-0096](0096-mediated-forge-read-tools.md)

## Context and Problem Statement

No GitHub MCP integration exists anywhere in this codebase today — the review agent's GitHub
awareness is limited to the mediated forge-read tools ADR-0096 curated on `CodePlatform`. To do the
best possible review, the model should be able to reach the **full** GitHub MCP tool surface
(issues, PRs, checks, related-file history, etc.), not just the narrow curated subset. The
credential problem is already solved: [ADR-0001](0001-use-github-app.md) established the
GitHub App model, and `services/control-plane/src/integrations/github.rs` already mints short-lived
installation access tokens from the App private key
(`app_jwt()` → `installation_token(installation_id)`, cached ~50 minutes). No PAT, no long-lived
bot credential is needed — the missing piece is wiring that token into a GitHub MCP server as the
review agent's credential.

## Decision Drivers

- No new credential type. Reuse the existing App-JWT → installation-token mint, per
  [ADR-0017](0017-agent-runner-control-plane-bootstrap.md)'s "agent pod holds no App key"
  invariant — the token is minted control-plane-side and handed to the runner per-task, scoped to
  exactly the installation the task belongs to.
- The GitHub MCP server must not become a way to bypass [ADR-0022](0022-review-writeback-control-plane.md)
  (control plane owns what gets posted) — GitHub MCP write operations (issue comments, etc.) are
  excluded from the review agent's exposed toolset; only read/query tools are enabled for review.
  `open` mode, which already has mediated write tools (ADR-0037/ADR-0104), may expose more.

## Considered Options

- **A — Stand up an official `github-mcp-server` instance, credentialed per-task with a freshly
  minted installation token, network-reachable only from the running task's pod.** Chosen.
- **B — Reimplement GitHub API access as more mediated Rust tools instead of MCP.** Rejected: this
  is exactly the narrow-curated-subset problem this ADR exists to solve — GitHub's MCP server
  surface is broad and maintained upstream; reimplementing it in Rust duplicates that maintenance
  burden for no benefit over just credentialing the real thing.

## Decision Outcome

Chosen option: **A**. The control plane mints a per-task installation token (existing
`integrations/github.rs` path, no new minting logic) and passes it to the agent-runner alongside
the rest of the task bootstrap ([ADR-0017](0017-agent-runner-control-plane-bootstrap.md)); the
runner starts (or points to) a GitHub MCP server instance for the task's lifetime, credentialed with
that token, and registers it in the review preset's OpenCode config
(`knowledge_tools.mcp_servers`, [ADR-0066](0066-deep-tier-external-knowledge-tools.md)'s registry
mechanism) with a read-only tool allowlist for review, a broader allowlist for `open` mode.

### Consequences

- Good, because the review agent can now query GitHub natively (issue history, check runs, related
  PRs) instead of only what the curated `CodePlatform` read tools expose.
- Good, because token scope and lifetime are unchanged from the existing model — no new blast
  radius, the MCP server just becomes a new *consumer* of the same short-lived token.
- Bad, because a GitHub MCP server process now runs per task; resource/startup-time cost is a
  tracked implementation concern, not a design blocker.
- Neutral, because this is GitHub-specific by construction — a GitLab or Bitbucket equivalent
  (if warranted) is out of scope for this ADR and would be its own follow-on.

## More Information

Complements [ADR-0104](0104-full-opencode-fs-tool-suite.md) (the fs-tool surface) — together they
give the OpenCode-hosted agent a complete, mediated view of both the local checkout and the
upstream forge.
