# Implementation plan: follow-ups from PR #626's review

Scope: the four findings from PR #626's review confirmed worth acting on (`services/control-plane`,
`services/agent-clients`). Each is written as an independent, minimally-scoped change — no reason to
land them together. Two findings from the same review (eager in-memory batch materialization,
duplicated row-builder closures) are intentionally excluded — see [Excluded](#excluded) at the end.

---

## 1. Request timeout on `ControlPlaneClient`

**Problem.** `services/agent-clients/src/control_plane/mod.rs::ControlPlaneClient::new` builds
`reqwest::Client::new()` with no timeout. Every call through this client — chunk submission, graph
submission, review posting — can hang indefinitely if the control plane (or whatever it's talking to
downstream) never responds. This is the literal mechanism behind the incident PR #626 fixed: a
request held open with nothing logged until something else eventually killed it. UNWIND batching
makes that far less likely to trigger, but doesn't remove the possibility for a large enough graph or
a slow enough Neo4j.

**Approach.** Mirror the pattern already established on `EmbeddingsClient`
(`services/agent-clients/src/embeddings.rs::with_timeout`) — a builder method that rebuilds the
transport with a configured timeout, called from the runner's bootstrap config so the value is
env-tunable rather than hardcoded:

```rust
impl ControlPlaneClient {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("building the control-plane HTTP client with a timeout cannot fail");
        self
    }
}
```

Wire it at construction in `agent-runner`'s bootstrap (`RunnerConfig`/`ControlPlaneClient::new` call
site), with a default generous enough for the largest legitimate payload (graph submission for a
large repo) — on the order of a few minutes — and an env override for operators to tune per
deployment, matching `EMBEDDINGS_*` config's existing shape.

**Files:** `services/agent-clients/src/control_plane/mod.rs`, the `agent-runner` bootstrap call site
that constructs `ControlPlaneClient`.

**Verification:** a unit test asserting the builder sets the timeout (mirroring
`EmbeddingsClient`'s own timeout tests), plus the existing control-plane/agent-runner suites
continuing to pass unchanged.

**Risk:** low — additive, only bounds an already-unbounded wait; the default just needs to be picked
generously enough not to false-positive on a legitimately large submission.

---

## 2. Batch `upsert_code_chunks` the same way

**Problem.** `services/control-plane/src/db/code_chunks.rs::upsert_code_chunks` has the identical
per-row-loop-in-a-transaction shape `upsert_graph` had before PR #626 — one `INSERT ... ON CONFLICT
DO UPDATE` per chunk, sequentially, inside one transaction. It's reachable from the same
`index_checkout` flow, and for a large repo (thousands of chunks) carries the same round-trip-count
scaling problem, just against Postgres instead of Neo4j.

**Approach.** Postgres's equivalent of Cypher's `UNWIND` is a multi-row `INSERT ... VALUES (...),
(...), ...` (or `INSERT ... SELECT * FROM UNNEST(...)` if pgvector's array support covers
`vector[]` cleanly — needs a quick check before committing to that form). Given the `ON CONFLICT ...
DO UPDATE` clause and the `::vector` cast per row, the more directly portable option is building one
parameterized multi-row `INSERT` via `sqlx::query_builder::QueryBuilder` — not currently used
anywhere in this codebase, but it's already part of the `sqlx` dependency already in use, not a new
external crate:

```rust
let mut qb = QueryBuilder::new(
    "INSERT INTO code_chunks (repository_id, commit_sha, file_path, language, chunk_type, \
     symbol_name, start_line, end_line, content, embedding) ",
);
qb.push_values(chunks, |mut b, chunk| {
    b.push_bind(repository_id)
        .push_bind(commit_sha)
        .push_bind(&chunk.file_path)
        // ...
        .push_bind(vector_literal(&chunk.embedding));
});
qb.push(
    " ON CONFLICT (repository_id, commit_sha, file_path, start_line, end_line) DO UPDATE SET \
     language = EXCLUDED.language, chunk_type = EXCLUDED.chunk_type, \
     symbol_name = EXCLUDED.symbol_name, content = EXCLUDED.content, \
     embedding = EXCLUDED.embedding",
);
qb.build().execute(&mut *tx).await?;
```

For very large chunk counts, cap rows per `INSERT` (e.g. a few hundred) and issue a handful of
statements rather than one unbounded one — Postgres has no `UNWIND`-style single-statement ceiling
concern the way Neo4j does, but an extremely wide parameter list is still worth bounding.

**Files:** `services/control-plane/src/db/code_chunks.rs`.

**Verification:** existing chunk-upsert tests (idempotency / `ON CONFLICT` behavior) continue to
pass against the rewritten function; add a test with a chunk count large enough to exercise the
batch-size cap if one is added.

**Risk:** low-medium — the `ON CONFLICT` + `::vector` cast interaction is the one part worth testing
carefully; everything else follows the same shape as #626.

---

## 3. Run the live-Neo4j tests in CI

**Problem.** `control-plane-tests.yml` only starts Postgres — its own comment says *"Neo4j is not
needed — the one graph test is `#[ignore]`d."* That's now two tests
(`upsert_graph_round_trips_against_live_neo4j`, `prune_graph_keeps_only_the_keep_set`), and they're
the only coverage of the Cypher `upsert_graph` actually runs — including the UNWIND rewrite from
#626. Neither has ever been exercised by CI.

**Approach.** The runner is a Docker-less ARC pod (rootless `podman`, not GitHub Actions'
`services:` blocks — the same reason Postgres is started via `podman run` rather than a `services:`
entry). Mirror that exact pattern for Neo4j:

```yaml
- name: Start Neo4j (rootless podman)
  run: |
    podman run -d --name neo4j \
      -p 7687:7687 \
      -e NEO4J_AUTH=neo4j/lightbridge \
      docker.io/library/neo4j:5
- name: Wait for Neo4j to be ready
  run: |
    for i in $(seq 1 30); do
      podman exec neo4j cypher-shell -u neo4j -p lightbridge 'RETURN 1' >/dev/null 2>&1 && exit 0
      sleep 2
    done
    echo "::error::neo4j did not become ready in time"; exit 1
```

then add `NEO4J_URI: bolt://127.0.0.1:7687` alongside the existing `DATABASE_URL` env, change the
test step to `cargo test -p control-plane --locked -- --include-ignored` (or split ignored Neo4j
tests into their own step), and stop the container in the existing `if: always()` cleanup alongside
Postgres. Update the stale "Neo4j is not needed" comment in the same change.

**Files:** `.github/workflows/control-plane-tests.yml`.

**Verification:** the workflow run itself — both previously-`#[ignore]`d tests going green in CI is
the proof.

**Risk:** low — additive to an existing job; the failure mode if misconfigured is the job timing out
waiting for Neo4j, not a false pass.

---

## 4. Uniqueness constraint on `:Symbol(repo_id, commit, node_id)`

**Problem.** `ensure_indexes` (`services/control-plane/src/integrations/neo4j.rs`) creates a vector
index and a fulltext index at startup, but nothing enforces uniqueness on the key `upsert_graph`
`MERGE`s against. Verified this does **not** cause the specific failure mode raised in review
(duplicate rows within one `UNWIND` batch correctly collapse to one node, tested directly against a
live Neo4j) — but it's still a real gap for two independent reasons worth fixing on its own merits:
concurrent `upsert_graph` calls for the same `(repo_id, commit, node_id)` racing without a
constraint, and `MERGE`'s existence check falling back to a full label scan instead of an index
lookup without one — a real cost at the row counts `upsert_graph` now writes in one statement.

**Approach.** Add a composite uniqueness constraint alongside the existing two indexes, applied
through the same idempotent `create_index_idempotent` helper already used for both of them:

```rust
create_index_idempotent(
    graph,
    "CREATE CONSTRAINT symbol_unique IF NOT EXISTS \
     FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE",
    "symbol_unique",
)
.await?;
```

**Files:** `services/control-plane/src/integrations/neo4j.rs::ensure_indexes`.

**Verification:** the existing live-Neo4j tests continue to pass (proves the constraint doesn't
reject the legitimate re-index/idempotent-MERGE path); once #3 lands, this runs in CI automatically.

**Risk:** low — purely additive schema, idempotent by construction like its two siblings.

---

## Suggested order

1. **#3 (CI coverage)** first — every other change here is safer to land once it's actually verified
   by CI rather than by hand each time.
2. **#4 (constraint)** — small, independent, no reason to wait.
3. **#1 (timeout)** — independent of the others, addresses the highest-severity residual gap.
4. **#2 (`upsert_code_chunks` batching)** — largest diff of the four; do last once the pattern from
   #626 has had a cycle to settle.

## Excluded

- **Eager in-memory batch materialization** (`Vec<HashMap<...>>` built fully before executing) — a
  real but low-severity tradeoff at the graph sizes actually observed (tens of MB at worst); revisit
  only if #2's Postgres side or real-world graph sizes ever make it matter.
- **Duplicated row-builder closures** in `upsert_graph` — the two closures map different field sets
  (5 vs. 3 fields); a shared generic helper for two call sites cuts against this repo's own stated
  convention against premature abstraction. Not planned unless a third call site appears.
