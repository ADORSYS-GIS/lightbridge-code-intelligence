# Control plane & data layer — deep dive

This is a pedagogical, verified-against-source walkthrough of the **control plane** (the Rust
service under `services/control-plane/`) and its **data layer** (Postgres/pgvector + Neo4j) in
Lightbridge Code Intelligence. It assumes zero prior context — every factual claim is checked
against the actual current source and cited as `path:line`; the repo's own docs (`docs/adr/`,
`docs/*.md`) are used for orientation only and are called out explicitly wherever they've drifted
from the live code (see file `02`'s naming note, in particular).

This is one of two sibling deep-dive efforts for "Session 2" of this project's documentation: a
companion set already covers `lci-codegraph` and `lci-mcp` in the same style (see e.g.
`lci-codegraph/deep-dive/05-tree-sitter-fundamentals.md` for calibration). This set covers the
control plane that those two systems feed into and are orchestrated by.

**These files are intentionally not committed** — scratch documentation for deep understanding, not
a durable part of the repo.

## Reading order

1. **[`01-control-plane-roles.md`](01-control-plane-roles.md)** — "One binary, many roles." How
   `main.rs` picks a role (`serve`, `dispatcher`, `reconciler`, `a2a`, `notifier`, `replay`, `mcp`,
   plus the `mint-runner-token` utility) from the first CLI arg or `CONTROL_PLANE_ROLE`, what each
   role does and which module owns it, why role selection happens before OTel init, and the
   trust-boundary framing (ADR-0002) that explains why the roles are split the way they are. §1.4
   steps back to the general and Rust-specific concepts that make this whole pattern possible: the
   multi-call-binary pattern, `match`-as-expression forcing every role function into the same
   shape, ownership + control-flow exclusivity for the one `state` move, and why a role's `Result`
   *is* its container's exit code.

2. **[`02-postgres-schema-and-queue.md`](02-postgres-schema-and-queue.md)** — The work-queue
   mechanics. The `tasks` table's queue columns, the `SELECT ... FOR UPDATE SKIP LOCKED` claim query
   quoted verbatim, the idempotency unique index, the transactional-outbox pattern (`outbox` table,
   ADR-0059 — and why it's not named `github_outbox` anymore), webhook dedup
   (`webhook_deliveries`), and the supporting tables: `reviews`, `review_comments`/
   `review_feedback`, `pending_review_actions`, `a2a_tasks`/`a2a_task_events`, `durable_step`.

3. **[`03-neo4j-structural-graph.md`](03-neo4j-structural-graph.md)** — The real Cypher behind the
   structural code graph: the `:Symbol`/`:REL` upsert, `find_symbol`, `get_callers`, the vector +
   fulltext index DDL, the uniqueness constraint, and — the centerpiece — `hybrid_symbol_search`'s
   weighted-reciprocal-rank-fusion query, quoted and explained clause by clause.

4. **[`04-pgvector-and-dual-retrieval.md`](04-pgvector-and-dual-retrieval.md)** — Why two stores.
   The `code_chunks` table (4096-dim, deliberately with no ANN index — and why), the exact
   cosine-similarity search query, and snapshot pinning (ADR-0050): why retrieval reuses the latest
   indexed commit rather than re-indexing every PR head.

5. **[`05-glossary.md`](05-glossary.md)** — Every domain term introduced in files `01`–`04` (role,
   lease, outbox, run epoch, snapshot, hybrid search, RRF, trust boundary, ...), one paragraph each,
   with pointers back to where each is defined/used.

6. **[`10-appstate-and-rust-shared-state.md`](10-appstate-and-rust-shared-state.md)** — A focused
   deep dive on `AppState` (`main.rs:74`), the struct every `serve`-role handler holds: what
   "application state" means as a general pattern, why Rust needs `Arc`/`Mutex` (not just a plain
   shared object, the way a GC'd language would) to hand it to concurrent request handlers safely,
   and this struct's exact memory model, field by field, down to what `.clone()` actually costs.

7. **[`11-keycloak-and-oidc-auth.md`](11-keycloak-and-oidc-auth.md)** — Keycloak/OIDC
   authentication end to end: the general concepts needed (Auth Code+PKCE, access vs ID token,
   JWKS/issuer discovery, claims, refresh tokens), then every real consumer — `apps/web`'s OIDC
   client (`proxy.ts`, not the stale-doc'd `middleware.ts`; `openid-client`+`jose`, not
   better-auth), the shared `packages/auth` TS client, the Rust control plane as a pure resource
   server (`jwt.rs`'s `JwtValidator`/`Claims`/`Caller`, ADR-0023's permissions-in-claims design),
   the `a2a` role's middleware variant of the same validator, and a third independent OIDC client
   in `clients/lci`. Closes with local-dev Keycloak wiring and deployment env vars. Cross-references
   `01` and `10` rather than re-deriving them, and calls out several stale docs (including this
   repo's own root `CLAUDE.md`, which still says "Next.js + better-auth") explicitly rather than
   silently fixing them.

8. **[`12-opencode-integration.md`](12-opencode-integration.md)** — What OpenCode (the vendored
   AI coding-agent runtime the review pipeline drives) actually is, how it's packaged into the
   live `review`-mode image, the real ACP transport, every file in
   `services/review-agent/src/opencode/` (config rendering, the ACP driver, the JSONL recorder,
   the gate composition), the dedicated `services/review-mcp` stdio server OpenCode spawns, the
   first-party plugins (and which one is deliberately neutered to a no-op for review, since the
   Rust-side gates are authoritative), and the real RFC-0009 → ADR-0094 → ADR-0097 ADR lineage.
   Deliberately does not repeat file `06`'s review-loop/gate mechanics — cites it instead — and
   corrects several claims in the repo's own `system_reports/10-opencode-integration.md`
   (notably: the vendored binary is musl-*dynamic*, not statically linked).

9. **[`13-pre-dispatch-cli-utilities.md`](13-pre-dispatch-cli-utilities.md)** — The one exception
   to file `01`'s "every role has the identical `async fn(AppState) -> Result<()>` shape" rule:
   `mint-runner-token`, intercepted in `main()` *before* `AppState::from_env()` runs at all. The
   general "don't pay for expensive setup on a cheap one-shot path" pattern, the real ADR-0092
   credential-bootstrap backstory, the full `mint_runner_token_cli()` code, and why this is a plain
   `if` before the role `match` rather than an eighth arm of it.

10. **[`14-frontend-console-and-control-plane-api.md`](14-frontend-console-and-control-plane-api.md)**
    — The frontend console: where it actually lives now (ADR-0115 moved it out of this repo's
    `apps/web`, now undeployed, into `apps/lci` in the sibling `converse-frontends` repo), its two
    server-only API clients (`api.ts`/`admin.ts`) and the `authedFetch`/`ApiResult` pattern that
    forwards the caller's own OIDC token with no service credential of its own, a page-by-page table
    of every real control-plane route each screen calls (cross-checked against
    `api_v2_router()` in `main.rs`), the code-graph canvas's browser → same-origin proxy route →
    control-plane three-hop path (tying `03`/`04`'s Neo4j/pgvector queries to an actual click), and
    the Feedback pages' one-query-many-panels shape. Closes by naming two real gaps: admin routes
    the console has no UI for yet, and a `'github' | 'gitlab'` frontend type that hasn't caught up
    to the backend's three-platform (`+ 'bitbucket'`) reality.

11. **[`15-how-inline-comments-get-their-line.md`](15-how-inline-comments-get-their-line.md)** — A
    single mediated tool (`add_review_comment`), traced end to end: the two places the model is told
    how to pick a line (the system prompt's `TOOL_PROTOCOL` *and* the tool's own JSON-schema
    parameter descriptions — code-owned, not operator-owned), the mediated POST into
    `pending_review_actions` (unvalidated), the finalize-time validator that re-fetches the real diff
    and independently decides added/clamped-to-nearest-added/deferred-to-the-summary/out-of-scope for
    every finding, the resulting `InlineComment` list frozen into the `outbox` row's JSON payload, and
    the reconciler's last-mile fork into GitHub's `ReviewComment` JSON vs. GitLab's `position` object
    (`base_sha`/`head_sha`/`start_sha`). Closes by naming a real, currently-true gap: `start_line` is
    accepted by the tool and explained by both prompt surfaces, but dropped on the floor between the
    buffer and `Finding` construction (ADR-0071's companion ticket #287) — the range-validation code
    downstream is fully built and tested, just not yet reachable from a live run.

12. **[`16-the-rustls-crypto-provider-pin.md`](16-the-rustls-crypto-provider-pin.md)** — One line
    at the very top of `main()` in both long-running binaries,
    `lci_observability::install_default_crypto_provider()`: what a pluggable rustls `CryptoProvider`
    actually is, why this workspace genuinely links two of them (`ring` via sqlx's
    `tls-rustls-ring-webpki`, `aws-lc-rs` via `rmcp` → reqwest → `rustls-platform-verifier`) and
    rustls 0.23 refuses to auto-select between them, the exact three-line fix and why it must run
    before anything else (including OTel init), and the real documented incident this prevents — a
    guaranteed startup panic on the `dispatcher` role's first `kube` client handshake. Also covers
    why `ring` specifically was pinned (it's already load-bearing for `jsonwebtoken` and the A2A
    push-token AEAD encryption) and the near-miss where removing Restate (ADR-0093) almost silently
    reintroduced the same class of bug for `jsonwebtoken`.

## How to use this

Read in order once, start to finish — each file assumes the previous ones. After that, it's a
reference: the glossary (`05`) is the fastest way back in if you only remember a term, not which
file it was in.
