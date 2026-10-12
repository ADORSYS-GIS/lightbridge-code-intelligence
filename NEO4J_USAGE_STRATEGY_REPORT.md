# Neo4j storage: measured usage and reduction strategies

**Date:** 2026-10-05
**Source data:** `neo4j-usage-treemap.html` — a live read-only snapshot of `lightbridge-ci-neo4j-0`
taken **2026-09-29 ~13:40 Z** (`df`, `du -sh`, `cypher-shell`, `SHOW INDEXES`, `SHOW TRANSACTIONS`,
`dbms.listConfig()`).
**Supersedes the estimates in** [`NEO4J_STORAGE_GROWTH_REPORT.md`](NEO4J_STORAGE_GROWTH_REPORT.md) —
see §3, which corrects two of its conclusions.
**Related:** [#662](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/662),
ai-helm#1130/#1138/#1143, ai-helm-values#463/#464.

---

## 1. What the measurement shows

| Component | Kind of data | Size | Share |
|---|---|---:|---:|
| `schema/index/vector-2.0` | HNSW vector index | **4.2 GiB** | 42.0% |
| `neostore.propertystore.db.arrays` | `Symbol.embedding` as `double[4096]` | **3.6 GiB** | 36.0% |
| `transactions/neo4j/*` | transaction log, 8 × 256 MiB | **2.1 GiB** | 21.0% |
| `id-buffer.tmp.6` | ingest scratch | 38 MiB | 0.37% |
| `propertystore.db.strings` | `label`, `source_file`, `commit` | 28 MiB | 0.27% |
| `propertystore.db` | `start_line`, `repo_id`, `node_id` | 27 MiB | 0.26% |
| `relationshipstore.db` (+ groups/degrees) | 287,476 `REL` edges | 10 MiB | 0.10% |
| `schema/index/range-1.0`, `token-lookup-1.0` | RANGE + lookup indexes | 7.8 MiB | 0.08% |
| `schema/index/fulltext-1.0` | fulltext on `label`, `source_file` | 5.2 MiB | 0.05% |
| `neostore.nodestore.db` | 136,350 `:Symbol` records | 2.0 MiB | 0.02% |
| `corrupted-neostore.transaction.db` | crash remnant | 228 KiB | — |

Graph contents: **136,350** `:Symbol` nodes · **287,476** `:REL` edges · **54** repositories ·
**4 distinct `commit` values** · 11,281 distinct source files · **105,120** symbols with an
embedding, **31,230** without.

**Embeddings are 93% of the footprint.** Arrays (3.6 GiB) + vector index (4.2 GiB) = 7.8 GiB of a
~10 GiB `du` total, and most of the 2.1 GiB transaction log is embedding `SET` churn. The structural
graph the database exists to serve — nodes plus edges — is **12 MiB, about 0.1%**.

### Derived unit costs

| Per embedded symbol | Measured | Theory | Comment |
|---|---:|---:|---|
| Property array | 36.8 KiB | 32.8 KiB (4096 × 8 B) | matches: Bolt has no 32-bit float, so values are stored as doubles |
| Vector index | **42.9 KiB** | ~20.2 KiB if raw f32 is kept beside the int8 copy; ~4.2 KiB if not | **2× to 10× above theory — see §2.4 and §4.2** |
| Structure (node + edges + strings + non-vector indexes) | ~0.6 KiB | — | 80 MiB / 136,350 — the part we actually query |

---

## 2. Anatomy: exactly what is stored, and exactly what happens

This section is the "how", in detail. Everything below is either read from the code, read from the
measurement, or derived arithmetically from both — each claim says which.

### 2.1 What one `:Symbol` is, on disk

A symbol is one node with six properties and no more. From `upsert_graph`
(`services/control-plane/src/integrations/neo4j.rs`):

```cypher
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id})
SET s.label = n.label, s.source_file = n.file, s.start_line = n.line,
    s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END
```

A real one, from the production logs and tests:

```text
repo_id     = 430                                               // integer, identifies the repository
commit      = "main"                                            // STRING — a branch name, not a SHA (see §3.1)
node_id     = "charts/librechat-app/files/seed-agents.js"       // STRING — path, or "src/auth.rs#40:validate"
label       = "seed-agents.js"                                  // STRING — what fulltext search matches
source_file = "charts/librechat-app/files/seed-agents.js"       // STRING
start_line  = 1                                                 // integer
embedding   = [0.0123, -0.0456, …]                              // 4096 numbers
```

Those land in **four different files**, and the measurement lets us check the per-record costs:

| What | File | Measured | Per node | Consistent with |
|---|---|---:|---:|---|
| The node itself | `neostore.nodestore.db` | 2.0 MiB | **15.4 B** | Neo4j's fixed ~15-byte node record — the node is a stub: labels, first property pointer, first relationship pointer |
| Scalars + property chain | `neostore.propertystore.db` | 27 MiB | ~208 B | ~5 fixed-size property records per node, each holding a key id, a type and either an inline value or a pointer |
| Strings | `propertystore.db.strings` | 28 MiB | ~215 B | `node_id` + `source_file` + `label` + `commit`, in 128-byte dynamic blocks (long paths spill across several) |
| **The embedding** | `propertystore.db.arrays` | **3.6 GiB** | **36.8 KiB** | 4096 × 8 B = 32,768 B of payload, chopped into ~273 dynamic records of 128 B (120 B payload + header) ≈ 34 KiB, plus id and fragmentation overhead |

So the node, its scalars and all its strings together are **about 440 bytes**. The embedding is
**36.8 KiB — 85× the rest of the node combined.**

The `:REL` edges are similarly cheap: 287,476 edges in 10 MiB is **~36 B/edge**, consistent with
Neo4j's fixed ~34-byte relationship record. All 287,476 of them carry one property, `relation`.

*Derivation note:* the 15.4 B and ~36 B figures are measured sizes divided by measured counts, and
they land on Neo4j's documented record sizes, which is a useful cross-check that the `du` numbers and
the Cypher counts describe the same database. The 120-byte dynamic-record payload is inferred from
the array arithmetic, not read from the docs.

### 2.2 Why the embedding is 8 bytes per dimension, not 4

The runner computes `Vec<f32>` — 4 bytes per dimension. It arrives as 8. The reason is the wire
protocol, not Neo4j's storage engine: PackStream defines exactly one floating-point type, a 64-bit
IEEE double, and states that 32-bit floats are not included. So `(0.0123f32).into()` becomes a
`BoltType::Float` carrying a double, and the array store receives 4096 doubles. There is no
`float32` property type to ask for.

```rust
// attach_symbol_embeddings — rows are (String, Vec<f32>)
("embedding".to_string(), embedding.clone().into()),   // f32 -> Bolt Float (f64) -> 8 B/dim on disk
```

That single protocol detail is **1.8 GiB of the 3.6 GiB array store**: half of it is padding of
precision the model never produced.

### 2.3 What the vector index stores, separately

`ensure_indexes` declares an index over that property:

```cypher
CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS
FOR (s:Symbol) ON s.embedding
OPTIONS { indexConfig: { `vector.dimensions`: 4096, `vector.similarity_function`: 'cosine' }}
```

The snapshot reports it as `4096-dim · COSINE · M=16 · ef_construction=100 · int8 quantized`. This is
a **second, independent copy** of the same numbers, written by Lucene into
`/data/databases/neo4j/schema/index/vector-2.0/`, and maintained automatically — every
`SET s.embedding` rewrites the property *and* re-indexes the vector. Nobody writes to it directly, and
it cannot be kept without the property it indexes.

Inside it, per vector:

| Part | Size at 4096 dims | Purpose |
|---|---:|---|
| Quantized vector (int8) | 4,096 B | what the HNSW graph is traversed on |
| Raw vector (float32), *if retained* | 16,384 B | rescoring candidates at full precision |
| HNSW neighbour lists | ~100–200 B | `M=16`, so up to 32 links on level 0, delta-encoded |

**Measured: 42.9 KiB per vector.** Lucene's scalar-quantized format delegates raw-vector storage to a
flat format, and the Javadoc I could reach does not say whether the raw copy is retained, so there are
two readings:

- **raw kept (~20.2 KiB/vector theory)** → measured is ~2.1× theory, i.e. roughly one extra copy of
  the index lying around in un-merged segments;
- **quantized only (~4.2 KiB/vector theory)** → measured is ~10× theory, i.e. heavy segment
  duplication.

Either way the conclusion in §4.2 holds — merges are not completing — and one command settles which:

```bash
kubectl -n converse exec lightbridge-ci-neo4j-0 -- \
  ls -la /data/databases/neo4j/schema/index/vector-2.0/*/
# many same-prefix segments, or .vec files roughly 4× the .veq files, tells the story
```

### 2.4 What happens on one push, step by step

This is the full lifecycle, with the bytes named at each step.

1. **Webhook.** A push to the default branch arrives; the control plane records a task and logs
   `default-branch push → re-index queued`. The dispatcher launches a Job.
2. **The runner clones and walks the repo**, producing chunks and, in one walk (ADR-0116), the
   structural graph with each symbol linked to the chunk that is its body.
3. **Chunks go to Postgres** in batches of 16 (`INDEX_EMBED_BATCH_SIZE=16`, visible as
   `chunks ingested … chunk_count: 16`): one `code_chunks` row per chunk with a `vector(4096)`
   embedding — **16 KiB each, float32, and deliberately unindexed** because 4096 exceeds pgvector's
   HNSW limit.
4. **The graph goes to Neo4j in pages of up to 2,000** nodes or edges, each page its own transaction
   (`graph ingested … nodes: 2000, edges: 0`). Here is the critical part: because the snapshot key is
   `"main"` rather than a SHA, `MERGE` **finds the nodes from the previous push and updates them**.
   Nothing new is created; everything is rewritten.
5. **Embeddings are attached** (`symbol embeddings attached … offered: 8, updated: 8`), each one a
   `SET s.embedding` over 4096 doubles.
6. **Each of those writes lands in three places at once:**
   - the **transaction log** first — the full 32 KiB array as part of the committed transaction;
   - the **array store**, where the old 273-record array is marked deleted and a new one written;
   - the **vector index**, as a new Lucene segment entry, with the old one tombstoned.
7. **A checkpoint** flushes the store and makes older transaction-log files prunable — bounded by
   `2 days 2G`, which is why the log sits at 2.1 GiB.
8. **Lucene merges** are supposed to rewrite small segments into bigger ones and drop the tombstoned
   entries, reclaiming space. A merge needs scratch room for the new segment *before* deleting the
   old one, and since 2026-09-24 those merges have been dying:
   `Lucene Merge Thread … Caused by: java.io.IOException: No space left on device`.
9. **Nothing returns space to the filesystem.** Neo4j does logical deletes: the freed record ids go
   into `.id` files for reuse, and the store files keep their size. The volume therefore shows the
   high-water mark, and only `neo4j-admin database copy` shrinks it.

So one push to a 5,000-symbol repository writes ~390 MiB (§4.1), frees ~390 MiB logically, and returns
0 bytes — while the index accumulates segments it can no longer merge.

### 2.5 Why it ends in a panic rather than a slow degradation

The failure mode is specific, and it is the same in all three outages. Neo4j commits in two phases:
it appends the transaction to the log, then applies it to the store. If the apply fails — out of heap
on 2026-09-18, out of disk on 2026-09-24 and 2026-10-05 — the database is left with a transaction that
is durable in the log but absent from the store. That is unrecoverable in place, so Neo4j marks the
database as panicked and refuses **every** subsequent transaction:

```text
TransactionCommitFailed: Could not apply the transaction: Transaction #112435
  {started 10:47:41.823, committed 10:47:42.019, with 2 commands …} to the store after written to log
TransactionStartFailed: The database has encountered a critical error, and needs to be restarted.
```

Two consequences worth internalising:

- **The trigger can be trivially small.** Transaction #112435 carried **2 commands**. Once the disk is
  full, the next write of any size panics the database.
- **The panic blocks its own cure.** Pruning stale snapshots and compacting the store are both writes,
  so a disk-full panic cannot self-heal: full disk → panic → no pruning → still full. That is why the
  2026-09-24 outage ran for 3 days 21 hours and why expanding the volume was the only way out.

And because the liveness probe is `tcpSocket: 7687`, which a panicked Neo4j answers perfectly well,
Kubernetes never noticed any of it.

---

## 3. Corrections to the previous report

Two conclusions in `NEO4J_STORAGE_GROWTH_REPORT.md` were wrong, and the measurement is what proves it.

**3.1 Growth is not snapshot accumulation — it is in-place rewrite churn.**
That report assumed each push leaves another snapshot behind and that the index sweeper was the
lever. The snapshot shows **4 distinct `commit` values for 54 repositories**: the key is a *branch
name* (`main`/`master`/`develop`) for 53 of them, and a SHA for exactly one. So snapshots are not
accumulating — every push **rewrites the same nodes in place**, re-running `SET s.embedding` over a
32 KiB array and forcing the vector index to re-index the same symbol. Pruning has almost nothing to
prune; the churn is the growth engine.

The cause is in code, `services/control-plane/src/http/internal.rs:795` and `:924`:

```rust
let commit_sha = head_sha.unwrap_or(default_branch);
// ...
fn retrieval_commit(head_sha: Option<&str>, default_branch: &str) -> String {
    head_sha.unwrap_or(default_branch).to_string()
}
```

When a task carries no `head_sha`, the snapshot key silently becomes the branch name.

**3.2 The 2026-09-29 OOMKill was caused by the analysis itself, not by the workload.**
I attributed the `exitCode 137` at 13:38 Z to Lucene merge pressure, and used it to justify
ai-helm#1143. The treemap's own disclosure says a probe reading embedding array sizes was OOMKilled
at 13:38 Z. Same timestamp, same container — **it was the measurement**. The memory increase is
still right (live usage was 1,651 MiB of 2 GiB, 82%, and finding F3 says the same), but the
attribution in that PR is wrong and should not be cited as evidence of a workload-induced OOM.

**3.3 The growth rate was understated by about 14×.** I derived ~107 MiB/day from "10 GiB over
96 days". Measured: **11 GiB used on 2026-09-29** (52% of 20 GiB), and the volume was full by
**2026-10-05 10:47** when `No space left on device` was logged — at most 6 days later, so **≥1.5
GiB/day**. Growth is not linear from zero; it tracks push volume and the embedding backfill.

*Inference, not measurement:* the 2026-10-03 panic was most likely the same `ENOSPC`, which would put
the fill between 09-29 and 10-03 (~2.2 GiB/day). Its root-cause line had already rotated out of both
the pod and control-plane logs before it could be captured, so the trigger of that specific panic
remains unproven.

---

## 4. Why it grows so fast

### 4.1 Every push rewrites every embedded symbol

A 5,000-symbol repository pushed once costs, at current encoding:

```
5,000 × (36.8 KiB array + 42.9 KiB index) ≈ 390 MiB of writes
```

and the superseded records are freed logically, not returned to the filesystem
([space reuse](https://neo4j.com/docs/operations-manual/5/performance/space-reuse/)). The store
tracks the high-water mark of live + not-yet-reused data.

### 4.2 Failed index merges amplify, and that is a feedback loop

The vector index measures **42.9 KiB per vector against a theoretical 20.2 KiB** (int8 copy + retained
raw float32 + HNSW links) **or 4.2 KiB** (quantized only) — §2.3 shows why both readings exist. A
factor of 2× to 10× is not explained by encoding. The most likely cause is
**un-merged duplicate segments**: Lucene writes a new segment per batch and reclaims space only when
merges complete, and merges have been dying for weeks:

```text
2026-09-24 10:15:27  Exception in thread "Lucene Merge Thread #399"
                     Caused by: java.io.IOException: No space left on device
2026-10-05 10:4?:??  Exception in thread "Lucene Merge Thread #0"
                     Caused by: java.io.IOException: No space left on device
```

A merge needs scratch space roughly equal to the segments it rewrites. Near a full disk it fails,
leaving both copies — so the index grows, which fills the disk, which fails the next merge. **This is
why expanding 10 GiB → 20 GiB bought only five days.** On 2026-10-05 the recreated pod reached
`Started.` at 10:42:22 and panicked at 10:47:42 — five minutes — on a transaction of **2 commands**.

### 4.3 The transaction log retains rebuildable data

2.1 GiB (21%) under a default `2 days 2G` retention, holding mostly embedding `SET` churn. The graph
is derived from git plus re-embedding, so a long point-in-time window buys little.

---

## 5. Strategies, ordered by payoff

### S1 — Stop storing 4096 doubles in the property store (≈ −2.7 GiB, largest single win)

Bolt/PackStream has no 32-bit float ([PackStream](https://neo4j.com/docs/bolt/current/packstream/)),
so `float32` is not reachable through a property of type `LIST<FLOAT>`. But a Neo4j vector index
accepts `LIST<INTEGER>` as well as `LIST<FLOAT>`
([vector indexes](https://neo4j.com/docs/cypher-manual/5/indexes/semantic-indexes/vector-indexes/)),
and Neo4j's array store bit-packs integer arrays to the minimum width the values need. Quantizing to
int8 before the write therefore shrinks the property roughly 8×:

```rust
/// Quantize a unit-normalised embedding to int8 so the stored property is an
/// integer array Neo4j can bit-pack, instead of 4096 doubles.
fn quantize_i8(v: &[f32]) -> Vec<i64> {
    v.iter().map(|x| (x.clamp(-1.0, 1.0) * 127.0).round() as i64).collect()
}

// in upsert_graph / attach_symbol_embeddings
("embedding".to_string(), quantize_i8(&embedding).into()),
```

The index is already int8-quantized internally, so recall is unchanged by the index. The property copy
is read back in exactly one place — `symbol_embedding()` (`neo4j.rs:632`, the only query returning
`s.embedding`), consumed by the admin "similar symbols" endpoint (`admin.rs:993`), which passes it
straight back into `hybrid_symbol_search` **as the query vector**. So quantizing does change that
endpoint: the vector needs dequantizing (`x / 127.0`) to satisfy the `&[f32]` signature, and its
query vector becomes int8-rounded. Given the index it queries is already int8, the practical recall
cost should be small — but it is a behaviour change, not a pure storage change.

**Two things to verify before committing to this:**
1. **The bit-packing claim.** Neo4j is documented to store arrays in a dynamic store, and integer
   arrays are bit-packed to the width the values need — but I have not measured it here. Re-index one
   repository this way and compare `du -sh neostore.propertystore.db.arrays` against 4096 B/symbol.
   If Neo4j stores 8 bytes per element regardless, this strategy yields nothing and S2/S3 are the
   only levers.
2. **That the vector index accepts the quantized property.** The Cypher manual lists
   `LIST<INTEGER | FLOAT>` as indexable, so an integer array should be valid, but the index must be
   re-created and `SHOW INDEXES` checked for `ONLINE` at 100% population.

### S2 — Cut the dimension (≈ 4× on both stores)

`qwen3-embedding-8b` supports Matryoshka-style truncation, and 4096 is the **maximum** a Neo4j vector
index accepts, so there is no headroom above and plenty below. 1024 dimensions costs ~8.2 KiB of
array and ~1 KiB of index per symbol.

```yaml
# ai-helm/charts/lightbridge-code-intelligence/values.yaml
config:
  embeddings:
    dimension: 1024
    allow_reindex_on_dim_change: true   # destructive: ADR-0018 wipes and re-indexes both stores
```

This is a **destructive** change — migration 0005 truncates `code_chunks`, and every Neo4j vector is
the wrong width until re-indexed. It needs its own change, a maintenance window, and a recall
comparison on a sample of real queries.

### S3 — Keep one source of truth for the vectors (≈ −2 to −5 GiB)

The same vector lives in three places: `code_chunks.embedding` in Postgres (source), the Neo4j
property, and the Neo4j index. Postgres already accepts that it cannot ANN-index this width —
`migrations/0005_embedding_4096.sql` says so and scopes an exact scan to one commit instead:

> No ANN index: 4096 exceeds pgvector's HNSW limit (2000 for `vector`, 4000 for `halfvec`) …
> Search is already scoped to a single (repository_id, commit_sha) … so an exact cosine scan over
> that small subset is fast enough.

If the same reasoning holds for symbols, the Neo4j copies can go entirely and the semantic half of
hybrid search can be served from Postgres, keeping Neo4j purely structural (~0.6 KiB/symbol):

```sql
-- semantic candidates from pgvector, scoped like every other retrieval query
SELECT symbol_name, file_path, start_line,
       1.0 - (embedding <=> $1::vector) AS score
FROM code_chunks
WHERE repository_id = $2 AND commit_sha = $3
ORDER BY embedding <=> $1::vector
LIMIT $4;
```

The fulltext half stays in Neo4j and the WRRF fusion moves into the control plane. It contradicts
ADR-0114's in-database fusion, so it needs an ADR amendment and a measured recall comparison — but it
is the only option that makes a 20 GiB volume comfortable for years.

### S4 — Cap transaction-log retention (≈ −1.5 to −2 GiB, chart-only)

```yaml
# charts/lightbridge-code-intelligence/values.yaml, neo4j controller env
NEO4J_db_tx__log_rotation_retention__policy: "4 hours 256M"
```

Default is `2 days 2G` ([transaction logs](https://neo4j.com/docs/operations-manual/5/database-internals/transaction-logs/)).
The graph is derived data; crash recovery means re-running the last ingest, not restoring a week.

The value is syntactically valid, checked against the documented pattern for this setting:
`^(true|keep_all|false|keep_none|(\d+[KkMmGg]?( (files|size|txs|entries|hours( \d+[KkMmGg]?)?|days( \d+[KkMmGg]?)?))))$`
— a period type plus an optional space restriction, where only `hours` and `days` may carry one.

### S5 — Fail the transaction instead of the database

A bulk `UNWIND` can exhaust the heap and panic the database. Capping transaction memory below the
container limit turns that into one failed task:

```yaml
NEO4J_db_memory_transaction_total_max: "1G"   # with a 2G heap inside a 4Gi limit
```

### S6 — Enable the JVM Vector API (CPU only, no disk)

The startup log says it plainly: *"Java vector incubator module is not readable"*. 4096-dim scoring
is running on the scalar path.

```yaml
NEO4J_server_jvm_additional: "--add-modules=jdk.incubator.vector"
```

The official image accepts only one value for `server.jvm.additional`, so confirm nothing else needs
that key before setting it.

### S7 — Make the snapshot key content-addressed (fixes the churn, §3.1)

```rust
// internal.rs — today
let commit_sha = head_sha.unwrap_or(default_branch);

// proposed: a snapshot must be keyed by a commit, never by a branch name
let Some(commit_sha) = head_sha else {
    tracing::error!(task_id = %id, "task has no head_sha; refusing to key a snapshot by branch");
    return (StatusCode::UNPROCESSABLE_ENTITY, "no head_sha").into_response();
};
```

With SHA keys, a push writes a *new* snapshot instead of rewriting the old one — so the index
sweeper becomes load-bearing, and the bounded deletes from #663 are what make that affordable. Land
S7 and the sweeper together, or the churn problem is replaced by an accumulation problem.

### S8 — Reclaim the high-water mark once

Logical deletes never shrink the files; only offline compaction does:

```bash
# the database must be OFFLINE: scale the StatefulSet to 0 and run this in a Job that
# mounts the same PVC (`--compact-node-store` defaults to false, so pass it explicitly)
neo4j-admin database copy neo4j neo4j-compacted --compact-node-store=true
```

It needs the database stopped and temporary space for a second copy, so it must come **after** the
volume has headroom. The manual's example: a store left at 31 MiB after deletions compacted to
800 KiB.

**Community Edition caveat:** multiple databases are an Enterprise feature, so `neo4j` cannot simply
be swapped for a second live database named `neo4j-compacted`. The copy has to be written to a path
(`--to-path-data` / `--to-path-txn`) and then moved over the original store while the database is
still stopped. Rehearse that move on a copy of the volume before doing it in production — this step
is the one in this report with the greatest potential to destroy the graph.

### S9 — Observability for the two things that were invisible

```promql
# PVC headroom — page at 15% free
kubelet_volume_stats_available_bytes{namespace="converse", persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}
  / kubelet_volume_stats_capacity_bytes{namespace="converse", persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}

# growth in bytes/day — the empirical scaling factor, continuously
-deriv(kubelet_volume_stats_available_bytes{persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}[6h]) * 86400
```

Plus embedding coverage per repo (the 23% gap was invisible until someone queried it), and a
**database-level health probe** — three outages, each unnoticed for days, because `tcpSocket: 7687`
passes on a panicked database.

### S10 — Housekeeping

Delete `corrupted-neostore.transaction.db` (228 KiB), watch `id-buffer.tmp.*` during bulk ingest
(38 MiB at snapshot time), and type the relationships: 287,476 edges all share one generic `REL`
type, which costs query expressiveness for no storage saving (the token store is kilobytes).

---

## 6. Capacity model

Per embedded symbol, and the projection for **all 136,350 symbols embedded** (i.e. once the backfill
completes), excluding the transaction log:

| Encoding | Array | Index | Total/symbol | All 136,350 embedded |
|---|---:|---:|---:|---:|
| **Today** (f64 property, observed index) | 36.8 KiB | 42.9 KiB | **79.7 KiB** | ~10.4 GiB |
| Today, index merged healthily (theory) | 36.8 KiB | ~4.2 KiB | 41.0 KiB | ~5.3 GiB |
| S1 int8 property | ~4.1 KiB | ~4.2 KiB | 8.3 KiB | ~1.1 GiB |
| S2 dimension 1024 | ~9.2 KiB | ~1.1 KiB | 10.3 KiB | ~1.3 GiB |
| S1 + S2 | ~1.1 KiB | ~1.1 KiB | 2.2 KiB | ~0.3 GiB |
| S3 no vectors in Neo4j | — | — | ~0.6 KiB | ~0.08 GiB |

Consistency check against the snapshot: 105,120 embedded × 79.7 KiB = **8.0 GiB**, plus 2.1 GiB of
transaction log = 10.1 GiB, against a measured `du` total of ~10.0 GiB. The unit costs reproduce the
measurement, which is the only reason to trust the rows below the first.

Forecast on the 20 GiB volume, including the ~2 GiB WAL unless S4 lands:

| Scenario | Steady state | Headroom for growth |
|---|---:|---|
| Status quo | full today | none — 1.5 GiB/day until it stalls |
| S4 + S8 only | ~6–8 GiB | months, then the same wall |
| S1 + S4 + S8 | ~2 GiB | years at current repo count |
| S3 + S4 | ~0.5 GiB | effectively unbounded |

The remaining **31,230 unembedded symbols** add **~2.4 GiB** at today's encoding (31,230 × 79.7 KiB),
or ~0.25 GiB after S1 — so the backfill alone decides whether the next fill is weeks or months away.

---

## 7. Recovery sequence for the current outage

The database is panicked **and** the disk is full, which is a deadlock: pruning and compaction are
both writes.

1. **Stop the write pressure.** Scale the dispatcher to 0 so no new index task starts; the retry
   storm is writing to a disk that cannot prune — **4,311** failed attaches in a 10-hour window on
   2026-09-27, and **4,327** in the 2026-10-04/05 window.
2. **Expand the volume** — the only action that breaks the deadlock. Size it from §6 rather than by
   doubling: 40 GiB leaves ~20 GiB free immediately (≈13 days at 1.5 GiB/day), and ~34 GiB once S8
   compaction has collapsed the duplicate segments (≈23 days). Neither is a fix; both buy the time to
   land S1/S2/S3.
   ```bash
   kubectl -n converse patch pvc data-lightbridge-ci-neo4j-0 \
     -p '{"spec":{"resources":{"requests":{"storage":"40Gi"}}}}'
   ```
3. **Restart the pod** so the filesystem resize completes and Neo4j replays its log.
4. **Land S4 immediately** (chart-only, frees ~1.5–2 GiB and bounds WAL growth).
5. **Compact (S8)** to collapse the duplicate index segments — this is what recovers the 4.2 GiB
   index to something near its 0.4 GiB theory.
6. **Then choose between S1, S2 and S3.** Without one of them, the volume refills: 1.5 GiB/day is
   ~27 days on a 40 GiB volume.
7. **S9 before anything else gets forgotten** — the probe and the disk alert are what turn the next
   occurrence into a restart instead of a four-day silent outage.

---

## 8. Verification commands

Re-run these after each step; they are the numbers this report is built on.

```bash
# disk split
kubectl -n converse exec lightbridge-ci-neo4j-0 -- sh -c \
  'df -h /data; du -sh /data/transactions/neo4j /data/databases/neo4j; \
   du -sh /data/databases/neo4j/schema/index/* | sort -h | tail'
```

```cypher
// graph shape, snapshot keys and embedding coverage
MATCH (s:Symbol)
RETURN count(*)                                             AS symbols,
       count(DISTINCT s.repo_id)                            AS repos,
       count(DISTINCT s.commit)                             AS snapshot_keys,
       sum(CASE WHEN s.embedding IS NULL THEN 0 ELSE 1 END) AS with_vector;

// is the key a SHA or a branch name?  (§3.1)
MATCH (s:Symbol) RETURN DISTINCT s.commit AS snapshot_key ORDER BY snapshot_key;

// index inventory and quantization
SHOW INDEXES YIELD name, type, properties, options;
```

```bash
# did the merge threads recover?  (§4.2)
kubectl -n converse logs lightbridge-ci-neo4j-0 | grep -c 'Lucene Merge Thread'
kubectl -n converse logs lightbridge-ci-neo4j-0 | grep -c 'needs to be restarted'
```

---

## 9. References

**Measurement:** `neo4j-usage-treemap.html`, snapshot 2026-09-29 13:40 Z, `hetzner-prod` /
`converse` / `lightbridge-ci-neo4j-0`. Generated by an AI agent from live read-only queries; one
probe was OOMKilled by the 2 GiB limit, which is finding F3 and is discussed in §3.2.

**Neo4j** — [vector indexes](https://neo4j.com/docs/cypher-manual/5/indexes/semantic-indexes/vector-indexes/)
(1–4096 dimensions, `LIST<INTEGER | FLOAT>`, M=16, ef_construction=100) ·
[transaction logs](https://neo4j.com/docs/operations-manual/5/database-internals/transaction-logs/)
(`2 days 2G` default) ·
[space reuse](https://neo4j.com/docs/operations-manual/5/performance/space-reuse/) (logical deletes,
`neo4j-admin database copy`) ·
[memory configuration](https://neo4j.com/docs/operations-manual/5/performance/memory-configuration/) ·
[PackStream](https://neo4j.com/docs/bolt/current/packstream/) (64-bit doubles only)

**Lucene** — [Lucene99HnswVectorsFormat](https://lucene.apache.org/core/9_9_0/core/org/apache/lucene/codecs/lucene99/Lucene99HnswVectorsFormat.html)

**This project** — `services/control-plane/src/integrations/neo4j.rs` ·
`services/control-plane/src/queue/index_sweeper.rs` ·
`services/control-plane/src/http/internal.rs` (snapshot key, §3.1) ·
`migrations/0005_embedding_4096.sql` · ADR-0018, ADR-0089, ADR-0114, ADR-0116 ·
[#662](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/662) ·
[`NEO4J_STORAGE_GROWTH_REPORT.md`](NEO4J_STORAGE_GROWTH_REPORT.md) (superseded where §3 says so)
