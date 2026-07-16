# ADR-0096: Forge read tools on the mediated surface — the App key stays control-plane-side

- **Status:** Proposed
- **Date:** 2026-07-16
- **Deciders:** @stephane-segning

## Context and Problem Statement

An agent working a ticket (the `open` mode, [ADR-0088](0088-open-mode-autonomous-ticket-agent.md))
or reviewing a diff benefits from reading **forge context beyond its bootstrap inputs**: other
branches, the full text of the triggering issue and *linked* issues, sibling PRs and their review
comments, a file at a ref that isn't in the shallow checkout. Today the agent gets only the diff and
the triggering ticket as bootstrap inputs; everything else is invisible.

The obvious move — drop the `gh` CLI or [github-mcp-server](https://github.com/github/github-mcp-server)
into the agent image and authenticate it — runs straight into the invariant
[ADR-0088](0088-open-mode-autonomous-ticket-agent.md) exists to protect: **the agent pod executes
untrusted code** (repo build scripts + LLM-generated code), so it is kept credential-light and
egress-restricted. The GitHub **App private key** is the master credential — it mints installation
tokens for *every* repo the App is installed on, at the App's full permission set. A prompt-injection
or a hostile build script in that pod could exfiltrate it → org-wide compromise (ADR-0088 risk O1).
Putting it there is the worst possible placement.

This ADR answers: **how does the agent get forge reads without a forge credential — least of all the
App key — entering the untrusted-code pod?**

## Decision Drivers

- **The ADR-0088 boundary holds:** no forge credential in the code-executing pod; its egress
  allowlist is not widened. The App key never leaves the control plane.
- **Reuse the existing seam.** The pod already talks to the control plane over the mediated
  `lightbridge` MCP endpoint with its task-scoped runner token
  ([ADR-0092](0092-per-task-runner-tokens.md)). Reads are just more tools on that channel —
  [ADR-0020](0020-mcp-servers-via-control-plane.md) ("MCP servers are thin clients of the control
  plane; no datastore/forge creds in the Job") + [ADR-0037](0037-agent-acts-via-mediated-tools.md)
  (mediated actions), extended from writes to reads.
- **Short-lived, scoped tokens, never the App key.** The App mints **read-only, repo-scoped,
  ≤1h installation access tokens**; those, not the private key, are what any GitHub tool ever sees.
- **Long tasks outlive a token.** An `open` task (investigate → edit → build → test → iterate) can
  run longer than an installation token's ≤1h TTL. Whoever holds the token must be able to refresh
  it without a mid-task failure.
- **Multi-forge.** The platform is forge-agnostic ([ADR-0072](0072-platform-abstraction-layer.md):
  `CodePlatform` / `GithubApp` / `GitlabClient`); a GitHub-only solution regresses that.

## Considered Options

- **Option A — mediated forge-read tools, control-plane-side** (this ADR): the control plane exposes
  a curated read set as mediated MCP tools; the pod calls them over the endpoint it already has. The
  App key and every minted token stay control-plane-side.
- **Option B — a scoped read token injected into the pod:** the control plane mints a read-only,
  single-repo, ≤1h installation token and injects it (as `GITHUB_PERSONAL_ACCESS_TOKEN`) for an
  in-pod `--read-only` github-mcp-server / `gh`; the sandbox egress allowlist is opened to
  `api.github.com`.
- **Option C — the GitHub App private key in the pod** (the naive "embed the tool with the bot's
  key"): rejected outright.

## Decision Outcome

Chosen option: **Option A.** The control plane — which already holds the App creds and already talks
to the forge ([ADR-0022](0022-review-writeback-control-plane.md),
[ADR-0037](0037-agent-acts-via-mediated-tools.md), [ADR-0072](0072-platform-abstraction-layer.md)) —
gains a **curated set of read-only forge tools on the mediated `lightbridge` MCP surface**. The agent
(any mode/host, including the OpenCode host of
[ADR-0094](0094-opencode-acp-open-mode-host.md)) calls them over the same channel it already uses; the
control plane performs the forge call with a **read-only, task-repo-scoped, short-TTL installation
token it mints and refreshes server-side.**

The pod gains the capability with **zero new credential and zero new egress** — it never holds a
token, never reaches `api.github.com`, and the token-expiry problem disappears because the control
plane mints a fresh installation token per call (they are cheap to mint from the App key).

**Tool surface (curated, read-only, forge-neutral).** Built on the `CodePlatform` abstraction so the
same tools serve GitHub and GitLab ([ADR-0072](0072-platform-abstraction-layer.md)):
`get_issue`, `list_branches`, `read_file_at_ref`, `get_pull_request`, `list_pull_request_comments`,
`search_code` (repo-scoped). Registered in the per-tier allowlist alongside the existing retrieval
tools ([ADR-0090](0090-hybrid-retrieval-tools.md) pattern); read-only, so they are safe for the
read-only `explore` subagent ([ADR-0094](0094-opencode-acp-open-mode-host.md)) as well as the primary.

**Authorization — reads are scoped to the task's own repo.** The mediated call is authenticated by
the caller's task-scoped runner token (ADR-0092); the control plane resolves the task → its repo and
scopes every read to **that repo** (any ref/issue/PR *within* it). The agent cannot pass an arbitrary
`owner/repo`; cross-repo reads are denied by default. This keeps a prompt-injected agent from turning
a read tool into an org-wide read primitive.

### Sub-decision — curate on `CodePlatform`, do **not** adopt github-mcp-server in-tree

The engine behind these tools is the **existing `CodePlatform` client**, not
[github-mcp-server](https://github.com/github/github-mcp-server). github-mcp-server is a fine tool
(single Go binary, `--read-only`, `--toolsets` scoping — confirmed), and it *could* run behind the
control-plane MCP ingress with a minted read-only token. But curating on `CodePlatform` wins here:

- **Multi-forge for free** — the same tools serve GitLab via `GitlabClient`; github-mcp-server is
  GitHub-only and bypasses the ADR-0072 abstraction.
- **Tight, curated surface** — a handful of named read tools with control-plane-shaped output, vs.
  github-mcp-server's dozens of tools returning raw GitHub API shapes, which inflate the agent's
  tool-choice/context budget ([ADR-0070](0070-window-proportional-prompt-budgets.md)).
- **No second forge client** to authenticate, scope, and keep patched.

github-mcp-server stays the fallback if the curated set proves too thin to be worth maintaining.

### Consequences

- **Good:** the ADR-0088 boundary is fully intact — no forge credential and no new egress in the
  untrusted-code pod; the App key never leaves the control plane; a scoped short-TTL token is the
  most any forge call ever sees, and only control-plane-side.
- **Good:** reuses the existing mediated seam — nothing new in the agent image; the reads are
  available to every mode/host and to the read-only `explore` subagent.
- **Good:** no token-expiry failure mode on long tasks (fresh installation token minted per call
  server-side); multi-forge via `CodePlatform`.
- **Bad / accepted:** the control plane does more work (a read proxy + per-repo scoping + token
  minting/caching) and is on the path of every forge read — but it is already the forge-credential
  authority, so this is the natural home, not new trust surface.
- **Bad / accepted:** a curated set can be too narrow; if the agent needs a read we didn't expose,
  it's a control-plane change, not a config flip. Mitigated by starting from the concrete needs
  above and keeping github-mcp-server as the escape hatch.
- **Neutral:** rate limits and audit now concentrate on the control plane's App identity — already
  true for every write; reads just add volume, and per-repo scoping bounds abuse.

## Pros and Cons of the Options

### Option A — mediated, control-plane-side (chosen)

- Good: zero credential/egress added to the untrusted pod; reuses the seam; no token-expiry;
  multi-forge; curated least-privilege surface.
- Bad: control plane does more; a curated set may need extension over time.

### Option B — scoped read token in the pod

- Good: the agent talks to the forge "directly"; no control-plane read proxy to build.
- Bad: **still puts a forge credential in the untrusted-code pod** and **opens sandbox egress to
  `api.github.com`** — two real widenings of ADR-0088, for a capability Option A delivers without
  either. And a ≤1h token can expire mid-task, forcing in-pod refresh (another channel) or a longer
  TTL (worse). Read-only + single-repo + short-TTL keeps the blast radius modest ("read this one repo
  for ≤1h if compromised"), so it is *defensible* — but strictly more surface than A for the same
  result. Rejected as the default; the token-minting machinery it needs is built by A anyway.

### Option C — App private key in the pod

- Good: none that survive the risk.
- Bad: the **master forge credential in the pod that runs untrusted code.** A prompt-injection or a
  hostile build script exfiltrates it → org-wide, write-capable compromise (ADR-0088 O1). Note even
  the tool the request named (github-mcp-server) does not want the App key — it takes a *token* — so
  there is no reason to place the key here. Contradicts ADR-0088 / ADR-0002 / ADR-0037. Hard reject.

## More Information

- [ADR-0088](0088-open-mode-autonomous-ticket-agent.md) — the sandbox/trust boundary this preserves;
  its bootstrap-inputs-only limitation is what these read tools relax, safely.
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) / [ADR-0020](0020-mcp-servers-via-control-plane.md)
  — the mediated-tool / thin-MCP-client boundary this extends from writes to reads.
- [ADR-0072](0072-platform-abstraction-layer.md) — the `CodePlatform` abstraction the curated tools
  build on (multi-forge).
- [ADR-0092](0092-per-task-runner-tokens.md) — the task-scoped runner token that authenticates the
  mediated call and anchors per-repo scoping.
- [ADR-0094](0094-opencode-acp-open-mode-host.md) — the OpenCode host whose `explore` subagent and
  `lci-open` primary consume these read tools.
- [github-mcp-server](https://github.com/github/github-mcp-server) (v1.6.0) and
  [gh CLI](https://github.com/cli/cli) (v2.96.0) — the considered off-the-shelf engines; kept as the
  fallback, run control-plane-side and `--read-only` if adopted.
