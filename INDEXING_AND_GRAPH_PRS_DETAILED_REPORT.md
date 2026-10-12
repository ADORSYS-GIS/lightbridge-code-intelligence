# Indexing & Graph PRs — Prioritized Report

**PRs:** #658 · #659 · #663 · #664 · #665 · #666
**Repository:** `ADORSYS-GIS/lightbridge-code-intelligence`
**State as of 2026-09-22:** `main` = `49ee422`. None of the six is merged.

The report is organized by **concern**, in the order to handle them. Section 1 comes first, and each
section only depends on the ones above it. Every section follows the same pattern:

1. **The concern**: what is wrong, with evidence.
2. **Why it ranks here.**
3. **Before merging**: findings still open on the PR, each checked against the code, and the fix needed.
4. **The changes**, file by file, with code.
5. **Tests.**
6. **Merge notes.**

Every code snippet is copied from the PR branch as it stands. Anything measured locally ran on
Neo4j 5.26.29 Community / Postgres 17. Anything quoted from production comes from logs posted on #662
or in this work. Four other open PRs by the same author (#614, #636, #660, #661) are unrelated and not
covered.

---

## The order at a glance

| # | Concern | PR | Status | Must happen before merge | Depends on |
|---|---|---|---|---|---|
| **1** | Deletes can take the database down (#662) | **#663** | ✅ **Merged** (`5bd8bdf`) | — | — |
| **2** | One failed batch restarts indexing from zero (#651) | **#658** | ✅ Approved; updated with `main` (`1dcad49`) | nothing | none |
| **3** | Concurrent writers create duplicate symbols (#662) | **#664** | 🔴 1 P1, 2 P2 | fix the silent lock failure; move the misplaced docs | none (conflicts with #663, keep both sides) |
| **4** | The identity key is unindexed; existing duplicates (#662) | **#665** | 🔴 1 P1, 1 P2 | resolve the index-swap window; decide the permission scope | **#664 deployed** before any repair is applied |
| **5** | A failed re-index deletes a graph it didn't write (#656) | **#666** | 🔴 1 P1 (bug), 2 P2, **conflicts with `main`** | fix the commit lookup; move the misplaced docs; rebase | none (conflicts with #663, keep both sides) |
| **6** | Index state is invisible (#656, #662) | **#659** | 🔴 1 P1, 2 P2 | rework so it recognizes #665's lookup index; report on every exit path | **#665** |
| **7** | `ensure_indexes` stops at its first failure | follow-up PR | not started | none | #663–#665 merged |
| **8** | Rollout, repair and cleanup in prod | ops | — | — | 1–6 deployed |
| **9** | Deferred hardening | not scheduled | — | — | — |

**#663 is merged; #658 is up to date with `main` and can merge.** Everything below
them has a P1 open.

---

## Priority 1 — Deletes can take the database down → #663

**Branch** `feat/bounded-graph-deletes` · **Head** `0f54d53` · **Issue** #662 · **Status: ✅ approved**
(stephane; he reports CI green)

### The concern

On 2026-09-18 the prod Neo4j ran out of heap and marked the database failed. Every transaction after
that was rejected (608 × `TransactionStartFailed … needs to be restarted`), while the pod stayed
`Running`. The root-cause comment on #662 matched each Neo4j error to a control-plane log line:

| Time (UTC) | Neo4j | Control plane | Role |
|---|---|---|---|
| 13:25:44 | Transaction **#7983** starts, **28,563 commands** | `lightbridge-governance`: `graph discard failed` | **The transaction that failed the database**: `delete_commit_graph` deleting a 3,610-node snapshot in one transaction |
| 13:26:13 | `OutOfMemoryError: Java heap space` | `ai-helm-values`: `merge edges … Java heap space` | A concurrent upsert, where the OOM showed up |
| 13:26:27 | `TransactionLogError` | `CoopData`: `attaching symbol embeddings failed` | First write rejected after the failure |

Upserts have been split into pages since #657. **None of the three delete paths has a size bound:**

| Path | Triggered by | On `main`, one transaction holds |
|---|---|---|
| `delete_commit_graph` | failed-submit rollback | the whole snapshot |
| `prune_graph` | `index_sweeper`, automatically | every stale snapshot of the repo, `collect()`ed into one heap list first |
| `delete_repo_graph` | repo removal or denial | every snapshot of the repo, the same way |

### Why it ranks first

It's the direct cause of the outage. It's control-plane only, has no dependencies and is approved.

### Before merging

Nothing blocks it. The two approver notes are follow-ups; see the end of this section.

### Changes — `services/control-plane/src/integrations/neo4j.rs` (+183 / −58)

#### Before (`main`)

```cypher
-- delete_commit_graph
MATCH (s:Symbol {repo_id: $repo, commit: $commit})
WITH s, count(s) AS _c
DETACH DELETE s
RETURN count(_c) AS deleted

-- delete_repo_graph (prune_graph identical plus `WHERE NOT s.commit IN $keep`)
MATCH (s:Symbol {repo_id: $repo})
WITH collect(s) AS nodes, count(s) AS deleted
FOREACH (n IN nodes | DETACH DELETE n)
RETURN deleted
```

#### After — a batch constant and one helper

```rust
/// Nodes removed per transaction when deleting graph data.
///
/// Neo4j holds a transaction's whole state on the heap until it commits, so a delete sized by its
/// input grows with the snapshot it removes. Batching keeps each transaction to a fixed size however
/// large the repository is.
const DELETE_BATCH_ROWS: u32 = 1_000;

/// Delete every `:Symbol` the `selector` matches, with its relationships, in transactions of
/// [`DELETE_BATCH_ROWS`]. Returns how many nodes matched.
///
/// `selector` is a `MATCH … WHERE …` binding `s`. `CALL { … } IN TRANSACTIONS` commits each batch on
/// its own and only runs in an auto-commit transaction, which is what `Graph::run` issues. The count
/// is read first because deleted nodes leave the query context.
async fn delete_symbols_in_batches(
    graph: &Graph,
    selector: &str,
    params: &[(&str, BoltType)],
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let mut count = query(&format!("{selector} RETURN count(s) AS n"));
    for (name, value) in params {
        count = count.param(name, value.clone());
    }
    let mut rows = graph
        .execute(count)
        .await
        .context("count symbols to delete")?;
    let matched = match rows.next().await.context("read delete count")? {
        Some(row) => row.get::<i64>("n").unwrap_or(0).max(0) as u64,
        None => 0,
    };
    if matched == 0 {
        return Ok(0);
    }

    let mut delete = query(&format!(
        "{selector} CALL (s) {{ DETACH DELETE s }} IN TRANSACTIONS OF {DELETE_BATCH_ROWS} ROWS"
    ));
    for (name, value) in params {
        delete = delete.param(name, value.clone());
    }
    graph.run(delete).await.context("delete symbols")?;
    Ok(matched)
}
```

- **Why `graph.run` and not `start_txn`:** Neo4j refuses `IN TRANSACTIONS` inside an explicit
  transaction. I verified that directly. neo4rs 0.8's `run` sends a plain `RUN`, which is auto-commit.
- **Why count first:** once `DETACH DELETE` has run, the nodes are no longer in the query context to count.
- **`CALL (s) { … }`** is the scope-clause syntax from Neo4j 5.23 onward. Compose runs `neo4j:5` and
  prod runs `5.26-community`, so both support it.
- **Injection:** every selector is a string literal at its call site, and every value that varies is a
  bound parameter. The reviewer confirmed this.

#### After — the three callers

```rust
pub async fn delete_commit_graph(graph: &Graph, repository_id: i64, commit_sha: &str) -> anyhow::Result<u64> {
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol {repo_id: $repo, commit: $commit})",
        &[("repo", repository_id.into()), ("commit", commit_sha.into())],
    )
    .await
    .context("delete commit graph")
}

pub async fn delete_repo_graph(graph: &Graph, repository_id: i64) -> anyhow::Result<u64> {
    // `commit IS NOT NULL` holds for every symbol; stating it lets the `(repo_id, commit)` index
    // serve a match that names only the repository.
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol) WHERE s.repo_id = $repo AND s.commit IS NOT NULL",
        &[("repo", repository_id.into())],
    )
    .await
    .context("delete repo graph")
}

pub async fn prune_graph(graph: &Graph, repository_id: i64, keep: &[String]) -> anyhow::Result<u64> {
    if keep.is_empty() {
        return Ok(0);
    }
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol {repo_id: $repo}) WHERE NOT s.commit IN $keep",
        &[("repo", repository_id.into()), ("keep", keep.to_vec().into())],
    )
    .await
    .context("prune repo graph")
}
```

The public signatures haven't changed, so the callers `queue/lifecycle.rs` and `queue/index_sweeper.rs`
are untouched. The reviewer traced both and found they handle an error as "retry later", and that a
re-run is idempotent against a partly finished delete.

#### After — a snapshot index in `ensure_indexes`

```rust
// Snapshot-level reads and deletes — discarding a commit, pruning stale commits, removing a
// repository — select by `(repo_id, commit)` and never name a node. The identity index below
// needs all three properties, so without this one each of them scans the whole label.
create_index_idempotent(
    graph,
    "CREATE INDEX symbol_snapshot IF NOT EXISTS FOR (s:Symbol) ON (s.repo_id, s.commit)",
    "symbol_snapshot",
)
.await?;
```

Which selectors use it, checked with `EXPLAIN`:

| Path | Predicate | Plan |
|---|---|---|
| discard | `repo_id = … AND commit = …` | `NodeIndexSeek` |
| prune | `repo_id = … AND NOT commit IN $keep` | `NodeIndexSeek` (the planner infers `commit IS NOT NULL`) |
| repo removal, old form | `repo_id = …` | `NodeByLabelScan` |
| repo removal, new form | `… AND commit IS NOT NULL` | `NodeIndexSeek` |

On a 16,022-node snapshot, the discard's selection drops from 74,125 database accesses to 32,045. More
importantly, its cost now depends on the snapshot being deleted, not on every symbol stored.

### Tests

`deletes_spanning_several_batches_remove_exactly_their_selection` (live): three 2,500-node commits
with edges, plus a second repository. Discard, prune and repository removal each assert exact counts,
that the kept commit survives, and that the other repository is untouched.

As the reviewer points out, this test proves the **selection** is correct across more than one batch's
worth of rows. It does **not** prove batching happened, and would pass just the same against an
unbatched delete (see “Follow-ups after merge” below).

Suite: **1,025 passed / 0 failed / 7 ignored**, 6/6 live Neo4j, clippy + fmt clean.

---

### Follow-ups after merge (non-blocking, from the approving review)

- **`AND s.commit IS NOT NULL` relies on an invariant the schema doesn't enforce.** Every `:Symbol` is
  written with a `commit` today, but a property-existence constraint isn't available on this deployment
  to guarantee it. If a symbol without a commit ever appeared, repository removal would skip it.
- **The multi-batch test proves the selection, not the batching.** It would pass unchanged against an
  unbatched delete. The PR text overstated what it shows. A test that actually observes transaction
  boundaries is a possible follow-up.

### Merge notes

Clean against `main`. PRs merged after it will conflict in `neo4j.rs` where they add code at the same
spot (#664, #666, #659). Resolve those by keeping both sides.

---

## Priority 2 — One failed batch restarts indexing from zero → #658

**Branch** `fix/index-resume-and-transient-body` · **Head** `cc59668` · **Issue** #651 ·
**Status: ✅ approved** (stephane; all findings resolved; he reports CI green)

### The concern

The runner embeds chunks in batches, and each batch lands in `code_chunks` with its embedding. If any
batch failed, the task failed. The next attempt started again from chunk 0 and re-embedded everything
already stored, multiplying embedding cost and time on a large repository and pushing the job toward
its deadline.

The failure that set this off was a **truncated embeddings response**: the gateway dropped the
connection mid-body. The client decoded the response straight into the target type, so a truncated
body looked like malformed JSON. It was classed as *not retryable*, so one dropped connection failed
the whole run.

### Why it ranks second

It's approved, touches only the chunk/embedding path, and doesn't overlap any graph PR. It can merge
alongside #663.

### Before merging

Nothing blocks it. Both earlier findings are resolved: the misplaced doc comment was fixed, and the
resume filter was extracted with tests that fail if the filter is inverted. The reviewer confirmed both.

### Changes, file by file

#### `services/control-plane/src/db/code_chunks.rs` — read what a snapshot already holds

```rust
/// The `(file_path, start_line, end_line)` of every chunk already stored for one snapshot.
///
/// Together with `repository_id` and `commit_sha` these are the conflict target `upsert_code_chunks`
/// writes against, so a chunk whose key appears here is already present with its embedding and does
/// not need to be embedded again.
pub async fn indexed_chunk_keys(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
) -> Result<Vec<(String, i32, i32)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_path, start_line, end_line FROM code_chunks \
         WHERE repository_id = $1 AND commit_sha = $2",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_all(pool)
    .await
}
```

The key returned is exactly the `ON CONFLICT` target of `upsert_code_chunks`. The embedding column is
`vector(4096) NOT NULL` and is written in the same statement as the row, so **if a key is present, its
embedding is present** by construction. The reviewer verified this against migration 0005.

#### `services/control-plane/src/http/internal.rs` — the endpoint

```rust
/// `GET /internal/tasks/{id}/chunks/indexed` — the chunks already stored for this task's snapshot.
pub async fn indexed_chunks(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    /* … 503 without a database … */
    let row: Option<(i64, Option<String>, String)> = match sqlx::query_as(
        "SELECT t.repository_id, t.head_sha, r.default_branch \
         FROM tasks t JOIN repositories r ON r.id = t.repository_id WHERE t.id = $1",
    ) /* … */;
    let Some((repository_id, head_sha, default_branch)) = row else {
        return (StatusCode::NOT_FOUND, "task not found").into_response();
    };
    // The same fallback the runner applies when it stamps `commit_sha` onto a chunk batch.
    let commit_sha = head_sha.unwrap_or(default_branch);

    match crate::db::indexed_chunk_keys(pool, repository_id, &commit_sha).await {
        Ok(keys) => Json(IndexedChunkKeys { commit_sha, keys: /* … map to IndexedChunkKey … */ })
            .into_response(),
        Err(error) => { /* 500 */ }
    }
}
```

The response structs `IndexedChunkKey { file_path, start_line, end_line }` and
`IndexedChunkKeys { commit_sha, keys }` are defined alongside it. Auth is `RunnerAuth`, the same per-task
bearer token as the sibling `POST …/chunks`. The response contains positions only, never chunk content
or embeddings.

The commit is resolved from **the task's own** `head_sha`/`default_branch`, the same way the runner
stamps it on each batch. (#666 gets this wrong; see Priority 5.)

#### `services/control-plane/src/main.rs` — route

```rust
// What this task's snapshot already holds, so a re-run embeds only the gap.
.route(
    "/internal/tasks/{id}/chunks/indexed",
    get(internal::indexed_chunks),
)
```

It's a GET, so the 16 MiB body limit on its sibling route isn't needed.

#### `services/agent-clients/src/control_plane/indexing.rs` — client method

```rust
/// One already-stored chunk's position (mirrors `internal.rs::IndexedChunkKey`).
#[derive(Debug, Deserialize)]
struct IndexedChunkKey { file_path: String, start_line: i32, end_line: i32 }

/// Body of `GET /internal/tasks/{id}/chunks/indexed`.
#[derive(Debug, Deserialize)]
struct IndexedChunkKeys { keys: Vec<IndexedChunkKey> }

/// `GET /internal/tasks/{id}/chunks/indexed` — the chunks already stored for this task's
/// snapshot, as `(file_path, start_line, end_line)`.
pub async fn indexed_chunk_keys(&self, task_id: Uuid) -> anyhow::Result<HashSet<(String, i32, i32)>> {
    let url = format!("{}/internal/tasks/{task_id}/chunks/indexed", self.base_url);
    let body: IndexedChunkKeys = self.http.get(&url).bearer_auth(&self.token)
        .send().await.context("requesting indexed chunk keys")?
        .error_for_status().context("control plane rejected the indexed-chunk lookup")?
        .json().await.context("parsing indexed chunk keys")?;
    Ok(body.keys.into_iter().map(|k| (k.file_path, k.start_line, k.end_line)).collect())
}
```

It returns a `HashSet`, so the runner's membership check is O(1) per chunk. The imports gain
`HashSet` and `Deserialize`. The follow-up commit also restored `submit_chunks`' own doc comment,
which an earlier insertion had detached (the bot's P2).

#### `services/agent-runner/src/indexer/mod.rs` — the resume

The filter is its own function, so it can be tested directly:

```rust
/// The chunks of `chunks` that `already_indexed` does not cover, in their original order.
#[must_use]
fn pending_chunks<'a>(
    chunks: &'a [Chunk],
    already_indexed: &HashSet<(String, i32, i32)>,
) -> Vec<&'a Chunk> {
    chunks
        .iter()
        .filter(|c| !already_indexed.contains(&(c.file_path.clone(), c.start_line, c.end_line)))
        .collect()
}
```

In `index_chunks`:

```rust
// Chunks already stored for this snapshot keep their embedding, so an index re-running over a
// commit it has partially indexed embeds only the gap. A lookup that fails is treated as an
// empty set: re-embedding is wasteful, not wrong, and is preferable to failing the run.
let already_indexed = match client.indexed_chunk_keys(context.task_id).await {
    Ok(keys) => keys,
    Err(error) => {
        tracing::warn!(error = %format!("{error:#}"), "indexed-chunk lookup failed; embedding every chunk");
        HashSet::new()
    }
};
let pending = pending_chunks(&out.chunks, &already_indexed);

let total = pending.len();
tracing::info!(
    chunk_count = out.chunks.len(),
    already_indexed = out.chunks.len() - total,
    pending = total,
    /* … */
    "walk complete; embedding in batches"
);
if pending.is_empty() {
    tracing::info!("every chunk for this snapshot is already indexed");
    return Ok(0);
}

for (batch_idx, batch_chunks) in pending.chunks(batch_size).enumerate() { /* unchanged */ }
```

- **If the lookup fails, it re-embeds everything and never skips.** That's the safe direction, and the
  reviewer confirmed it.
- The log line now separates chunks already stored from chunks pending.
- A fully indexed snapshot returns straight away, without calling the embeddings API.

#### `services/agent-clients/src/embeddings.rs` — retry a truncated body

Before:

```rust
let mut data = response
    .json::<EmbedResponse>()
    .await
    .map_err(|e| EmbedError {
        // A malformed 2xx body is not a transport problem — don't retry it.
        error: anyhow::Error::new(e).context("parsing embeddings response"),
        transient: false,
        retry_after: None,
    })?
```

After:

```rust
// Reading the body and parsing it are separated so the two failures stay distinguishable:
// a body that stops arriving mid-stream is a transport failure worth retrying, while one
// that arrives whole and does not parse is deterministic. Decoding straight into the type
// collapses both into one decode error.
let body = response.bytes().await.map_err(|e| EmbedError {
    error: anyhow::Error::new(e).context("reading embeddings response"),
    transient: true,
    retry_after: None,
})?;

let mut data = serde_json::from_slice::<EmbedResponse>(&body)
    .map_err(|e| EmbedError {
        error: anyhow::Error::new(e).context("parsing embeddings response"),
        transient: false,
        retry_after: None,
    })?
```

A body cut off by the network is now `transient: true` and gets the existing retry (3 attempts,
backoff with jitter). A body that arrives whole but is invalid is still `transient: false`. Matching on
reqwest's `is_body()`/`is_decode()` wouldn't work: a truncation shows up as a decode error, and matching
that would also start retrying genuinely malformed JSON.

### Tests

| Test | File | Proves |
|---|---|---|
| `a_truncated_response_body_is_retried_rather_than_failing_the_call` | `embeddings.rs` | A raw TCP listener promises `Content-Length: 4096`, sends a fragment and closes; the second connection answers properly, and `embed` reaches it. wiremock can't produce a truncated body, which is why the test uses a raw socket. |
| `indexed_chunk_keys_parses_the_stored_positions` / `…_is_empty_for_a_snapshot_never_indexed` | `control_plane_contract.rs` | Wire shape; the end line is part of the key |
| `indexed_chunk_keys_are_scoped_to_one_snapshot` | `db/tests.rs` | Scoped to the snapshot: another commit or another repo never leaks in |
| four `pending_chunks_*` tests | `indexer/mod.rs` | Which chunks survive, not only how many; every component of the key counts |

**Mutation-verified:** dropping the `!` in the filter fails **all four** `pending_chunks_*` tests. The
reviewer traced each one by hand; the commit message said three.

---

### Merge notes

Clean against `main`, with no overlap with any other PR here. Deploying it means rebuilding the agent
image as well as the control plane.

---

## Priority 3 — Concurrent writers create duplicate symbols → #664

**Branch** `feat/snapshot-write-lock` · **Head** `53659fa` · **Issue** #662 · **Status: 🔴 changes needed**

### The concern

`symbol_identity` was refused in prod because duplicates already exist:

```text
Both Node(3379) and Node(3387) have … repo_id = 430, commit = 'main',
node_id = 'charts/librechat-app/files/seed-agents.js'
```

**How `MERGE` produces a duplicate.** Without a uniqueness constraint, `MERGE` isn't atomic. Two
transactions can each look for the node, find nothing (the other's write isn't committed yet), and each
create it. Reproduced locally with the exact prod key:

| Setup | Nodes with that key afterwards |
|---|---|
| no constraint | **2**, and neither writer errors |
| `symbol_identity` present | **1** |

Overlapping writers to one snapshot are **rare**, because the dispatcher already prevents most of them:

- **One index at a time:** `create_index_task` refuses a second index task for a repository while one
  is queued, running or posting its result.
- **Reviews wait:** a review created while an index is in flight is parked (`waiting_for_index`,
  ADR-0055), and a review of an already-indexed repository reuses that index instead of writing one.
- **No duplicate runs of one task:** the reaper requeues a task only after its Job has failed or gone,
  never while the pod is alive.

What remains is narrow, e.g. two tasks that each index a repository with no index yet under the same
commit key. The prod duplicates show such an overlap did happen at some point, and nothing in the
schema prevents it. When it happened can't be told from the data.

The id in that message, a bare path, is not malformed: `lci-codegraph` emits one **file node** per file,
alongside `<file>#<line>:<name>` definitions. A single run never emits the same id twice (0 duplicates
when run over `ai-helm`), so a duplicate always means two writers.

Once a duplicate exists it blocks the constraint, and without the constraint nothing stops the next
overlap from adding another.

### Why it ranks third

New duplicates have to stop **before** the existing ones are repaired (Priority 4). Otherwise a repair
is undone by the next overlapping write.

### Before merging

| Severity | Finding | Verified | Fix |
|---|---|---|---|
| **P1** | If `snapshot_write_lock` fails to create at boot, `ensure_indexes` returns early. `main.rs` logs that only as "hybrid search disabled", nothing retries, and **the lock silently stops serializing**. That's the failure mode this PR exists to prevent. | **Yes**: the `?` in `ensure_indexes`; `main.rs` logs the error and continues | Declare the lock constraint separately from the other steps and report it as its own failure. Ideally a write shouldn't proceed unlocked without saying so. |
| P2 | The new function was inserted between `ensure_indexes` and its doc comment. `ensure_indexes` has lost its documentation, and the new function has picked it up. | **Yes** | Move the function above `ensure_indexes`' doc comment |
| P2 | The live test drops the shared `symbol_identity` constraint. That's only safe under `--test-threads=1`. | **Yes** (#665's test does the same) | Serialize the tests that change the schema, or restore it more defensively |

### Changes — `services/control-plane/src/integrations/neo4j.rs` (+139)

#### The lock, as the first statement of every `upsert_graph` transaction

```rust
let mut txn = graph.start_txn().await.context("begin neo4j txn")?;

// Writes to one snapshot run one at a time. `MERGE` only guarantees a single node per key when
// a uniqueness constraint backs it; without one, two transactions writing the same snapshot can
// both find a symbol absent and both create it. Merging the snapshot's lock node takes its
// unique-index entry, which Neo4j holds until this transaction ends — so a concurrent writer to
// the same snapshot waits here, then sees this one's committed nodes. Deleting the node in the
// same statement keeps the lock without leaving anything behind.
txn.run(
    query("MERGE (l:SnapshotWriteLock {repo_id: $repo, commit: $commit}) DELETE l")
        .param("repo", repository_id)
        .param("commit", commit_sha),
)
.await
.context("lock snapshot for writing")?;
```

The lock and the node and edge writes all run on the same `txn` and commit together, so this is a real
mutex scoped to one transaction. The key is `(repo_id, commit)`, so writers to different snapshots never
wait on each other. Each transaction takes exactly one lock, before anything else, so there can't be a
deadlock. Only `upsert_graph` takes the lock, because it's the only path that *creates* nodes.

#### The constraint that makes it exclusive

```rust
/// Declare the uniqueness constraint that makes [`upsert_graph`]'s snapshot lock exclusive.
///
/// A `MERGE` on the lock node serializes writers only because this constraint turns its key into a
/// unique-index entry that one transaction at a time can hold. Its label never keeps a node past a
/// write, so there are never existing duplicates for creation to trip on.
pub async fn ensure_snapshot_write_lock(graph: &Graph) -> anyhow::Result<()> {
    create_index_idempotent(
        graph,
        "CREATE CONSTRAINT snapshot_write_lock IF NOT EXISTS \
         FOR (l:SnapshotWriteLock) REQUIRE (l.repo_id, l.commit) IS UNIQUE",
        "snapshot_write_lock",
    )
    .await
}
```

and in `ensure_indexes`, just before the identity constraint:

```rust
ensure_snapshot_write_lock(graph).await?;
```

**This placement has a documented defect.** The function was inserted between `ensure_indexes`' doc
comment and `pub async fn ensure_indexes`. `ensure_indexes` has lost its documentation, and the new
function has picked it up. It's listed under “Before merging” above.

#### Why a Neo4j lock rather than the existing Postgres advisory lock

`tasks.rs` already uses `pg_advisory_xact_lock`. Reusing it would hold a pooled Postgres connection for
the whole Neo4j write, which can take minutes per page while the identity index is missing. A queue of
writers on one snapshot could then drain the pool that the webhook and API paths share. The Neo4j lock
covers exactly the transaction it protects and is released on commit, rollback or server-side abort.

#### Tests — `concurrent_writes_to_one_snapshot_create_each_symbol_once` (live) + helper `constraint_exists`

The test drops `symbol_identity` (so only the lock prevents duplicates), spawns four tasks that write
the same 1,500 nodes to one snapshot, and asserts 1,500 nodes with no lock node left behind. It restores
the constraint afterwards if it existed before.

**Mutation-verified:** with the lock statement replaced by a no-op, three runs out of three failed:

```text
left: 5884  right: 1500
left: 5934  right: 1500
left: 5782  right: 1500
```

Suite: **1,025 / 0 / 7**, 6/6 live, clippy + fmt clean.

---

### Merge notes

It conflicts with #663 in `neo4j.rs`. Both add a line after the fulltext index in `ensure_indexes`,
and both add a live test at the same spot. Keep both sides. **Deploy it before applying any #665
repair.**

---

## Priority 4 — The identity key is unindexed, and duplicates already exist → #665

**Branch** `feat/symbol-identity-repair` · **Head** `b24d286` · **Issue** #662 · **Status: 🔴 changes needed**

### The concern

On `main`, when `symbol_identity` is refused, the key `(repo_id, commit, node_id)` is left **with no
index at all**. Every node `MERGE` and both endpoint `MATCH`es of every edge then scan the whole
`:Symbol` label:

| Statement | No index | With index |
|---|---|---|
| node `MERGE` | 120,517 db accesses | 3 |
| edge write | 241,037 | 9 |

In prod this showed up as **~30 ms per lookup**. Cratestack (16,022 nodes, 28,887 edges) spent 81 s per
2,000-row page and timed out on edge page 5. CoopData timed out on its first edge page.

Two Neo4j behaviours, both verified, shape the fix:

1. It refuses the constraint while **any** key is duplicated.
2. It **also** refuses it while a plain index covers the same properties:
   ```text
   There already exists an index (:Symbol {repo_id, commit, node_id}).
   A constraint cannot be created until the index has been dropped.
   ```

### Why it ranks fourth

It restores index-speed lookups (the startup fallback does that as soon as it's deployed) and removes
the existing duplicates. The repair only stays repaired once Priority 3 is live.

### Before merging

| Severity | Finding | Verified | Fix / decision |
|---|---|---|---|
| **P1** | `promote()` drops the lookup index before the constraint's backing index is `ONLINE`. While it builds, **every repository's** lookups scan the label, not just the repaired snapshot's. The PR's risk section doesn't mention this. | **Yes**, by construction. The length of the window hasn't been measured. | Measure how long the build takes on a prod-sized label, and document it. Options: move the swap into its own explicit step (e.g. a separate flag) for a quiet period, or hold graph writes during it. |
| P2 | `repo:configure` is a flat permission check with no repository in it, so any holder can run a **database-wide** repair. | Matches how `Caller::require` works; the PR body already raises it | Decide: keep it, or add a platform-level scope |
| P2 | The live test drops the shared schema (same as #664) | **Yes** | As in #664 |

### Changes

#### `services/control-plane/src/integrations/neo4j/identity.rs` (new, +501)

This is a child module of `neo4j.rs` (the non-`mod.rs` layout), so no existing path changes.

```rust
const CONSTRAINT: &str = "symbol_identity";
const LOOKUP_INDEX: &str = "symbol_identity_lookup";
const REPAIR_BATCH_ROWS: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityIndex {
    /// `symbol_identity`: a unique-index seek, and `MERGE` cannot create a second node per key.
    Constraint,
    /// `symbol_identity_lookup`: the same seek without uniqueness, used while duplicates exist.
    LookupIndex,
}
```

**`declare`** runs at startup and never tears an index down:

```rust
pub async fn declare(graph: &Graph) -> anyhow::Result<IdentityIndex> {
    if exists(graph, "CONSTRAINTS", CONSTRAINT).await? {
        return Ok(IdentityIndex::Constraint);
    }
    if exists(graph, "INDEXES", LOOKUP_INDEX).await? {
        return Ok(IdentityIndex::LookupIndex);
    }
    match create_constraint(graph).await {
        Ok(()) => Ok(IdentityIndex::Constraint),
        Err(refused) => {
            tracing::warn!(
                error = %format!("{refused:#}"),
                "symbol identity constraint refused; serving the key from a lookup index until the \
                 duplicates are repaired"
            );
            create_lookup_index(graph).await?;
            Ok(IdentityIndex::LookupIndex)
        }
    }
}
```

If a lookup index already exists, that means the constraint was refused before. Dropping the index to
retry on every restart would leave the key unindexed while the replacement rebuilt over the whole label.
The lookup index gives the same seek. Measured: 4 database accesses for a node upsert against the
constraint's 3 (120,517 with no index), and 11 for an edge write against 9 (241,037 with no index).

**`promote`** is called only by an applied repair:

```rust
async fn promote(graph: &Graph) -> anyhow::Result<IdentityIndex> {
    if exists(graph, "CONSTRAINTS", CONSTRAINT).await? {
        return Ok(IdentityIndex::Constraint);
    }
    graph.run(query(&format!("DROP INDEX {LOOKUP_INDEX} IF EXISTS"))).await
        .context("drop identity lookup index")?;
    match create_constraint(graph).await {
        Ok(()) => Ok(IdentityIndex::Constraint),
        Err(refused) => {
            tracing::warn!(/* … */ "symbol identity constraint still refused after repair; restoring the lookup index");
            create_lookup_index(graph).await?;
            Ok(IdentityIndex::LookupIndex)
        }
    }
}
```

The lookup index has to be dropped first, because the two can't coexist. **This opens a window across
the whole database** while the constraint's backing index builds (the P1 under “Before merging” above).

**`create_constraint` / `create_lookup_index`** are thin wrappers over `create_index_idempotent`
(`… REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE` and `CREATE INDEX symbol_identity_lookup … ON (…)`).
**`exists(kind, name)`** runs `SHOW {kind} YIELD name WHERE name = $name`. `kind` is a fixed keyword,
never input from the caller.

**Report types:**

```rust
pub struct DuplicatedSnapshot {
    pub repo_id: i64,
    pub commit: String,
    pub duplicated_symbols: i64, // keys with more than one node
    pub extra_nodes: i64,        // nodes beyond the first — what a repair removes
}

pub struct RepairReport {
    pub applied: bool,
    pub snapshots: Vec<DuplicatedSnapshot>,
    pub extra_nodes: i64,
    pub identity: IdentityIndex,
}
```

**`repair(graph, apply)`** finds the duplicated snapshots. If `apply` is set, it collapses each one and
then calls `promote`. Otherwise it calls `declare` and reports what it found.

**`duplicated_snapshots`** works one snapshot at a time. A database-wide aggregation would hold one
entry per stored symbol on the heap, which is exactly what #662 was about.

```cypher
MATCH (s:Symbol) RETURN DISTINCT s.repo_id AS repo, s.commit AS commit
-- then, per snapshot:
MATCH (s:Symbol {repo_id: $repo, commit: $commit})
WITH s.node_id AS id, count(*) AS copies WHERE copies > 1
RETURN count(id) AS symbols, coalesce(sum(copies - 1), 0) AS extra
```

**`collapse_snapshot`** removes duplicates in place. The survivor for each key is the copy with the
lowest `elementId`.

Step 1 moves every edge that touches a surplus copy onto the survivors:

```cypher
UNWIND $ids AS id
MATCH (:Symbol {repo_id: $repo, commit: $commit, node_id: id})-[r:REL]-()
WITH DISTINCT r
CALL (r) {
  WITH r, startNode(r) AS a, endNode(r) AS b
  MATCH (ka:Symbol {repo_id: $repo, commit: $commit, node_id: a.node_id})
  WITH r, a, b, min(elementId(ka)) AS ka_id
  MATCH (kb:Symbol {repo_id: $repo, commit: $commit, node_id: b.node_id})
  WITH r, a, b, ka_id, min(elementId(kb)) AS kb_id
  WHERE ka_id <> elementId(a) OR kb_id <> elementId(b)
  MATCH (ka:Symbol) WHERE elementId(ka) = ka_id
  MATCH (kb:Symbol) WHERE elementId(kb) = kb_id
  MERGE (ka)-[:REL {relation: r.relation}]->(kb)
  DELETE r
} IN TRANSACTIONS OF 1000 ROWS
```

Step 2 removes the surplus copies, passing any embedding to the survivor:

```cypher
UNWIND $ids AS id
MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: id})
WITH id, s ORDER BY elementId(s)
WITH id, collect(s) AS copies
WITH head(copies) AS keep, tail(copies) AS extras
UNWIND extras AS extra
CALL (keep, extra) {
  SET keep.embedding = coalesce(keep.embedding, extra.embedding)
  DETACH DELETE extra
} IN TRANSACTIONS OF 1000 ROWS
RETURN count(*) AS removed
```

- Concurrent writers join every copy of one endpoint to every copy of the other. `MERGE` collapses
  that fan-out back to one edge per relation.
- Both steps must choose the same survivor: `min(...)` in step 1, `ORDER BY … head` in step 2.
  Mutation M3 shows what breaks if they don't.
- `coalesce` only fills in an embedding when the survivor has none. It never overwrites an existing one.

**Why repair in place, and not drop-and-re-index:** the race is routine, so many repositories may be
affected. Re-indexing each would re-embed it in full (about 17k chunks for cratestack alone) against a
database that is already under pressure.

#### `services/control-plane/src/integrations/neo4j.rs` (+40 / −25)

```rust
pub(crate) mod identity;
```

In `ensure_indexes`, the old block logged a refusal and carried on with the key unindexed:

```rust
if let Err(error) = create_index_idempotent(graph, "CREATE CONSTRAINT symbol_identity …", "symbol_identity").await {
    tracing::warn!(?error, "symbol identity constraint not created; symbol lookups will scan the label");
}
```

The new block always ends with the key indexed:

```rust
match identity::declare(graph).await? {
    identity::IdentityIndex::Constraint => {
        tracing::info!("symbol identity served by its uniqueness constraint");
    }
    identity::IdentityIndex::LookupIndex => {
        tracing::warn!(
            "symbol identity served by a lookup index: duplicated symbols prevent the \
             uniqueness constraint; POST /admin/graph/symbol-identity/repair resolves them"
        );
    }
}
```

The comment above it no longer lists `prune_graph` among the triple lookups, because it selects on
only two of the three properties.

#### `services/control-plane/src/http/admin.rs` (+32)

```rust
#[derive(Debug, Deserialize)]
pub struct SymbolIdentityRepairQuery {
    /// Collapse the duplicates found. Without it the endpoint only reports them.
    #[serde(default)]
    apply: bool,
}

/// `POST /admin/graph/symbol-identity/repair?apply=` — report every graph snapshot that holds a
/// symbol more than once and, with `apply=true`, reduce each to one node per key and move the
/// identity key onto its uniqueness constraint. …
pub async fn repair_symbol_identity(
    caller: Caller,
    State(state): State<AppState>,
    Query(q): Query<SymbolIdentityRepairQuery>,
) -> Response {
    if let Err(e) = caller.require("repo:configure") {
        return e.into_response();
    }
    let Some(neo4j) = state.neo4j.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "neo4j not configured").into_response();
    };
    match crate::integrations::neo4j::identity::repair(neo4j, q.apply).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => { /* log, 500 "repair failed" */ }
    }
}
```

By default it's a dry run. `apply` is a `bool`, used for control flow and never interpolated into a query.

#### `services/control-plane/src/main.rs` (+4)

```rust
.route(
    "/admin/graph/symbol-identity/repair",
    post(admin::repair_symbol_identity),
)
```

### Tests — `repair_collapses_duplicates_and_keeps_every_edge_and_embedding` (live)

The fixture is what concurrent writers leave behind:

- `a` ×2, `b` ×3, `c` ×1
- every copy of an endpoint joined to every copy of the other
- `b` calling itself
- **an edge and the only embedding held by a surplus copy of `a`**, i.e. written before the other copy existed
- a clean second snapshot of the same repository

Assertions:

- **Dry run:** reports exactly `{main, 2 duplicated, 3 extra}`, changes nothing, and serves the key from the lookup index.
- **Apply:** leaves one node per key and exactly the edges `a -calls-> b`, `a -contains-> c`,
  `b -calls-> b`, `b -calls-> c`. The embedding ends up on the survivor, the other snapshot is untouched,
  the constraint is created and the lookup index is gone.
- **Repeat:** finds nothing to do.

**Mutation-verified:**

| Mutation | Result |
|---|---|
| M1: edge moves disabled | fails, because the surplus copy's only edge is lost |
| M2: embedding not carried over | fails |
| M3: the two steps pick different survivors | fails |

M1 only fails because the fixture includes an edge that *only* a surplus copy holds. With nothing but the
every-copy-to-every-copy fan-out, the survivor already has every edge and the broken step would pass.

A real control-plane run with a duplicate present showed both startup warnings, and the route answered
503 (registered, and OIDC not configured locally) against 404 for an unknown path.

Suite: **1,025 / 0 / 7**, 6/6 live, clippy + fmt clean.

---

### Merge notes

Clean on top of #663. With #664 also merged, it makes an adjacent edit around the identity block; keep
both sides. **Operator order after deploy:** check the startup log, confirm with `SHOW INDEXES` that
`symbol_identity_lookup` is `ONLINE`, run the dry run, then `?apply=true` during low indexing activity
and only once #664 is live (see Priority 8).

---

## Priority 5 — A failed re-index deletes a graph it didn't write → #666

**Branch** `feat/scoped-graph-discard` · **Head** `8b36e13` · **Issue** #656 · Cherry-picked from
`a87c084` · **Status: 🔴 changes needed; conflicts with `main`**

### The concern

#657 (merged) made the structural graph submit in pages of 2,000, with a rollback: if a later page fails,
`DELETE /internal/tasks/{id}/graph` removes the snapshot so no partial graph is left behind. That delete
is keyed on `(repo_id, commit)`, and a commit can already hold a *complete* graph from an earlier run. A
re-index at the same key is a routine, allowed task. So a re-index that fails partway **deletes the
previous good graph**, where the single-transaction submit before it would have rolled back and left the
graph alone.

The fix was written and reviewed on #657 as `a87c084`, but it was pushed after the branch was squashed,
so it never reached `main`. The #662 thread shows this same delete running against governance's `main`
snapshot.

### Why it ranks fifth

It prevents data loss on failed re-indexes, but only when a submit fails. Priorities 1–4 make that much
less likely. It also has a real bug to fix first, and it's the only one of these PRs besides #658 that
touches the runner.

### Before merging

| Severity | Finding | Verified | Fix |
|---|---|---|---|
| **P1 (bug)** | `graph_snapshot` gets its commit from `task_scope`, which returns the repository's **latest indexed** commit, not this task's commit. When indexing commit B while A is still the latest indexed, it counts A's graph, `may_discard` returns `false`, and **B's partial graph is never removed**. | **Yes**: `task_scope` prefers `latest_indexed_commit`; `discard_graph` uses `head_sha.unwrap_or(default_branch)` | Resolve the commit the way `discard_graph` does (and #658's `indexed_chunks`); a shared `task_commit` helper would serve all three. Add a test that goes through the endpoint with a task whose commit differs from the latest indexed one. |
| P2 | `graph_node_count` was inserted between `discard_graph` and its doc comment. `discard_graph` has lost its documentation. | Same insertion pattern as #664 | Move the doc comment back onto `discard_graph` |
| P2 | A partial graph left by an **earlier** failed run of the same commit reads as `Some(n)` and is never cleaned up | Documented trade-off (ADR-0117) | Accept, or track which pages each run wrote |
| — | **Conflicts with `main`** in `docs/adr/README.md`: #649 added the ADR-0118 row next to the ADR-0117 row this PR rewrites | **Yes** (trial merge) | Rebase and keep both rows |

### Changes, file by file

#### `services/agent-runner/src/indexer/graph.rs` — the decision

```rust
/// Whether a failed page sequence may discard the commit's snapshot.
///
/// Pages commit individually, so an interrupted sequence leaves the ones that already landed.
/// Removing them is right only when this run created them: a snapshot that predates the run is a
/// complete graph for this commit, and replacing it is this run's job where destroying it is not.
/// An unknown prior state (`None`) is treated as pre-existing — leaving a graph in place is
/// recoverable on the next run, deleting one is not.
#[must_use]
fn may_discard(symbols_before: Option<u64>) -> bool {
    matches!(symbols_before, Some(0))
}
```

In `index_graph`, before the change, the discard was unconditional:

```rust
if let Err(error) = client.submit_graph_paged(/* … */).await {
    if let Err(discard) = client.discard_graph(context.task_id).await { /* warn */ }
    return Err(error).context("submitting codegraph structural graph");
}
```

After the change:

```rust
// Read before the first page lands; afterwards the count reflects this run's own writes.
let symbols_before = match client.graph_node_count(context.task_id).await {
    Ok(count) => Some(count),
    Err(error) => {
        tracing::warn!(
            error = %format!("{error:#}"),
            "graph snapshot lookup failed; a failed submit will leave the snapshot in place"
        );
        None
    }
};
if let Err(error) = client
    .submit_graph_paged(context.task_id, &commit_sha, &nodes, &edges, page_size)
    .await
{
    // Discarding leaves the commit un-indexed, which readers handle, rather than a subset that
    // reads as a complete graph — a missing edge is indistinguishable from a symbol that
    // genuinely has no callers. That trade only applies to a snapshot this run created.
    if may_discard(symbols_before) {
        if let Err(discard) = client.discard_graph(context.task_id).await {
            tracing::warn!(/* … */ "discarding the partial graph failed; the snapshot may hold an incomplete graph");
        }
    } else {
        tracing::warn!(
            symbols_before = ?symbols_before,
            "graph submit failed over a snapshot that predates this run; leaving it in place"
        );
    }
    return Err(error).context("submitting codegraph structural graph");
}
```

| Snapshot before page 1 | On failure |
|---|---|
| `Some(0)`: first index | discard, because every page came from this run |
| `Some(n)`: an earlier graph exists | keep it |
| `None`: the size read failed | keep it |

#### `services/control-plane/src/http/internal.rs` — `GET /internal/tasks/{id}/graph`

```rust
/// `GET /internal/tasks/{id}/graph` — how many symbols this task's commit snapshot already holds.
pub async fn graph_snapshot(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    /* … 503 without db / neo4j … */
    let (repository_id, commit_sha) = match task_scope(pool, id).await {   // ← the P1
        Ok(Some(scope)) => scope,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => { /* 500 */ }
    };
    match crate::integrations::neo4j::count_commit_symbols(neo4j, repository_id, &commit_sha).await {
        Ok(nodes) => Json(serde_json::json!({ "nodes": nodes })).into_response(),
        Err(error) => { /* 500 */ }
    }
}
```

**This is wrong: stephane's P1, verified.** `task_scope` resolves the repository's **latest indexed**
commit, and only falls back to the task's own commit when nothing has been indexed yet:

```rust
async fn task_scope(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<(i64, String)>, sqlx::Error> {
    let Some(ctx) = crate::db::get_task_context(pool, id).await? else { return Ok(None); };
    let commit = match crate::db::latest_indexed_commit(pool, ctx.repository_id).await? {
        Some(c) => c,
        None => retrieval_commit(ctx.head_sha.as_deref(), &ctx.default_branch),
    };
    Ok(Some((ctx.repository_id, commit)))
}
```

That's the right behaviour for retrieval (`search`, `graph_query`). It's the wrong one here:
`ingest_graph` and `discard_graph` both resolve **this task's** `head_sha`/`default_branch`. When an
index task targets commit B while A is still the latest indexed, the size check counts A's snapshot,
say 5,000 nodes. Then `may_discard(Some(5000))` returns `false`, and B's partial graph is never removed.
**The fix is to resolve the commit the same way `discard_graph` does.** No test goes through
`graph_snapshot` with a task whose commit differs from the latest indexed one.

#### `services/control-plane/src/integrations/neo4j.rs` — `count_commit_symbols`

```rust
/// How many symbols one commit snapshot holds for a repository.
pub async fn count_commit_symbols(graph: &Graph, repository_id: i64, commit_sha: &str) -> anyhow::Result<u64> {
    let mut result = graph
        .execute(
            query("MATCH (s:Symbol {repo_id: $repo, commit: $commit}) RETURN count(s) AS n")
                .param("repo", repository_id)
                .param("commit", commit_sha),
        )
        .await
        .context("count commit symbols")?;
    /* … read n, clamp ≥ 0 … */
}
```

Plus a live test, `counting_a_snapshot_separates_a_first_index_from_a_re_index`, that checks the count
goes 0 → 4 and is scoped to the commit. It calls `count_commit_symbols` directly, so it doesn't catch
the P1.

#### `services/control-plane/src/main.rs`

```rust
post(internal::ingest_graph)
    .get(internal::graph_snapshot)
    .delete(internal::discard_graph)
```

#### `services/agent-clients/src/control_plane/indexing.rs`

```rust
/// Body of `GET /internal/tasks/{id}/graph`.
#[derive(Debug, Deserialize)]
struct GraphSnapshot { nodes: u64 }

pub async fn graph_node_count(&self, task_id: Uuid) -> anyhow::Result<u64> {
    let url = format!("{}/internal/tasks/{task_id}/graph", self.base_url);
    let body: GraphSnapshot = self.http.get(&url).bearer_auth(&self.token)
        .send().await.context("reading the graph snapshot")?
        .error_for_status().context("control plane rejected the graph snapshot read")?
        .json().await.context("parsing the graph snapshot")?;
    Ok(body.nodes)
}
```

The import widens to `use serde::{Deserialize, Serialize};`. **Doc defect (bot P2):** the method was
inserted after `discard_graph`'s doc comment, so that `DELETE` doc now attaches to `graph_node_count`
and `discard_graph` has no documentation. This is the same slip as in #664.

#### `services/agent-clients/tests/control_plane_contract.rs`

`graph_node_count_reads_the_task_snapshot_size`: a wiremock `GET` returns `{"nodes": 5154}` and the test
asserts `5154`.

#### Docs: `ROADMAP.md`, `docs/adr/0117-…md`, `docs/adr/README.md`

- **ADR-0117:** the rollback section is retitled *"leaves behind nothing it created"*. It now shows
  `may_discard`, explains the first-index vs re-index behaviour, and adds a consequence (one extra round
  trip per submit) and an out-of-scope item (per-node provenance).
- **ADR index row and roadmap entry:** reworded to match.
- **The ADR index row now conflicts with `main`,** because #649 added the ADR-0118 row right next to it.

### Tests

The predicate test is **mutation-verified**: an always-true `may_discard` fails it. Suite **1,027 / 0 / 7**,
6/6 live, clippy + fmt clean.

---

### Merge notes

Besides the `main` conflict, it conflicts with #663 in `neo4j.rs`, where both add code directly above
`delete_commit_graph`; keep both sides. **The agent image has to be rebuilt** for the runner half to
take effect.

---

## Priority 6 — Index state is invisible → #659

**Branch** `feat/neo4j-index-health` · **Head** `3793f41` · Based on `5c936ab` ·
**Status: 🔴 rework needed**

### The concern

A `CREATE INDEX` or `CREATE CONSTRAINT` that returns `Ok` doesn't mean the planner will use it. The
index can still be `POPULATING`, or the constraint can be refused and skipped. Both look exactly like
a busy database. The only record that the constraint had been refused was one `WARN` at control-plane
startup, in a different pod, possibly days earlier. The runner's error only said
`operation timed out`, so diagnosis came down to guessing, and one guess (a `145,029` db-hit figure)
didn't reproduce.

### Why it ranks last among the PRs

It adds observability and changes no behaviour. It also has to know about the indexes introduced by
#663, #664 and #665, so it has to be reworked on top of them.

### Before merging

| Severity | Finding | Verified | Fix |
|---|---|---|---|
| **P1** | `report_index_states` never runs when vector or fulltext index creation fails, because `ensure_indexes` returns early via `?`. That's the failure this PR is meant to expose. | **Yes** | Report states on every exit path |
| **Cross-PR** | After #665, the key may be served by `symbol_identity_lookup`. This PR would report `symbol_identity ABSENT` and warn on **every write**. | **Yes**, by reading both diffs | Treat either index as serving the key; add `symbol_snapshot`, `snapshot_write_lock` and `symbol_identity_lookup` to `MANAGED_INDEXES` |
| P2 | The gauge uses the state as a label, which leaves stale series behind. And because of the latch, the state is never re-read once `ONLINE`, so a later drop isn't seen. | **Yes** | Put the state in the value, or clear old series; reconsider the latch |
| P2 | The `ONLINE → 1 / else → 0` mapping has no test | **Yes** | Assert on the rendered gauge |

### Changes, file by file

#### `services/control-plane/src/integrations/neo4j.rs`

The import becomes `use std::collections::{BTreeMap, HashMap};`, and these are added:

```rust
pub const MANAGED_INDEXES: [&str; 3] = ["symbol_embedding_idx", "symbol_label_fulltext", "symbol_identity"];
pub const IDENTITY_INDEX: &str = "symbol_identity";
pub const ABSENT: &str = "ABSENT";

#[must_use]
pub fn is_usable(state: &str) -> bool {
    state.eq_ignore_ascii_case("ONLINE")
}

/// State of each managed index as Neo4j reports it: `ONLINE`, `POPULATING`, `FAILED`, or [`ABSENT`].
pub async fn index_states(graph: &Graph) -> anyhow::Result<BTreeMap<String, String>> {
    let mut states: BTreeMap<String, String> = MANAGED_INDEXES
        .iter()
        .map(|name| ((*name).to_string(), ABSENT.to_string()))
        .collect();
    let mut result = graph
        .execute(query("SHOW INDEXES YIELD name, state RETURN name, state"))
        .await
        .context("show indexes")?;
    while let Some(row) = result.next().await.context("read index row")? {
        /* overwrite the slot for any managed index Neo4j lists */
    }
    Ok(states)
}
```

The map starts with every managed index set to `ABSENT`, so an index that doesn't exist is reported as
absent instead of being left out.

```rust
/// The identity index's state when it is *not* usable, or `None` when it is.
pub async fn identity_index_unready(graph: &Graph, cached: &AtomicBool) -> Option<String> {
    if cached.load(Ordering::Relaxed) {
        return None;
    }
    let state = match index_states(graph).await {
        Ok(states) => states.get(IDENTITY_INDEX).cloned().unwrap_or_else(|| ABSENT.to_string()),
        // An unreadable state is not evidence of an unusable index; say nothing rather than
        // attribute a slow write to a cause that was never established.
        Err(_) => return None,
    };
    crate::http::metrics::neo4j_index_state(IDENTITY_INDEX, &state);
    if is_usable(&state) {
        cached.store(true, Ordering::Relaxed);
        None
    } else {
        Some(state)
    }
}
```

At the end of `ensure_indexes`, the refusal warning now uses `{error:#}`, and a new
`report_index_states(graph)` runs. It logs every managed index at `INFO` when `ONLINE` and at `WARN`
otherwise (with a specific message for the identity index), and sets the gauge.

#### `services/control-plane/src/http/metrics.rs`

```rust
use metrics::{counter, gauge, histogram};

pub fn neo4j_index_state(name: &str, state: &str) {
    gauge!(
        "lci_neo4j_index_ready",
        "index" => name.to_string(),
        "state" => state.to_string(),
    )
    .set(if state.eq_ignore_ascii_case("ONLINE") { 1.0 } else { 0.0 });
}
```

#### `services/control-plane/src/http/internal.rs` — in `ingest_graph`

```rust
// Name the cause while the effect is happening. …
if let Some(index_state) =
    crate::integrations::neo4j::identity_index_unready(neo4j, &state.identity_index_ready).await
{
    tracing::warn!(
        task_id = %id,
        index = crate::integrations::neo4j::IDENTITY_INDEX,
        state = %index_state,
        nodes = batch.nodes.len(),
        edges = batch.edges.len(),
        "writing a graph page while the identity index is unusable; every symbol lookup is a \
         label scan, so this write is slow in proportion to all symbols stored"
    );
}
```

This only logs. The write goes ahead either way. The reviewer confirmed it has no effect on behaviour.

#### `services/control-plane/src/main.rs`

```rust
/// Whether the Neo4j identity index has been observed `ONLINE`. Latched: once true it stays
/// true, so the graph-write path re-reads the state only while the index is unusable.
pub identity_index_ready: Arc<std::sync::atomic::AtomicBool>,
// …
identity_index_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
```

#### `http/admin.rs` (×2), `http/webhook.rs` (×2), `mcp/auth.rs` (×1)

Test-only `AppState` constructors get `identity_index_ready: Default::default(),`. The reviewer
verified all five.

### Tests

- `only_an_online_index_counts_as_usable` (unit).
- `index_states_reports_absent_then_online_for_the_identity_index` (live): drops the constraint, asserts
  `ABSENT`, asserts no latch, creates the constraint, asserts `ONLINE`, asserts the latch, then restores
  the prior state.
- A real control-plane run captured both the healthy and duplicate-blocked startup logs, plus the
  `/metrics` output.

Suite: **1,026 / 0 / 7**.

---

### Merge notes

It conflicts with the merged stack at the tail of `ensure_indexes`. Rebase it after #665 and keep both
sides.

---

## Priority 7 — `ensure_indexes` stops at its first failure (follow-up PR)

### The concern

Once #663–#665 are merged, `ensure_indexes` declares, in this order: the vector index, the fulltext
index, `symbol_snapshot`, `snapshot_write_lock`, and then the identity index via `identity::declare`.
Every step returns early with `?`. A failure at any step skips all the steps after it, and `main.rs`
logs that only as "hybrid search disabled".

The P1s on #664 (lock silently absent) and #659 (report unreachable) are two cases of this one problem.

### What to do

Once the stack is merged, attempt each declaration separately, report each outcome (through #659's
report), and treat a missing `snapshot_write_lock` as serious, since without it writes are not
serialized. Doing this once on top of the merged stack is cleaner than patching each PR separately.

---

## Priority 8 — Rollout, repair and cleanup in prod (operations)

In order, after the PRs are deployed:

1. **Neo4j restarted, heap sized**: ADORSYS-GIS/ai-helm#1130. A probe that detects a failed database
   is also still needed.
2. **Deploy the control plane with #663–#665.** Expect `symbol identity served by a lookup index` at
   startup. Check `SHOW INDEXES YIELD name, state`: `symbol_identity_lookup` and `symbol_snapshot`
   should be `ONLINE`. They build online, which takes a while on a large label.
3. **Dry-run the repair:** `POST /api/v2/admin/graph/symbol-identity/repair`, then review the snapshot
   list.
4. **Apply it** with `?apply=true`, only once #664 is live and indexing is quiet. It should report
   `"identity": "constraint"`. Keep the index-swap window from Priority 4 in mind.
5. **Roll the agent image** for #658 and #666.
6. **Clean up partial graphs the repair doesn't cover.** It removes duplicates, not incomplete
   snapshots. That covers cratestack's `main` from task `9df34a5f` (where the discard returned 500),
   and anything whose rollback failed. Then re-index those repositories, plus the ones whose graphs were
   discarded during the incident (e.g. `lightbridge-governance`).
7. **Close #662 by hand.** None of the PR bodies uses a closing keyword.

---

## Priority 9 — Deferred hardening (not scheduled)

- **Ingest page size:** a server-side limit. The runner pages at 2,000 rows, but the server trusts whatever it's sent.
- **Unused `embedding` field:** remove it from the structural ingest body. An outdated client could still send 2,000 × 4096-dimension vectors (≈ 64 MB of raw doubles) in one transaction.
- **Transaction timeouts:** set per transaction from the control plane (neo4rs support unverified).
- **Failed-database signal:** a distinct counter for `TransactionStartFailed`, and short-circuiting to 503 instead of attempting writes.
- **Cross-snapshot write limit:** cap concurrent graph writes *across* snapshots. #664 only serializes writes within one snapshot.
- **Default-branch snapshot keys:** key them by the resolved SHA instead of the branch name. That makes overlapping writers much rarer, and stops `main` snapshots from accumulating symbols that no longer exist.

---

## Appendix A — Merge conflicts, measured

**Each PR on its own against `main` (`49ee422`):** #659, #663, #664 and #665 are clean. **#666
conflicts** in `docs/adr/README.md`. **#658 conflicted** in `internal.rs` and the client contract tests
(it predated #657); it has since been merged with `main` (`5bd8bdf`) at `1dcad49`, keeping both sides.

**Stacked, in the order above (trial merges in a scratch worktree):**

| Step | Result | Where |
|---|---|---|
| + #663 | clean | |
| + #658 | clean (no shared files) | |
| + #664 | conflict | `neo4j.rs`: `ensure_indexes` after the fulltext index, and the test module |
| + #665 | clean on #663; next to #664's edit | `neo4j.rs`: the identity block |
| + #666 | conflict | `neo4j.rs`, above `delete_commit_graph`; plus `docs/adr/README.md` against `main` |
| + #659 | conflict | `neo4j.rs`: the tail of `ensure_indexes` |

Every conflict is two PRs adding code at the same spot, so the resolution is to **keep both sides**.
None of them is a logic conflict.

## Appendix B — Design decisions left to reviewers

| PR | Decision | Choice made | Alternative |
|---|---|---|---|
| #663 | Batch size | constant 1,000 | configurable |
| #664 | Where the lock lives | a Neo4j lock node | a Postgres advisory lock (holds a pooled PG connection per waiting writer) |
| #665 | Repair strategy | in place, graph only | drop and re-index (re-embeds every affected repo) |
| #665 | Permission scope | `repo:configure` | a new platform-level scope |
| #665 | Index-swap window | drop, then create | a separate explicit step, or writes held during the swap |
| #666 | Unknown prior state | keep the snapshot | discard it (a stronger "no partial graph" guarantee, but it can delete a complete graph) |
| #659 | Gauge shape | state as a label | state as the value |

## Appendix C — Process

- **Approvals:** #658 and #663 are approved; the other four have none.
- **Human-verification boxes:** the AI-usage boxes are unticked on all six, deliberately. They're for the author.
- **CI:** I couldn't read CI results with the tools available. The reviewer reports "all checks green" on #658 and #663.
- **Before each merge:** re-pull the PR's own bot comments (CLAUDE.md).
- **Recurring slip:** the misplaced doc comments in #657, #658, #664 and #666 all come from inserting new code directly above an existing function, which separated that function from its doc comment.
