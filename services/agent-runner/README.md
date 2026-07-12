# agent-runner

The Rust binary that runs **inside each per-task Kubernetes Job**. It bootstraps from the control plane,
does the heavy repository work, and reports results back — holding no standing credentials.

## Lifecycle

1. **Bootstrap** ([ADR-0017](../../docs/adr/0017-agent-runner-control-plane-bootstrap.md)): the Job
   carries only its task id + the control-plane callback wiring. The runner calls
   `GET /internal/tasks/{id}` for its context plus a freshly-minted, short-lived installation token.
2. **Clone** the target repo at the head SHA using that token.
3. **Semantic index**: tree-sitter chunking → embeddings (OpenAI-compatible,
   [ADR-0018](../../docs/adr/0018-openai-compatible-embeddings.md)) → `code_chunks` (pgvector), posted to
   the control plane.
4. **Structural graph**: Graphify → the control plane writes Neo4j (best-effort / non-fatal,
   [ADR-0019](../../docs/adr/0019-graphify-cli-structural-graph.md)).
5. **SAST** (review tasks): a deterministic **opengrep** scan of the diff'd files
   ([ADR-0061](../../docs/adr/0061-sast-deterministic-finding-source.md)) — findings ride the *same*
   review channel (no second poster) and the LLM is made aware of them but never gated by them.
6. **Review** — a **native Rust agent loop** ([ADR-0026](../../docs/adr/0026-native-review-agent.md))
   that **acts via mediated write tools as it goes** ([ADR-0037](../../docs/adr/0037-agent-acts-via-mediated-tools.md)):
   `add_review_comment` (inline finding), `retract_finding` (drop one that didn't hold), `add_comment`
   (plain reply), `finish` (verdict). It runs in **two tiers** keyed by the task's tier
   ([ADR-0062](../../docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)): **`fast`** (auto on
   PR-opened) = a cheap model + SAST + a lean diff-only pass with **no retrieval**, a small per-tier tool
   allowlist (`review.<tier>.tools`), and a short turn cap; **`deep`** (`@mention`) = a strong model with
   full graph/vector retrieval + `read_file`, multi-turn, plus a coverage gate and a P0/P1 **refute pass**
   ([ADR-0041](../../docs/adr/0041-full-diff-coverage-gate.md)/[ADR-0043](../../docs/adr/0043-review-finding-verification.md)).
   The control plane buffers all writes and posts nothing until finalize, so a mid-run failure posts
   nothing. The system prompt is **required operational config** (ai-helm `config.reviewSystemPrompt` /
   `…Fast`, mounted) — no built-in default; review fails closed without one. This in-process loop is the
   only review path (OpenCode + its stdio MCP servers were removed in #140).
7. **Report** terminal status; on a clean finish the control plane shapes + **enqueues** the review for
   egress (validating findings against the PR diff,
   [ADR-0022](../../docs/adr/0022-review-writeback-control-plane.md)) — the `reconciler` posts it as one
   grouped review ([ADR-0059](../../docs/adr/0059-reconciler-owns-all-github-egress.md)).

The stage sequence lives in [`src/main.rs`](src/main.rs) (`run()`).

## Job kinds & index reuse

- **`index`** task (on repo approval / default-branch push): index only, no review.
- **`review`** task: carries a **tier** — `fast` (auto on PR-opened) or `deep` (`@mention`) — see the
  Review step above. A warm repo **reuses the base index** and skips re-indexing
  ([ADR-0025](../../docs/adr/0025-review-reuses-base-index.md)); a cold repo indexes first.

See [docs/jobs-and-lifecycle.md](../../docs/jobs-and-lifecycle.md).

## Security posture

No GitHub App key, no datastore credentials: the agent's retrieval tools go through the control-plane
API (the trust property from [ADR-0020](../../docs/adr/0020-mcp-servers-via-control-plane.md), now
in-process rather than via stdio MCP servers). It holds only the short-lived install token, the shared
`AGENT_RUNNER_TOKEN`, the embeddings key, and a mounted internal-CA cert.

## Cancellation

Cooperative: the runner polls `GET /internal/tasks/{id}/status` (~10s) and exits promptly if the task was
cancelled upstream, complementing the dispatcher reaper deleting the Job (#116).

## Tests

`cargo nextest run -p agent-runner` covers the Job host and review behavior. The extracted HTTP
clients own their **wiremock** control-plane contract suite under
`services/agent-clients/tests/control_plane_contract.rs`; run it with
`cargo nextest run -p lci-agent-clients` (no cluster needed).
