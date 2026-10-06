# ADR-0120: An index snapshot is keyed by the commit it was taken from

- **Status:** Proposed
- **Date:** 2026-10-06
- **Deciders:** @leghadjeu-christian

## Context and Problem Statement

A **snapshot** is everything stored under one `(repository_id, commit_sha)` key, across both halves of
the index: `code_chunks` rows in pgvector, keyed
`(repository_id, commit_sha, file_path, start_line, end_line)`, and `:Symbol` nodes in Neo4j, keyed
`(repo_id, commit, node_id)`. There is no `Snapshot` type in the code; the snapshot *is* that key.
[ADR-0050](0050-retrieval-pins-to-latest-indexed-snapshot.md) makes it the unit of retrieval —
everything an agent reads is pinned to one — and [ADR-0052](0052-index-snapshot-pruning.md) makes it
the unit of garbage collection.

Both halves derived the key independently, from the task:

```rust
let commit_sha = context.head_sha.as_deref().unwrap_or(&context.default_branch);
```

An **index** task carries no commit — `create_index_task` does not set `head_sha` at all, so it is
`NULL`, and its own doc comment records this as load-bearing for the task's idempotency tuple — so the `unwrap_or` fired on every default-branch index and the key became
the literal string `"main"`.

Three properties then compound. Writes merge but never remove (`ON CONFLICT … DO UPDATE` in Postgres,
`MERGE … SET` in Neo4j), so a symbol that moved to a new line is written at its new position while the
copy at the old one is left untouched. `'main'` is always the repository's latest snapshot, so ADR-0052's
keep-set always contains it and it is never pruned. And every push writes into it again. The result is a
single snapshot that is the union of every version of the code ever indexed, and which retrieval
nonetheless reports as the current one.

Measured by replaying this repository's last 30 pushes into one snapshot:

| | Stored | In the code at HEAD | Stale |
|---|---:|---:|---:|
| `:Symbol` nodes | 6,032 | 4,925 | 1,107 (18%) |
| `:REL` edges | 12,954 | 9,566 | 3,388 |

Of the 1,107 stale symbols, **1,023 are line-shifted copies** — the same definition recorded at a line
it has since moved away from. An agent cannot tell one from a live symbol: both carry `commit: 'main'`.

Production confirms the shape. A read-only snapshot of the production graph on **2026-09-29** found
**4 distinct `commit` values across 54 repositories** — a branch name (`main`/`master`/`develop`) for 53
of them and a real SHA for exactly one, the one whose index was last written by a *review* task, which
does carry a `head_sha`.

That measurement also establishes what the defect costs in storage, from the other direction. Because
the key is reused, every push re-runs `SET s.embedding` over a 32 KiB array per symbol and forces the
vector index to re-index it — roughly **390 MiB of writes for a 5,000-symbol repository**, superseded
logically rather than returned to the filesystem. ADR-0052's sweeper, which exists precisely to bound
this, had nothing to do: `repos_with_stale_snapshots` only returns repositories holding more than one
distinct `commit_sha`, which with one branch key per repository is never true.

So: **what key should an index write under, and when does what it wrote become readable?**

## Decision Drivers

- A snapshot must describe one state of the code. Stale entries are indistinguishable from live ones.
- Both halves must always agree on the key, or retrieval mixes one version's chunks with another's graph.
- Retrieval must never read a snapshot that is still being written (ADR-0050 promises a commit that
  provably has chunks).
- A repository already indexed must keep working across the deploy; nothing may go dark.
- ADR-0052's sweeper must become load-bearing in the same change, or churn is replaced by unbounded
  accumulation.
- Prefer the mechanism already running: one GC tick, one keep-set, one readiness lookup.

## Considered Options

- **A — Resolve the commit from the checkout the runner indexed.**
- **B — Refuse the write when the task carries no `head_sha`.**
- **C — Have the control plane resolve the branch to a SHA when it creates the index task.**
- **D — Keep the branch key and delete the stale entries after each write.**
- **E — Keep the branch key and accept the staleness.**

## Decision Outcome

Chosen option: **"A — resolve the commit from the checkout the runner indexed"**, because the runner is
the only party that knows which tree it actually read, and reading the key from that tree makes the key
true by construction rather than by agreement between components. The decisions below are what option A
means concretely, labelled `D1`–`D5`.

### D1 — The key is `git rev-parse HEAD` in the directory that was walked

```rust
let out = indexer::walk(checkout).await?;
// The commit both halves of the index are stored under, read from the tree they describe.
let commit_sha = clone::head_commit(checkout).await?;
```

Not the webhook's `after` SHA, not the branch tip resolved at dispatch, and not the task's `head_sha`:
the commit of **the tree that was parsed**. Anything else can disagree with what was indexed — a clone
that landed a moment after another push resolves to a different commit than the one the task was
created for, and the snapshot would then be named after code it does not contain.

`perform_indexing` resolves it **once** and passes it to both halves. `index_graph` and `index_chunks`
each lost their own derivation and took a `commit_sha: &str` parameter. This is the part that makes
disagreement impossible rather than unlikely: neither half decides any more, so there is no second
expression to keep in step with the first.

Failing to read it fails the index. It runs immediately after a successful checkout, so a failure means
something is wrong with the checkout itself — and a failed index is recoverable, while an index
silently keyed by a branch is the defect this ADR exists to remove.

### D2 — A run records the commit it is writing; the snapshot becomes readable when the run succeeds

Keying by commit introduces a window that reusing a key did not have. A new snapshot is, by definition,
newer than the one retrieval is reading, so "the newest rows" would point at a snapshot that is still
being written — a few thousand symbols into a repository, with no edges yet.

So readability is tracked explicitly, on the run:

- `tasks.indexed_sha` is set from the **first batch** of a run (`record_indexed_commit`), from both the
  chunk and the graph ingest path. Best-effort and idempotent: a primary-key `UPDATE` guarded by
  `IS DISTINCT FROM`, so the first batch writes and the rest match no row.
- A row in `index_snapshots (repository_id, commit_sha, completed_at)` says a run **finished** writing
  that snapshot. It is inserted in `set_task_status`'s **existing transaction**, on a `succeeded`
  status:

```sql
INSERT INTO index_snapshots (repository_id, commit_sha)
SELECT repository_id, indexed_sha FROM tasks WHERE id = $1 AND indexed_sha IS NOT NULL
ON CONFLICT (repository_id, commit_sha) DO UPDATE SET completed_at = now()
```

Being inside that transaction is the decision, not an implementation detail: the status and the
snapshot's visibility cannot disagree, and a rollback takes both. A run that fails, times out or is
cancelled inserts nothing, so a partial snapshot is never readable — and, having no row, is also
prunable by D4.

This is the `ready`-row-per-commit gate that
[ADR-0055](0055-review-waits-for-index-readiness.md)'s "failed/partial index" follow-up reserved
`repo_index` for. `repo_index` still has no writer and is unchanged; `index_snapshots` records
completion for the retrieval path, which is the half that was load-bearing.

### D3 — Retrieval pins to the newest completed snapshot, and falls back to the newest rows

`latest_indexed_commit` prefers `index_snapshots`, and keeps the old query behind it:

```rust
// A snapshot being written has no `index_snapshots` row, so retrieval stays on the last one that
// finished. Backed by `index_snapshots_latest_idx`, so this is an index lookup, not a scan — it runs
// on every search/graph query via `task_scope`.
let completed: Option<String> = /* SELECT … FROM index_snapshots … ORDER BY completed_at DESC LIMIT 1 */;
if completed.is_some() { return Ok(completed); }

// Repositories indexed before snapshots were recorded have no such row, and their newest rows
// are the whole index.
/* SELECT commit_sha FROM code_chunks … ORDER BY created_at DESC, id DESC LIMIT 1 */
```

The fallback is what makes this deployable rather than a migration event. Every repository indexed
before this change has rows and no completion marker; without the fallback all of them would read as
never-indexed at the moment of deploy, and ADR-0050's anchor would collapse to the PR head — a full
re-index per PR, which is the regression ADR-0050 was written to remove. With it, such a repository
keeps reading exactly what it read before and moves onto a real commit the first time an index
completes.

The read-side fallback for a repository that has **never** been indexed — `head_sha`, else the default
branch, in `retrieval_commit` — is untouched. It is a retrieval choice for a repository with nothing to
pin to, not a write key, and it was never the defect.

### D4 — Pruning is ADR-0052's, unchanged, and only now has work to do

No sweeper code changes. What changes is that its keep-set finally describes something:

```rust
if db::has_active_index_task(pool, repository_id).await? { return Ok(()); }  // mid-write: skip
let mut keep = db::in_use_commits(pool, repository_id).await?;               // in-flight reviews
if let Some(latest) = db::latest_indexed_commit(pool, repository_id).await?  // the completed snapshot
    && !keep.contains(&latest) { keep.push(latest); }
```

The chain holds at each step. While a run writes, `has_active_index_task` is true and the repository is
skipped entirely, so the partial snapshot cannot be collected — which matters because an index task
carries a NULL `head_sha` and `in_use_commits` therefore cannot protect it, and because `prune_graph`
has no recency grace. Once the run completes, `latest_indexed_commit` is the new commit, the previous
one is not in the keep-set, and both halves prune it. If the run fails, the keep-set stays on the
previous snapshot and the partial one is collected on the next cycle.

Pairing this with D1 is not optional. The strategy work that measured the production graph said it
directly: land the key change and the sweeper together, or the churn problem is replaced by an
accumulation problem. The sweeper is in place (ADR-0052) and its deletes are bounded
([#663](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/663), merged
2026-09-22) rather than one transaction per repository, so the collection this change starts generating
is affordable.

### D5 — Existing branch-keyed snapshots are not cleaned up here

They are left to D4. A repository becomes prunable the first time it indexes after this lands, and its
`'main'` snapshot goes on the following sweep — within one `prune_interval` (default 600 s), plus the
10-minute recency grace on the Postgres half.

Deleting them eagerly in a migration would mean deleting the index that retrieval is reading at that
moment, for every repository at once, before any replacement exists. D3's fallback keeps those
snapshots serving until their replacement is complete; removing them is the sweeper's job, under the
keep-set that already knows what is still in use.

One consequence of that choice is worth stating plainly, because it is about a different ADR's
blocker: ADR-0117's `symbol_identity` constraint cannot be created while duplicate
`(repo_id, commit, node_id)` rows exist, and those duplicates live in exactly these accumulated
snapshots. This ADR does not delete them. It makes them collectable, and it stops new ones
accumulating under a reused key — each new snapshot starts empty.

### Consequences

- Good, because a snapshot now describes one commit by construction. The 18% staleness and the
  1,023 line-shifted copies are not cleaned up, they are structurally absent from anything written
  from here on.
- Good, because both halves of the index can no longer disagree about which commit they describe.
- Good, because retrieval never reads a half-written snapshot, and the guarantee is a transaction
  rather than a timing assumption.
- Good, because a failed run no longer merges its partial writes into the data retrieval is serving.
- Good, because a re-index can finally repair a repository: previously no write could remove anything,
  so a wrong snapshot stayed wrong until someone deleted rows by hand.
- Good, because ADR-0052 starts doing the work it was written for.
- Bad, because the peak footprint rises: between a run completing and the next sweep a repository holds
  two snapshots instead of one. At the production graph's measured unit costs — 36.8 KiB of array plus
  42.9 KiB of vector index per embedded symbol — that is a transient ~390 MiB for a 5,000-symbol
  repository, and logical deletes do not return bytes to the filesystem without an offline compaction.
  On a volume that logged `No space left on device` on 2026-10-05, headroom is a precondition for the
  deploy, not a follow-up.
- Bad, because embedding cost per push is not improved. It is not made worse either — every chunk is
  re-embedded today as well, since `upsert_code_chunks` overwrites each embedding on conflict — but
  carrying an unchanged chunk's vector across snapshots is now the obvious next saving, and it was not
  available while there was only one snapshot to carry it within.
- Neutral, because a symbol's embedding belongs to a snapshot, not to a symbol:
  `attach_symbol_embeddings` matches on `(repo_id, commit, node_id)`, and `upsert_graph`'s
  `ELSE s.embedding` branch preserved a vector across runs only because the key was reused. Each
  snapshot's coverage is now whatever its own run produced — more honest, since a vector computed from
  an older version of a symbol was previously attached to it regardless, but it means embedding
  coverage must be re-measured after the first commit-keyed index rather than compared across the
  change.
- Neutral, because a review task that indexes a repository with no index yet still stores its snapshot
  under the PR head, as before, and now also marks it complete — so that repository's retrieval pins to
  a PR commit until a default-branch index runs. Pre-existing behaviour, recorded as a follow-up.
- Neutral, because this changes no retrieval *policy*. ADR-0050 still pins to the latest indexed
  snapshot rather than the PR head; only the identity of that snapshot changes.

## Pros and Cons of the Options

### A — Resolve the commit from the checkout the runner indexed

- Good, because the key is read from the tree it describes, so it cannot be wrong about it.
- Good, because it needs no new field on the task and no agreement between dispatch and the runner.
- Good, because one resolution point replaces two independent derivations.
- Bad, because it introduces the write window D2 must then close, and the peak footprint of D4's
  two-snapshot interval.

### B — Refuse the write when the task carries no `head_sha`

- Good, because it is the smallest possible change and makes the invariant explicit at the boundary.
- Bad, because an index task *never* carries a `head_sha`, so this fails every default-branch index —
  the only writer of the index — and the system stops indexing entirely.
- Bad, because it reports a design gap as a per-request error, on a path with no caller able to fix it.

### C — Have the control plane resolve the branch to a SHA when it creates the index task

- Good, because the task would then carry a real commit and the existing `unwrap_or` would be harmless.
- Good, because the key would be known before dispatch, which is convenient for bookkeeping.
- Bad, because the resolved commit can differ from the one the runner ends up checking out: a push
  between task creation and clone, a retry minutes later, a reaper-requeued run. The snapshot would be
  named after a tree that was never parsed — a subtler version of the same defect.
- Bad, because it puts a forge call on the task-creation path, which currently needs none.

### D — Keep the branch key and delete the stale entries after each write

- Good, because it would leave exactly one snapshot and avoid the two-snapshot peak entirely.
- Bad, because "stale" can only mean "not in the set this run just wrote", so the delete must be
  computed against the live snapshot retrieval is reading, mid-write. A run that then fails leaves the
  index with entries removed and their replacements never written.
- Bad, because it keeps a key that lies about what it contains, and keeps retrieval unable to
  distinguish versions; it treats the symptom and preserves the cause.

### E — Keep the branch key and accept the staleness

- Good, because it is free, and the index is advisory context rather than a correctness surface.
- Bad, because the staleness is not bounded — it grows with every push, without limit.
- Bad, because the agent is given stale definitions with no signal that they are stale, and 1,023 of
  the 1,107 measured cases are line-shifted copies, i.e. exactly the kind that reads as plausible.
- Bad, because it leaves the measured write amplification in place and keeps ADR-0052 inert.

## More Information

- [#669](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/669) — the measurement,
  the replay method and the implementation plan
- [#672](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/672) — this decision, as
  implemented
- [ADR-0050](0050-retrieval-pins-to-latest-indexed-snapshot.md) — the snapshot as the unit of
  retrieval, and why it is the latest indexed one rather than the PR head
- [ADR-0052](0052-index-snapshot-pruning.md) — the keep-set and the GC tick this relies on
- [ADR-0055](0055-review-waits-for-index-readiness.md) — the readiness gate whose follow-up reserved a
  per-commit `ready` row
- [ADR-0086](0086-in-house-code-graph-crate.md) — the runner-extracts/control-plane-writes contract,
  which is why the key had to be resolved runner-side
- [ADR-0116](0116-one-walk-node-id-symbol-embeddings.md) — one walk, and why a symbol's vector is
  attached per snapshot
- [ADR-0117](0117-paged-graph-submission-and-symbol-identity.md) — the `symbol_identity` constraint
  whose duplicates live in the accumulated snapshots (D5)
- `NEO4J_USAGE_STRATEGY_REPORT.md` — the 2026-09-29 production measurement: 4 distinct `commit` values
  across 54 repositories, the per-symbol unit costs, and the churn-versus-accumulation warning
