# PR #672 — Keying an index by the commit it was taken from

**PR:** [#672](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/672) · **Issue:** [#669](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/669)
**Branch:** `feat/snapshot-keyed-by-indexed-commit` · **Commit:** `b128594` · base: remote `main` `0aa4ca5`
**Size:** 9 files, +223 / −16

---

## 1. What a snapshot is, and what went wrong

A **snapshot** is everything stored under one `(repository_id, commit_sha)` key, across both stores:

| Store | Table / label | Key |
|---|---|---|
| pgvector | `code_chunks` | `(repository_id, commit_sha, file_path, start_line, end_line)` |
| Neo4j | `:Symbol` nodes | `(repo_id, commit, node_id)` |

There is no `Snapshot` type in the code. The snapshot *is* that key, and both halves must agree on it
or retrieval reads one half of one version and one half of another.

The write key was derived independently in each half, from the task:

```rust
// services/agent-runner/src/indexer/graph.rs  (and mod.rs, identically) — BEFORE
let commit_sha = context
    .head_sha
    .as_deref()
    .unwrap_or(&context.default_branch)
    .to_string();
```

An **index** task carries no commit — `create_index_task` inserts `head_sha` as `NULL` — so the
`unwrap_or` fired on every default-branch index and the key became the literal string `"main"`.

Three properties then compound:

1. **Writes merge, they never remove.** Postgres uses `ON CONFLICT (…) DO UPDATE`, Neo4j uses
   `MERGE … SET`. A symbol that moved to a new line is written at its new position; the copy at the
   old position is untouched and stays forever.
2. **`'main'` is always the latest snapshot**, so the sweeper's keep-set always contains it and it is
   never pruned.
3. **Every push writes into that same snapshot**, so it is the union of every version of the code
   ever indexed.

Measured by replaying this repository's last 30 pushes into one snapshot (the #669 experiment):

| | Stored | Actually in the code | Stale |
|---|---|---|---|
| `:Symbol` nodes | 6,032 | 4,925 | 1,107 (18%) |
| edges | 12,954 | 9,566 | 3,388 (the difference) |

Of the 1,107 stale symbols, **1,023 are line-shifted copies** — the same definition recorded at a
line it has since moved away from. Production confirms the shape: grouping `:Symbol` by `(repo_id, commit)` returns
`main` for the two repositories with a large index — 6,192 nodes for repository `7910` and 3,687 for
`8349` — and a real sha only for repository `10752`, whose 463 nodes were last written by a review
pinned to `850f91df…`.

---

## 2. What the PR changes

### 2.1 The runner resolves the commit from the tree it indexed

New in `services/agent-runner/src/clone.rs:226`:

```rust
/// The commit a checkout is at.
///
/// What an index describes, and the key it is stored under: a branch name moves, so a snapshot named
/// after one is a different tree each time it is written.
pub async fn head_commit(dir: &Path) -> anyhow::Result<String> {
    let out = git(dir, &["rev-parse", "HEAD"], "").await?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!("git rev-parse HEAD returned nothing for {}", dir.display());
    }
    Ok(sha)
}
```

`perform_indexing` resolves it **once** and hands the same value to both halves
(`services/agent-runner/src/run.rs:402`):

```rust
let out = indexer::walk(checkout).await?;
// The commit both halves of the index are stored under, read from the tree they describe.
let commit_sha = clone::head_commit(checkout).await?;

let graph_result = indexer::graph::index_graph(context, &commit_sha, &out, client).await;
// …
let chunk_count = indexer::index_chunks(context, &commit_sha, &out, client, embedder).await?;
```

`index_graph` and `index_chunks` each lost their derivation block and gained a `commit_sha: &str`
parameter. The two halves can no longer disagree, because neither decides any more.

### 2.2 A run records the commit it is writing under

New in `services/control-plane/src/db/tasks.rs:775`:

```rust
/// Record the commit a run is writing its index under.
///
/// Called with the first batch of a run, so the snapshot it is building can be marked complete —
/// and become readable — when that run succeeds.
pub async fn record_indexed_commit(
    pool: &PgPool,
    id: Uuid,
    commit_sha: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE tasks SET indexed_sha = $2 WHERE id = $1 AND indexed_sha IS DISTINCT FROM $2",
    )
    /* … */
}
```

Called from both ingest paths (`http/internal.rs:718` chunks, `:903` graph), best-effort:

```rust
if let Err(error) = crate::db::record_indexed_commit(pool, id, &batch.commit_sha).await {
    tracing::warn!(%error, task_id = %id, "recording the indexed commit failed");
}
```

### 2.3 The snapshot becomes readable when the run succeeds

Inside `set_task_status`'s **existing** transaction, before `tx.commit()`
(`services/control-plane/src/db/tasks.rs:826`):

```rust
// A snapshot becomes readable when the run that wrote it succeeds, in the same transaction as
// that status: retrieval pins to completed snapshots, so a half-written one is never served.
if status == "succeeded" {
    sqlx::query(
        "INSERT INTO index_snapshots (repository_id, commit_sha) \
         SELECT repository_id, indexed_sha FROM tasks WHERE id = $1 AND indexed_sha IS NOT NULL \
         ON CONFLICT (repository_id, commit_sha) DO UPDATE SET completed_at = now()",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
}
```

Being in that transaction is the point: the status and the snapshot's visibility cannot disagree, and
a rollback takes both.

### 2.4 Retrieval pins to a completed snapshot

`latest_indexed_commit` (`services/control-plane/src/db/code_chunks.rs:20`) is now two sequential
queries:

```rust
// A snapshot being written has no `index_snapshots` row, so retrieval stays on the last one that
// finished. Backed by `index_snapshots_latest_idx`, so this is an index lookup, not a scan — it runs
// on every search/graph query via `task_scope`.
let completed: Option<String> = sqlx::query_scalar(
    "SELECT commit_sha FROM index_snapshots WHERE repository_id = $1 \
     ORDER BY completed_at DESC LIMIT 1",
).bind(repository_id).fetch_optional(pool).await?;
if completed.is_some() {
    return Ok(completed);
}

// Repositories indexed before snapshots were recorded have no such row, and their newest rows
// are the whole index. `id DESC` tie-breaks when two snapshots share a `created_at` …
sqlx::query_scalar(
    "SELECT commit_sha FROM code_chunks WHERE repository_id = $1 \
     ORDER BY created_at DESC, id DESC LIMIT 1",
).bind(repository_id).fetch_optional(pool).await
```

Two queries rather than one `UNION ALL`: Postgres rejects `ORDER BY … LIMIT` directly before
`UNION ALL` (`42601`).

### 2.5 Migration `0042_index_snapshots.sql`

```sql
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS indexed_sha TEXT;

CREATE TABLE IF NOT EXISTS index_snapshots (
    repository_id BIGINT      NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    commit_sha    TEXT        NOT NULL,
    completed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, commit_sha)
);

CREATE INDEX IF NOT EXISTS index_snapshots_latest_idx
    ON index_snapshots (repository_id, completed_at DESC);
```

Note: this is effectively the readiness table ADR-0055's follow-up reserved `repo_index` for —
`repo_index` still has no writer, and this PR does not change that.

---

## 3. Old flow vs new flow

### Write

| Step | Before | After |
|---|---|---|
| Index task created | `head_sha = NULL` | unchanged |
| Runner clones default branch | — | — |
| Key derived | `head_sha.unwrap_or(default_branch)` → `"main"`, twice, independently | `git rev-parse HEAD` → `a1b2c3…`, once, shared |
| Graph written | `MERGE` into `(repo, 'main')` — merges with everything already there | `MERGE` into `(repo, a1b2c3…)` — a key nothing has written |
| Chunks written | `DO UPDATE` into `(repo, 'main')` | into `(repo, a1b2c3…)` |
| Commit recorded | — | `tasks.indexed_sha = a1b2c3…` on the first batch |
| Task succeeds | status only | status **+** `index_snapshots` row, one transaction |
| Next push | writes into the same `'main'` snapshot again | writes a new snapshot; the previous one stops changing |

### Read

| Step | Before | After |
|---|---|---|
| `task_scope` asks for the commit | `latest_indexed_commit` → newest `code_chunks` row → `'main'` | → newest **completed** `index_snapshots` row |
| Mid-index | newest rows are the half-written ones | no row yet; readers stay on the previous snapshot |
| Never indexed | falls back to `head_sha`, else default branch | unchanged (same fallback, `http/internal.rs:932`) |
| Pre-existing repository | — | `code_chunks` fallback, until its next index completes |

The read-side fallback for a never-indexed repository is untouched: `task_scope` still calls
`retrieval_commit(head_sha, default_branch)` when `latest_indexed_commit` is `None`
(`http/internal.rs:932`). What changed is
only which commit the **write** side chooses.

### Pruning (unchanged code, changed consequence)

The sweeper was already written for one-snapshot-per-push and the keep-set chain now works as
designed:

```rust
if db::has_active_index_task(pool, repository_id).await? { return Ok(()); }   // mid-write: skip
let mut keep = db::in_use_commits(pool, repository_id).await?;               // in-flight reviews
if let Some(latest) = db::latest_indexed_commit(pool, repository_id).await?  // the completed snapshot
    && !keep.contains(&latest) { keep.push(latest); }
```

- **While a run writes**, `has_active_index_task` is true and the repository is skipped entirely, so
  the partial snapshot is safe.
- **Once it completes**, `latest_indexed_commit` is the new commit, the previous one is not in the
  keep-set, and `prune_code_chunks` / `prune_graph` remove it.
- **If a run fails**, no `index_snapshots` row exists, the keep-set stays on the previous snapshot,
  and the partial one is collected on the next sweep — previously a failed run's partial writes were
  merged into `'main'` and stayed.

`prune_code_chunks` additionally spares rows younger than 10 minutes; `prune_graph` has no such grace.

---

## 4. Impacts

1. **A snapshot describes one commit.** Line-shifted copies and deleted symbols are gone by
   construction, not by cleanup: the next snapshot is written under a key nothing has touched, so it
   contains exactly what the walk produced. The measured 18% staleness drops to 0% for newly written
   snapshots.
2. **Both halves always agree.** One resolution point in `run.rs` instead of two derivations.
3. **Retrieval never sees a half-written index.** Readers move only when a run succeeds, and
   atomically with that status.
4. **A failed run no longer poisons the index.** Its partial snapshot is unreadable and prunable,
   instead of being merged into the live one.
5. **Pruning starts working.** `repos_with_stale_snapshots` only returns repositories with more than
   one distinct `commit_sha`; with a single `'main'` key it returned nothing, so the sweeper had
   nothing to do. It now has a previous snapshot to collect each cycle.
6. **A re-index is no longer a no-op against stale data.** Previously, re-running an index could not
   remove anything, so there was no way to repair a snapshot short of a manual delete.
7. **Secondary, on the `symbol_identity` constraint:** keying by commit does not fix the concurrent-
   `MERGE` duplicate race (that is #657's lock), but each new snapshot starts empty, so duplicates no
   longer accumulate across pushes under one key. Existing `'main'` duplicates still have to be
   cleaned once before the constraint can be created.
8. **Cost unchanged per run, higher peak footprint.** See below.

---

## 5. Storage impact

### What is stored

Unchanged in shape — same tables, same columns, same `:Symbol` properties. What changes is the value
in `commit_sha` / `commit`, plus two small additions: one `TEXT` column on `tasks`, and one
`index_snapshots` row per completed index (three columns, no payload).

### Footprint

Let **S** be one commit's true index size (4,925 symbols / 12,954 edges / its chunks, for this
repository).

| | Before | After |
|---|---|---|
| Snapshots held, steady state | 1 | 1 |
| Snapshots held, between a completed index and the next sweep | 1 | 2 |
| Size of that one snapshot | S + everything ever dropped, **growing without bound** | exactly S |
| After 30 pushes (measured) | 1.22 × S and still climbing | S |
| Peak | unbounded | ~2 × S, bounded by the sweep interval |

So the **peak is higher** — briefly two snapshots instead of one — while the **steady state is
smaller and, more importantly, bounded**. The old single snapshot's growth had no ceiling: it was the
union of every version of the code the repository had ever had indexed. At 1.22× after 30 pushes it
was already larger than two clean snapshots would be at the point where the sweeper collects.

The measurement above is the **graph** half. The chunk half accumulates by the same mechanism — the
same positional key with `DO UPDATE`, so a chunk at a position no longer occupied is never removed —
but I did not quantify `code_chunks` staleness separately, so I am not putting a number on it.

### Embedding cost

Unchanged. Every chunk is re-embedded on every index today as well: `upsert_code_chunks` runs
`DO UPDATE SET … embedding = EXCLUDED.embedding` for each one, so writing to the same key never saved
an embedding call. Carrying an unchanged chunk's embedding across snapshots is a worthwhile
follow-up, not a regression introduced here.

---

## 6. Retrieval impact

`task_scope` resolves `(repository_id, commit_sha)` for every retrieval the review agent makes
through the MCP tools, and everything downstream is filtered by that pair.

- **Query cost:** one indexed lookup on `index_snapshots` (primary key / `index_snapshots_latest_idx`)
  replaces one indexed lookup on `code_chunks`. For a repository with no completed snapshot both run,
  i.e. one extra lookup until its next index finishes.
- **What the agent gets:** results from a single commit. Before, a graph query could return a symbol
  at a line it had moved away from, or a definition deleted several pushes ago, with no way to tell it
  from a live one — the `commit` property said `'main'` for both.
- **Freshness is unchanged.** Retrieval still pins to the latest *indexed* snapshot, not the PR head
  (ADR-0050). This PR does not change that policy; it changes which commit that snapshot actually is.
- **Deploy behaviour:** a repository with no `index_snapshots` row keeps reading its newest rows, so
  nothing goes dark at deploy. It moves onto a real commit the first time an index completes after the
  deploy.

---

## 7. Verification

Against `b128594`, with `postgres` and `neo4j` reported healthy by `docker compose ps`:

```text
cargo test --workspace                          1049 passed, 0 failed, 10 ignored
cargo clippy --workspace --all-targets -D warnings   clean
cargo fmt --all -- --check                           clean
```

| Test | Proves |
|---|---|
| `head_commit_is_the_commit_the_checkout_is_at` | against a real git repo: a full 40-character id, a different one after a second commit on the same branch, never the branch name |
| `latest_indexed_commit_waits_for_the_run_to_complete` | readers stay on the finished snapshot while the next is written, and move when it completes |
| `a_failed_run_does_not_become_readable` | a failed run leaves no snapshot to pin to |

**Mutation-checked.** Replacing `if status == "succeeded"` with `if false` makes readers follow the
newest rows into a half-written snapshot, and the test fails:

```text
left:  Some("sha-b")   (what readers would get mid-write)
right: Some("sha-a")   (the snapshot that actually finished)
```

The pre-existing `latest_indexed_commit_returns_newest_snapshot` passes unchanged — that is the
fallback path.

**CI:** `lint`, `test`, `governance`, `pipeline / rust-build` and all three image builds pass.
`quality` fails through #668's base-ref bug: the gate logs `base: HEAD~10` and scores 4,851 lines
against this PR's 223, and its single finding is at `services/control-plane/src/http/analytics.rs:281`
— a file this PR does not touch. Re-run that check after #668 merges.

**One local test failure, pre-existing:** the live Neo4j suite returned 8/9, failing
`a_delete_commits_each_batch_on_its_own`. `neo4j.rs` is byte-identical to `main` on this branch and
the test exercises none of this PR's code; it is order-dependent (which batch the failing row lands in
depends on Neo4j's scan order) and returned 1,000 once and 2,000 twice against identical code. Being
fixed separately.

---

## 8. Open points

1. **Existing `'main'` snapshots are not cleaned up by this PR.** They stay until each repository's
   next index completes and the sweeper collects them. A one-off delete would reclaim the space
   sooner, and is also a prerequisite for creating `symbol_identity` (its duplicates live there).
2. **The `code_chunks` fallback has no expiry.** It is correct but permanent; worth deciding whether
   it should age out once every active repository has a completed snapshot.
3. **A review that indexes a never-indexed repository** stores under the PR head, as before, and now
   also marks it complete — so that repository's retrieval pins to a PR commit until a default-branch
   index runs. Pre-existing behaviour, recorded as a follow-up in #669.
4. **Embedding reuse across snapshots** (section 5) is the obvious next cost win.
5. **#658's resume** must compare content, not just position, before skipping a chunk — a chunk whose
   text changed at an unchanged position would otherwise be left stale. Belongs in that PR.
6. **#666's `graph_snapshot`** resolves its commit through `task_scope` where it should use the task's
   own commit; a defect in that PR, not this one.

---

## 9. Relation to the measured Neo4j usage

Sources: [`NEO4J_USAGE_STRATEGY_REPORT.md`](NEO4J_USAGE_STRATEGY_REPORT.md) and the live snapshot it
was built from (`neo4j-usage-treemap.html`, `lightbridge-ci-neo4j-0`, 2026-09-29 ~13:40 Z — 11 GiB of
a 20 GiB PVC, 136,350 `:Symbol`, 287,476 `:REL`, 54 repositories, **4 distinct `commit` values**).

**The two investigations found the same defect from opposite ends.** Finding **F8** — "`commit` holds
branch names for 53 repos (`main`/`master`/`develop`) and a SHA for one" — and §3.1 of the strategy
report are the same `head_sha.unwrap_or(default_branch)` that this PR removes. #669 approached it as
a correctness problem (what is stored is not what the code is); the usage report approached it as a
write-amplification problem (the same nodes are rewritten in place on every push). Both follow from
one key.

The usage report's **S7 / P8** is this PR's change, proposed from the storage side. It suggested
*refusing* the write when `head_sha` is absent:

```rust
let Some(commit_sha) = head_sha else {
    tracing::error!(task_id = %id, "task has no head_sha; refusing to key a snapshot by branch");
    return (StatusCode::UNPROCESSABLE_ENTITY, "no head_sha").into_response();
};
```

That would have failed every default-branch index, because an index task never carries a `head_sha`.
This PR resolves the real commit from the checkout instead, so the write succeeds under a correct key.

Its warning applies directly, and is the most important thing to carry from that report:

> Land S7 and the sweeper together, or the churn problem is replaced by an accumulation problem.

Both halves of that precondition are in place: the sweeper exists (ADR-0052) and
[#663](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/663) (bounded graph deletes)
merged on 2026-09-22, so `prune_graph` deletes in batches rather than one transaction.

### Finding by finding

| Finding | Does this PR address it? |
|---|---|
| **F8** snapshot key is a branch name for 53 of 54 repositories | **Yes — directly.** Every new index writes a real commit. |
| **§3.1** growth is in-place rewrite churn, not accumulation | **Changed in kind, not removed.** Each push now writes a new snapshot and the sweeper deletes the old one, instead of `SET`-ing over the live one. Bytes written per push are roughly unchanged. |
| **§4.1** every push rewrites every embedded symbol (~390 MiB for 5,000 symbols) | **No.** The same symbols are written, to a new key instead of over the old records. |
| **F1 / F2** embeddings are 93% of the footprint; stored as `double[4096]` | **No.** Untouched. Needs P1 (float32 or int8) / P6 (one source of truth). |
| **§4.2** failed Lucene merges leave duplicate segments, a disk-full feedback loop | **No.** Index write volume is comparable; this needs headroom (P1/P5/S8), not a key change. |
| **F3** 2 GiB memory limit, OOMKilled restarts | **No.** Ingest volume per run is unchanged. Needs P2. |
| **F4 / P5** 2.1 GiB of transaction log holding rebuildable data | **No.** Chart-level retention change. |
| **F6** 23% of symbols carry no embedding | **Neutral, with one behaviour change** — see below. |
| **F5** JVM Vector API disabled · **F7** all edges typed `REL` · **F9** crash residue | **Unrelated.** |

### What it does buy the storage problem

1. **Pruning starts doing work at all.** `repos_with_stale_snapshots` only returns repositories with
   `count(DISTINCT commit_sha) > 1`. With one branch key per repository that was never true, so the
   sweeper has had nothing to prune since the defect began — which is exactly why the snapshot shows
   4 distinct `commit` values across 54 repositories. Each repository becomes prunable the first time
   it indexes after this lands.
2. **Stale symbols stop carrying embeddings.** A stale symbol is not free: at measured unit costs it
   is 36.8 KiB of array plus 42.9 KiB of vector index — **79.7 KiB each**. The 1,107 stale symbols
   measured on this repository are therefore ≈ **86 MiB** of pure waste in one repository's snapshot.
   (That 18% ratio was measured by replaying one repository's 30 pushes; a graph-wide figure would
   need the same replay per repository, so I am not extrapolating it to the 136,350 nodes.)
3. **A path to creating `symbol_identity`.** The treemap shows `symbol_snapshot` (RANGE on
   `repo_id, commit`) and `snapshot_write_lock` online, but **no `symbol_identity` uniqueness
   constraint** — its creation fails on the duplicate `(repo_id, commit, node_id)` rows that live in
   the accumulated `'main'` snapshots. This PR does not delete those, but it makes them prunable, and
   each new snapshot starts empty so duplicates no longer accumulate across pushes under one key.
4. **A failed run no longer merges its partial writes into the live snapshot**, so a crash mid-ingest
   stops leaving residue behind in the data retrieval reads.

### What it costs, and why sequencing matters

**This PR does not reduce Neo4j's footprint, and it raises the peak.** Between an index completing and
the next sweep (prune tick, default **600s**, and `prune_graph` has no recency grace) a repository
holds two snapshots instead of one. For a 5,000-embedded-symbol repository that is a transient
**≈ 390 MiB** of arrays plus index — the same arithmetic as §4.1, because it is the same bytes, held
twice rather than written over.

The bound, if many repositories happened to re-index inside one sweep window, is the whole embedded
footprint again: 105,120 embedded symbols × 79.7 KiB ≈ **8 GiB**. That is a worst case nobody expects
in practice — re-indexes follow pushes, not a schedule — but on a volume that logged
`No space left on device` on 2026-10-05 it is not a safe margin, and logical deletes do not return
bytes to the filesystem without S8's offline compaction.

**So the order matters:** get headroom first (P1 float32/int8 on `Symbol.embedding`, P5 transaction-log
retention, P2 memory, and S8 once there is room for the copy), then let this land. Merging it is safe;
what needs the headroom check is the moment it reaches production and repositories start writing their
first commit-keyed snapshot. Worth confirming the PVC's free space before the deploy, and watching
`kubelet_volume_stats_available_bytes` across the first few sweeps.

### One behaviour change to know about (F6)

`upsert_graph` preserves an existing vector when a batch arrives without one:

```cypher
SET s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END
```

and `attach_symbol_embeddings` matches on `(repo_id, commit, node_id)` — embeddings belong to a
snapshot, not to a symbol. Under one reused key that `ELSE` branch carried vectors forward from
earlier runs; under a key per commit there is nothing to carry forward, so **each snapshot's embedding
coverage is whatever its own run produced**. Within a run, and across a retry or a re-submitted page,
it still protects an already-attached vector.

In practice this is more correct — a vector computed from an older version of a symbol was attached to
it regardless — and the F6 gap is structural rather than a stalled job: only chunks the walk linked to
a definition carry a `node_id` (ADR-0116), so a symbol with no linked chunk has no vector either way.
The thing to be aware of is that whatever coverage currently exists under a `'main'` key is discarded
when that snapshot is pruned, so the F6 numbers should be re-measured after the first commit-keyed
index rather than compared across the change.

---

## 10. Final local verification (2026-10-06)

Run against `397cba1` — `b128594` plus `adorsys/main` merged in (`3477dde`, a clean merge; main had
only added a ROADMAP entry) plus [ADR-0120](docs/adr/0120-snapshot-keyed-by-indexed-commit.md), its
index row, the ROADMAP entry and one test.

```text
$ docker compose ps --format '{{.Service}}: {{.Status}}'
neo4j: Up 4 hours (healthy)
postgres: Up 4 hours (healthy)

$ cargo fmt --all -- --check                               FMT_OK
$ cargo clippy --workspace --all-targets -- -D warnings     no warnings, no errors
$ cargo test --workspace
FINAL: 1050 passed, 0 failed, 10 ignored

$ cargo test -p control-plane --bin control-plane integrations::neo4j -- --ignored
running 9 tests
test result: ok. 9 passed; 0 failed
```

### Claims checked against the code, not from memory

| Claim | Verified by |
|---|---|
| An index task carries no commit | `create_index_task` (`db/repositories.rs:400`) omits `head_sha` from its `INSERT` column list entirely; its doc comment calls the resulting NULL **load-bearing** for the task's idempotency tuple |
| `head_sha` is in the uniqueness guard | `tasks_idempotency_idx` on `(repository_id, target_type, target_id, command_text, head_sha, run_epoch) NULLS NOT DISTINCT` (migration `0003_task_queue.sql:15`) |
| The sweeper had nothing to prune | `repos_with_stale_snapshots` is `GROUP BY repository_id HAVING count(DISTINCT commit_sha) > 1` — never true with one branch key per repository |
| The partial snapshot is protected mid-write | `has_active_index_task` returns the repository early from `sweep_repo`, before any keep-set is computed |
| Postgres prune has a grace, Neo4j does not | `prune_code_chunks` filters `created_at < now() - interval '10 minutes'`; `prune_graph` has no time predicate |
| Sweep cadence | `DEFAULT_PRUNE_INTERVAL_SECS = 600` (`queue/dispatcher.rs:30`) |
| Embeddings belong to a snapshot | `attach_symbol_embeddings` matches `(repo_id, commit, node_id)` — a vector cannot be found across keys |
| An empty walk submits nothing | `index_chunks` returns on `out.chunks.is_empty()`, `index_graph` on `out.graph.nodes.is_empty()` |
| Bounded graph deletes are available | [#663](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/663) merged 2026-09-22 |
| ~390 MiB transient per 5,000 symbols | 5,000 × (36.8 + 42.9) KiB = **389.2 MiB** at the measured unit costs |
| No numbering collision after the merge | migrations end at `0042_index_snapshots.sql`; ADRs end at `0120` |

### One test added, and the mutation that motivated it

`a_run_that_wrote_nothing_marks_no_snapshot` pins why the commit is recorded **from the first batch**
rather than when the index task is created: recording it on arrival makes `tasks.indexed_sha` evidence
that something was written under that key, not merely an intention to write there.

Proven by mutation, in two directions:

| Mutation | Result |
|---|---|
| `if status == "succeeded"` → `if false` (no completion marker) | both new tests fail: `left: Some("sha-b")` / `right: Some("sha-a")`, and `left: []` / `right: ["sha-a"]` |
| Record the commit up front, before any batch | `left: ["sha-a", "sha-b"]` / `right: ["sha-a"]` — a run that wrote **nothing** marks a snapshot, and `latest_indexed_commit` then pins retrieval to a commit with zero rows |

The second is the hollow index ADR-0050 was written to prevent, which is why registering the commit at
task creation is not equivalent to recording it when data arrives.

Both mutations were reverted and the tree re-confirmed green.

### Live Neo4j suite

**9 passed / 0 failed** this run, including `a_delete_commits_each_batch_on_its_own`, which failed on
the earlier pass. Its deterministic fix is **not** on this branch
(`git merge-base --is-ancestor` says so), so this is the order-dependent test passing rather than a
test that has been repaired — it is being fixed separately. Nothing else in the suite is affected
either way.

### CI on `397cba1`

`lint`, `test`, `governance`, `pipeline / rust-build` and all three image builds **pass**.

`quality` **fails**, for the third time with the same signature and the same single finding in a file
this PR does not touch:

```text
GITHUB_BASE_REF: main
INFO: Diff-scoping: 5130 added line(s) in this PR (base: HEAD~10).
ERROR: PR contains 1 new actionable finding(s) … on changed lines.
ci.rules.semgrep.security.rust-no-panic … at services/control-plane/src/http/analytics.rs:281
```

[#668](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/668) fixes the base-ref
resolution; re-run this check once it merges.
