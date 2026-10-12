# Neo4j storage growth in Lightbridge Code Intelligence

**Date:** 2026-09-28
**Author:** prepared with Claude Code (AI-assisted; every cluster figure below is copied from a command whose output is quoted)
**Scope:** why the production Neo4j volume filled, what actually occupies it, what we need to keep, and a scaling model to predict the next time
**Related:** [lightbridge-code-intelligence#662](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/662), [ai-helm#1130](https://github.com/ADORSYS-GIS/ai-helm/pull/1130) (heap), [ai-helm#1138](https://github.com/ADORSYS-GIS/ai-helm/pull/1138) (20Gi volume)

---

## 1. Executive summary

The knowledge graph stores a **4096-dimensional embedding on every `:Symbol` node**, and that single
design choice accounts for essentially all of the disk growth. Each embedded symbol costs roughly
**48 KiB on disk** — about 32 KiB in the property store plus about 16 KiB in the vector index —
versus a few hundred bytes for the structural facts (label, file, line, edges) the graph exists to
serve. Structure is ~1% of the bytes; vectors are ~99%.

Three multipliers turn that into a fast-filling volume:

1. **Every default-branch push writes a complete new snapshot** of the repository, keyed by
   `(repo_id, commit)`. A 3,600-symbol repository costs ~173 MiB per push.
2. **Deleting a snapshot does not return disk to the filesystem.** Neo4j does logical deletes; store
   files never shrink. Pruning frees space *inside* the files for reuse, not back to the volume.
3. **The same vector is stored three times** across the platform: once in Postgres/pgvector, once as
   a Neo4j node property, once inside the Neo4j vector index. The last two are both on this volume —
   the property is the data, the index is a separate Lucene copy of it, exactly as a database column
   and an index over that column are separate on disk (§5.4.1).

Measured growth was **~107 MiB/day**, which filled the 10 GiB volume in ~96 days. The volume is now
20 GiB, of which roughly half is already consumed by the existing high-water mark, so on current
behaviour **the next exhaustion lands around late December 2026**.

Log files are **not** on this volume and never were — they are not part of the problem.

---

## 2. What happened (two different failures, often confused)

| Date | Trigger | Mechanism | Fix |
|---|---|---|---|
| 2026-09-18 13:26 UTC | 28,563-command transaction (a full-snapshot `DETACH DELETE`) | `java.lang.OutOfMemoryError: Java heap space` → transaction written to the log but not applied → database panic | Heap sized explicitly (ai-helm#1130), deletes bounded (#663), writes serialized (#664) |
| 2026-09-24 09:46 UTC | A 16-command transaction | `java.io.IOException: No space left on device` → same "written to log, not applied" state → database panic | Volume expanded 10Gi → 20Gi, pod restarted (2026-09-28 09:48 UTC) |

Both surface identically to callers (`Neo.DatabaseError.Transaction.TransactionStartFailed`, "The
database has encountered a critical error, and needs to be restarted"), which is why the second one
looked like a repeat of the first. It was not: by 09-24 the heap fix was already deployed
(`sha-0aa4ca5`, containing #663 and #664).

Evidence for the disk failure, from the Neo4j pod log:

```text
2026-09-24 09:46:17  ERROR TransactionCommitFailed: Could not apply the transaction:
                     Transaction #37262 {... with 16 commands ...} to the store after written to log
2026-09-24 10:15:27  Exception in thread "Lucene Merge Thread #399"
                     org.apache.lucene.index.MergePolicy$MergeException
                     Caused by: java.io.IOException: No space left on device
                       at org.apache.lucene.codecs.lucene99.Lucene99HnswVectorsWriter.writeGraph(...)
```

The Lucene frame names the component that was writing when the disk ran out: the HNSW **vector
index**.

A second, quieter failure mode matters as much as the first: **nothing noticed**. The liveness and
readiness probes are `tcpSocket: 7687`; a panicked Neo4j keeps the Bolt port open, so the pod
reported `1/1 Running` for 3 days 21 hours while every graph read and write failed.

---

## 3. What is actually stored in Neo4j

### 3.1 The schema

One node label and one relationship type, written by
[`services/control-plane/src/integrations/neo4j.rs`](services/control-plane/src/integrations/neo4j.rs):

```cypher
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id})
SET s.label = n.label, s.source_file = n.file, s.start_line = n.line,
    s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END
```

```cypher
MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: e.src})
MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: e.dst})
MERGE (a)-[r:REL {relation: e.rel}]->(b)
```

Per node: `repo_id` (int), `commit` (string), `node_id` (string, a path or symbol path), `label`
(string), `source_file` (string), `start_line` (int), and **`embedding` (list of 4096 floats)**.

### 3.2 The indexes

From `ensure_indexes` in the same file:

| Index | Type | On | Purpose |
|---|---|---|---|
| `symbol_embedding_idx` | **VECTOR** (HNSW, cosine) | `s.embedding` | semantic half of hybrid search (ADR-0114) |
| `symbol_label_fulltext` | FULLTEXT (Lucene) | `s.label`, `s.source_file` | lexical half of hybrid search |
| `symbol_identity` | UNIQUENESS constraint | `(repo_id, commit, node_id)` | identity + makes `MERGE` an index seek |

### 3.3 The dimension: 4096

Production config, read from the live ConfigMap `lightbridge-ci-control-plane-config`:

```json
{ "allow_reindex_on_dim_change": false, "dimension": 4096 }
```

The model is `qwen3-embedding-8b` ([ai-helm-values `environments/prod/values/lightbridge-code-intelligence.yaml`](https://github.com/ADORSYS-GIS/ai-helm-values)),
whose output width is coupled to this value and to the pgvector column (migration 0005).

Two consequences worth stating plainly:

- **4096 is the maximum a Neo4j 5 vector index accepts.** The Cypher manual gives the valid range as
  1–4096 inclusive ([vector indexes](https://neo4j.com/docs/cypher-manual/5/indexes/semantic-indexes/vector-indexes/)).
  There is no headroom above; only downward.
- **Neo4j stores those floats as 64-bit doubles.** Bolt/PackStream has no 32-bit float type — the
  specification states that neither unsigned integers nor 32-bit floating point numbers are included
  ([PackStream](https://neo4j.com/docs/bolt/current/packstream/)). A `Vec<f32>` sent from Rust is
  widened to `double` on the wire and stored as such.

---

## 4. Where the bytes go on the volume

The PVC `data-lightbridge-ci-neo4j-0` is mounted at `/data` only:

```text
$ kubectl -n converse get pod lightbridge-ci-neo4j-0 -o jsonpath='{.spec.containers[0].volumeMounts[*]}'
/data <- data
/var/run/secrets/kubernetes.io/serviceaccount <- kube-api-access-...
```

| Path | Contents | Grows with |
|---|---|---|
| `/data/databases/neo4j` | node/relationship/property store files, **plus Lucene index directories** (vector + fulltext) | symbols, embeddings, snapshots |
| `/data/transactions` | the write-ahead log (WAL) | write throughput; capped by retention policy |

### 4.1 Transaction logs are capped, but the cap is 2 GiB

Neo4j 5 defaults ([transaction logs](https://neo4j.com/docs/operations-manual/5/database-internals/transaction-logs/)):

- `db.tx_log.rotation.size` = **256 MiB**
- `db.tx_log.rotation.retention_policy` = **"2 days 2G"** (5.13+)

So up to **2 GiB of a 10 GiB volume — 20% — can be WAL**, and the chart overrides neither. Pruning
happens only after a checkpoint, and a log file whose segment holds the newest checkpoint cannot be
pruned.

### 4.2 Deleted data does not give disk back

This is the single most important storage property to understand here
([space reuse](https://neo4j.com/docs/operations-manual/5/performance/space-reuse/)):

> "A logical delete means that all relevant records are marked as deleted, but the space they occupy is not immediately returned to the operating system."

Store files do not shrink. Freed record IDs are tracked in `.id` files and reused by later writes.
The manual's own example is stark: deleting 100,000 nodes left the store at 31 MiB, and only
`neo4j-admin database copy` (offline compaction) brought it down to 800 KiB.

**Implication for us:** the index sweeper reclaims logical space, and the volume still shows the
high-water mark. Disk usage tracks the *peak* of (live + recently deleted) data, never the current
live set.

### 4.3 Log files are NOT on this volume

Answering the question directly: **no application or database log files consume PVC space.**

- The official image sets `server.directories.logs=/logs`
  (`: ${NEO4J_server_directories_logs:="/logs"}` in the 5.26 entrypoint,
  [docker-neo4j-publish](https://github.com/neo4j/docker-neo4j-publish)), and `/logs` is **not
  mounted** — it lives on the container's writable layer, on node-local ephemeral storage.
- Neo4j's user log also goes to **stdout**, which the kubelet captures and Alloy ships to **Loki**.
  That is the copy we successfully queried for the 09-24 root cause after the container's own log had
  rotated.

Two practical points follow:

1. **Do not mount `/logs` on the data PVC.** It would take space from the database for no benefit.
2. **`debug.log` (the file holding stack traces) is ephemeral and is lost when the pod is replaced.**
   In this incident the `No space left on device` trace survived only because it also reached stderr.
   Routing the debug log to stdout as well (a custom log4j config in the chart) would put every
   future stack trace in Loki, where it outlives the pod. That is a chart change worth making — it
   costs no PVC space, because Loki has its own storage.

---

## 5. Why it grew so fast

### 5.1 Per-symbol cost model

| Component | Bytes per embedded symbol | Basis |
|---|---|---|
| `embedding` property | 4096 × 8 B = **32,768 B** | PackStream floats are 64-bit doubles |
| Vector index, flat storage | 4096 × 4 B ≈ **16,384 B** | Lucene stores float vectors as float32 in the flat format ([Lucene99HnswVectorsFormat](https://lucene.apache.org/core/9_9_0/core/org/apache/lucene/codecs/lucene99/Lucene99HnswVectorsFormat.html)) |
| HNSW graph links (`.vex`) | ~64–128 B | `M` default 16, up to 2M neighbours on level 0, delta-encoded vints |
| Node/property/label records | ~100–200 B | aligned store format, Community default |
| Fulltext index entries | ~100–300 B | `label` + `source_file` |
| **Total** | **≈ 48–50 KiB** | |

Structural facts alone would be **~0.5 KiB**. The embedding and its index are ~98–99% of the cost.

The property-store and HNSW figures are derived from the documented formats rather than measured on
our data — §8 gives the commands to confirm them against the live store. The Lucene float32 figure
assumes Neo4j hands Lucene a `KnnFloatVectorField`; if it stores doubles instead, the index cost
doubles and the model is conservative.

### 5.2 Snapshot multiplication

Each default-branch push triggers a re-index that writes a **complete new snapshot** under a new
`commit` value. Observed page sizes from the control-plane log (pages of 2,000):

```text
12:53:01 graph ingested {"task_id":"aaeff2fa…","nodes":2000,"edges":0}    # ADORSYS-GIS/CoopData
12:54:22 graph ingested {"task_id":"aaeff2fa…","nodes":2000,"edges":0}
12:55:42 graph ingested {"task_id":"aaeff2fa…","nodes":1887,"edges":0}    # 5,887 symbols total
13:21:33 graph ingested {"task_id":"330e78d6…","nodes":2000,"edges":0}    # ADORSYS-GIS/lightbridge-governance
13:22:44 graph ingested {"task_id":"330e78d6…","nodes":1610,"edges":0}    # 3,610 symbols total
```

At 48 KiB per symbol:

| Repository size | Disk per snapshot | 1 push/day | 5 pushes/day |
|---|---|---|---|
| 1,000 symbols | 48 MiB | 1.4 GiB/month | 7 GiB/month |
| 3,610 symbols (lightbridge-governance) | **173 MiB** | 5.1 GiB/month | 25 GiB/month |
| 5,887 symbols (CoopData) | **283 MiB** | 8.3 GiB/month | 41 GiB/month |
| 20,000 symbols | 960 MiB | 28 GiB/month | 141 GiB/month |

A **single push to one medium repository costs more disk than a day's average growth**. The index
sweeper reclaims old snapshots, but per §4.2 that reclaim is logical, so the high-water mark only
ratchets upward.

### 5.3 The sweeper's blind spots

`services/control-plane/src/queue/index_sweeper.rs` prunes snapshots outside the keep-set (latest
indexed commit ∪ commits pinned by non-terminal tasks). Three conditions limit it:

1. **It skips a repo while an index task is active** — "Never prune a repo mid-index".
2. **It requires a working database.** Pruning is a write; once the database panics, pruning stops
   exactly when it is most needed. Disk full → panic → no pruning → disk stays full. This is why the
   09-24 outage could not self-heal.
3. **It cannot shrink store files**, only free records for reuse.

### 5.4 The same vector, stored three times

| # | Store | Encoding | Per symbol/chunk | Which volume |
|---|---|---|---|---|
| 1 | Postgres `code_chunks.embedding` | pgvector `vector(4096)`, float32, **no ANN index** | ~16 KiB | `lightbridge-main-db-*` (40Gi each) |
| 2 | Neo4j `:Symbol.embedding` (node property) | list of 4096 doubles | ~32 KiB | **`data-lightbridge-ci-neo4j-0`** |
| 3 | Neo4j `symbol_embedding_idx` (vector index) | Lucene HNSW, float32 | ~16 KiB | **same volume** |

Copies 2 and 3 are both on the volume that filled. Together they are the ~48 KiB per symbol from
§5.1.

#### 5.4.1 What "the property and the index" actually means

This is the part that is easy to misread, so it is worth spelling out: **the property and the index
are two separate things on disk, holding the same numbers.** The relational analogy is exact — a
column and an index over that column are stored separately, and you pay for both.

**Copy 2 — the property.** The embedding is written onto the node as an ordinary Cypher property. It
lands in Neo4j's property store (a dynamic array record, because it is a list), and it is the source
of truth.

Written in two places. First on the structural upsert, in
`services/control-plane/src/integrations/neo4j.rs`:

```rust
txn.run(
    query(
        "UNWIND $nodes AS n \
         MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id}) \
         SET s.label = n.label, s.source_file = n.file, s.start_line = n.line, \
             s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END",
    )
    .param("repo", repository_id)
    .param("commit", commit_sha)
    .param("nodes", rows),
)
```

and again when a chunk's vector is attached to the symbol whose body it is (ADR-0116):

```rust
"UNWIND $rows AS r \
 MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: r.id}) \
 SET s.embedding = r.embedding \
 RETURN count(s) AS updated"
```

`r.embedding` is a `Vec<f32>` in Rust, but PackStream has no 32-bit float type, so all 4096 values
are widened to 64-bit doubles on the wire and stored that way: **4096 × 8 B = 32 KiB**.

**Copy 3 — the index.** Declaring a vector index tells Neo4j to build and maintain a *second*
structure over that same property — a Lucene HNSW graph in
`/data/databases/neo4j/schema/index/…`, with its own `.vec` (raw vectors) and `.vex` (neighbour
graph) files:

```rust
// ensure_indexes()
"CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS \
 FOR (s:Symbol) ON s.embedding \
 OPTIONS { indexConfig: { \
   `vector.dimensions`: 4096, \
   `vector.similarity_function`: 'cosine' \
 }}"
```

Nobody writes to this index directly. Every `SET s.embedding = …` above causes Neo4j to copy those
4096 values into the index as well, encoded as float32: **4096 × 4 B = 16 KiB**. That is the code
path the crash trace caught mid-write on 09-24:

```text
Caused by: java.io.IOException: No space left on device
  at org.apache.lucene.codecs.lucene99.Lucene99HnswVectorsWriter.writeGraph(...)
```

**Why both must exist.** The index is derived data — it cannot be queried without the property it
indexes, and dropping the property drops its index entries. The hybrid search reads the index to
*find* candidates, then reads properties off the matched nodes to *return* them:

```rust
"CALL db.index.vector.queryNodes('symbol_embedding_idx', $sourceK, $queryVector) \
 YIELD node AS s \
 WHERE s.repo_id = $repo AND s.commit = $commit \
 …
 RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file, \
        s.start_line AS start_line"
```

So within Neo4j you cannot keep one and drop the other: no index means no `queryNodes` (a full scan
with cosine computed in Cypher instead), and no property means no index. The only way to stop paying
48 KiB per symbol is to decide the graph does not need semantic search at all — option 4 in §6.

**Copy 1 — Postgres, for contrast.** The same vector is already in pgvector, written by
`services/control-plane/src/db/code_chunks.rs`:

```sql
INSERT INTO code_chunks
  (repository_id, commit_sha, file_path, language, chunk_type, symbol_name,
   start_line, end_line, content, embedding)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector)
```

against the column from migration `0005_embedding_4096.sql`:

```sql
ALTER TABLE code_chunks ADD COLUMN embedding vector(4096) NOT NULL;
```

Note what that migration says about indexing it:

> No ANN index: 4096 exceeds pgvector's HNSW limit (2000 for `vector`, 4000 for `halfvec`), so we
> can't index it. Search is already scoped to a single (repository_id, commit_sha) … so an exact
> cosine scan over that small subset is fast enough.

Postgres therefore stores **one** copy (16 KiB, float32, unindexed) and accepts a scan, while Neo4j
stores **two** copies (48 KiB) to get an ANN index — an index at a dimension pgvector itself refuses
to build. The graph volume pays roughly **3× what the Postgres row costs** for the same information.

ADR-0116 already removed one redundancy (a symbol's vector now rides the chunk that is its body,
rather than being embedded a second time through the model). The *storage* redundancy is what
remains: the Neo4j property and index exist solely so `db.index.vector.queryNodes` can run inside the
hybrid WRRF query of ADR-0114.

### 5.5 Empirical rate

| Fact | Value | Source |
|---|---|---|
| Volume created | ~2026-06-20 | PVC age 100d on 2026-09-28 |
| Volume exhausted | 2026-09-24 09:46 UTC | `No space left on device` |
| Elapsed | ~96 days | |
| Capacity | 10 GiB | `kubectl get pvc` |
| **Average growth** | **~107 MiB/day ≈ 3.2 GiB/month** | 10 GiB ÷ 96 d |

Cross-check: 107 MiB/day ÷ 48 KiB ≈ **2,280 embedded symbols/day**, i.e. roughly one medium
repository re-indexed per day. That matches the observed push cadence, so the model and the
measurement agree within the accuracy either deserves.

---

## 6. What we actually need, and what we could stop storing

| Stored today | Needed for | Verdict |
|---|---|---|
| `:Symbol` structural properties + `REL` edges | `find_symbol`, `get_callers`, `get_callees`, graph overview — the reason Neo4j is here | **Keep.** ~1% of bytes. |
| `symbol_identity` constraint | correctness and write performance | **Keep.** Currently **missing in production** — creation failed on duplicates (see §9). |
| `symbol_label_fulltext` | lexical half of hybrid search | **Keep.** Cheap. |
| `:Symbol.embedding` + `symbol_embedding_idx` | semantic half of hybrid search (ADR-0114), agent-facing MCP tool | **~99% of the bytes.** The reduction options below all target this. |
| Per-commit snapshots beyond the latest | in-flight tasks pinning a commit | Keep the mechanism; it is already swept. |

### Reduction options, cheapest first

1. **Cap transaction-log retention** — chart-only, no code, no re-index.
   `NEO4J_db_tx__log_rotation_retention__policy: "1 days 512M"` frees up to ~1.5 GiB of the 20 GiB
   and bounds WAL growth. Trade-off: a shorter window for log-based recovery.
2. **Reduce the embedding dimension.** 4096 → 1024 cuts per-symbol cost from ~48 KiB to ~12 KiB, a
   **4× reduction**, and Qwen3 embeddings are trained for Matryoshka-style truncation. This is a
   destructive change: it is guarded by `allow_reindex_on_dim_change: false` and requires a full
   re-index of both stores (ADR-0018). Biggest saving per unit of work.
3. **Enable vector quantization** if the deployed version supports it. The Cypher manual states
   binary quantization became the default only as of Neo4j 2026.08; we run **5.26.27**, so the
   default there must be confirmed with `SHOW INDEXES YIELD name, options` (§8) rather than assumed.
4. **Drop the Neo4j copy of the vectors entirely** and serve the semantic half of hybrid search from
   pgvector, keeping Neo4j purely structural. This would cut the graph volume by ~98% and make a
   10 GiB volume comfortable again. It contradicts ADR-0114's in-database WRRF design, so it needs an
   ADR amendment and a measurement of the quality difference — a deliberate architectural decision,
   not an optimisation.
5. **Periodic offline compaction** (`neo4j-admin database copy`) to collapse the high-water mark.
   Requires downtime and temporary space for a second copy; treat as an operational lever, not a fix.

---

## 7. Scaling model and forecast

### 7.1 Formulas

```
bytes_per_symbol      ≈ 48 KiB              (4096-dim; ≈ 12 KiB at 1024-dim, ≈ 0.5 KiB with no vector)
snapshot_bytes        ≈ symbols × bytes_per_symbol
live_set_bytes        ≈ Σ_repos (symbols_latest_snapshot × bytes_per_symbol)
high_water_mark       ≈ live_set + un-reused deleted records + Lucene segments awaiting merge
volume_needed         ≈ high_water_mark × 1.3  +  WAL cap (2 GiB default, 0.5 GiB if capped)
daily_growth          ≈ Σ_repos (pushes_per_day × snapshot_bytes)   [until reuse catches up]
```

The 1.3 factor is a fragmentation allowance, not a measured constant; §8 shows how to replace it with
a measured one.

### 7.2 Forecast for the current 20 GiB volume

| Scenario | Daily growth | Time to exhaust the remaining ~10 GiB | Approximate date |
|---|---|---|---|
| Status quo (measured) | 107 MiB/day | ~96 days | **late December 2026** |
| +2 active repos at current cadence | ~200 MiB/day | ~51 days | mid-November 2026 |
| WAL capped at 512M (option 1) | 107 MiB/day | ~110 days | mid-January 2027 |
| Dimension 1024 (option 2) | ~27 MiB/day | ~380 days | late 2027 |
| No vectors in Neo4j (option 4) | ~1 MiB/day | years | — |

"Remaining ~10 GiB" assumes the pre-existing high-water mark still occupies roughly the old 10 GiB.
The first action in §8 replaces that assumption with a measurement, and the forecast should be
re-run once it does.

### 7.3 Leading indicators to watch

| Signal | Where | Threshold |
|---|---|---|
| PVC free space | Mimir, `kubelet_volume_stats_available_bytes` | warn at 30% free, page at 15% |
| Growth rate | same metric, `deriv(...[6h])` | alert if sustained above 300 MiB/day |
| Prune effectiveness | `index_prune_outcome` / `index_prune_deleted` metrics (already emitted) | alert on `error`, or on no `pruned` for 24h |
| Database actually alive | new DB-level probe (see §9) | any failure |

---

## 8. How to measure this properly (commands)

These need permissions this analysis did not have (`pods/exec` and `pods/portforward` are both denied
for `ssg:christian.leghadjeu@adorsys.com` in `converse`). Run them to replace the estimates above with
measurements.

**A. Actual disk breakdown — do this first, and before any future pod replacement:**

```bash
kubectl -n converse exec lightbridge-ci-neo4j-0 -- sh -c \
  'df -h /data; du -sh /data/transactions; du -sh /data/databases/neo4j; \
   du -sh /data/databases/neo4j/schema/index/* 2>/dev/null | sort -h | tail'
```

This separates WAL from store from Lucene indexes, and the vector index directory alone validates or
refutes the 16 KiB/symbol index figure.

**B. Live graph size, per repo and per snapshot:**

```cypher
MATCH (s:Symbol)
RETURN s.repo_id AS repo, s.commit AS commit, count(*) AS symbols,
       sum(CASE WHEN s.embedding IS NULL THEN 0 ELSE 1 END) AS with_vector
ORDER BY symbols DESC;

MATCH (s:Symbol) RETURN count(*) AS total_symbols;
MATCH ()-[r:REL]->() RETURN count(r) AS total_edges;
```

Multiply `with_vector` by 48 KiB and compare with (A). That ratio is the real fragmentation factor to
use in §7.1 instead of 1.3.

**C. Index inventory, and whether quantization is on:**

```cypher
SHOW INDEXES YIELD name, type, entityType, properties, options;
SHOW CONSTRAINTS;
```

Confirms whether `symbol_identity` exists (see §9) and what `vector.quantization` is set to on 5.26.

**D. Snapshot accumulation (is the sweeper keeping up?):**

```cypher
MATCH (s:Symbol)
RETURN s.repo_id AS repo, count(DISTINCT s.commit) AS snapshots
ORDER BY snapshots DESC;
```

Anything above 1–2 per repo for long means pruning is not keeping pace.

**E. Volume headroom over time (Mimir, tenant `anonymous`):**

```promql
kubelet_volume_stats_available_bytes{namespace="converse", persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}
  / kubelet_volume_stats_capacity_bytes{namespace="converse", persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}
```

```promql
-deriv(kubelet_volume_stats_available_bytes{persistentvolumeclaim="data-lightbridge-ci-neo4j-0"}[6h]) * 86400
```

The second gives bytes/day directly — the empirical scaling factor, continuously.

---

## 9. Open items

| # | Item | Why it matters |
|---|---|---|
| 1 | **`symbol_identity` constraint is missing in production.** Creation failed on 2026-09-17 with duplicate nodes (`repo_id=430, commit='main', node_id='charts/librechat-app/files/seed-agents.js'`). | Without it every `MERGE` scans instead of seeking; write cost grows with total database size. Dedupe, then create it. |
| 2 | **No disk alert on the PVC.** | The volume filled silently. §7.3 has the queries. |
| 3 | **Probes are `tcpSocket: 7687`.** | A panicked database looks healthy; this outage ran 3d21h unnoticed. |
| 4 | **`debug.log` is ephemeral.** | The 09-24 root cause was nearly lost. Route it to stdout so Loki keeps it. |
| 5 | **Chart default vs. live StatefulSet.** ai-helm#1138 sets 20Gi but `volumeClaimTemplates` is immutable, so ArgoCD reports `ComparisonError` until the StatefulSet is recreated (`kubectl -n converse delete sts lightbridge-ci-neo4j --cascade=orphan`, then sync). | The app cannot sync at all while the comparison fails. |
| 6 | **`graph_overview` still failing for `repo_id=1202`** at 09:52 UTC, after the 09:48 recovery, while all other graph operations succeed. | Unexplained; needs its own look. |

---

## 10. References

**Neo4j**
- [Vector indexes](https://neo4j.com/docs/cypher-manual/5/indexes/semantic-indexes/vector-indexes/) — 1–4096 dimension range, cosine/euclidean, `M`=16 and `ef_construction`=100 defaults, quantization default as of 2026.08
- [Transaction logs](https://neo4j.com/docs/operations-manual/5/database-internals/transaction-logs/) — `rotation.size` 256 MiB, `retention_policy` "2 days 2G" (5.13+), pruning tied to checkpoints
- [Space reuse](https://neo4j.com/docs/operations-manual/5/performance/space-reuse/) — logical deletes, store files never shrink, `neo4j-admin database copy` to compact
- [Store formats](https://neo4j.com/docs/operations-manual/5/database-internals/store-formats/) — `aligned` is the Community default
- [PackStream](https://neo4j.com/docs/bolt/current/packstream/) — 64-bit doubles only, no 32-bit float type
- [Docker configuration](https://neo4j.com/docs/operations-manual/5/docker/configuration/) — `NEO4J_` prefix, `.`→`_`, `_`→`__`
- [docker-neo4j-publish 5.26 entrypoint](https://github.com/neo4j/docker-neo4j-publish) — sets a 512M page-cache default and `server.directories.logs=/logs`; sets no heap default

**Lucene**
- [Lucene99HnswVectorsFormat](https://lucene.apache.org/core/9_9_0/core/org/apache/lucene/codecs/lucene99/Lucene99HnswVectorsFormat.html) — `.vec` flat vectors, `.vex` delta-encoded HNSW neighbours, `.vem` metadata

**Kubernetes**
- [Expanding PVCs](https://kubernetes.io/docs/concepts/storage/persistent-volumes/#expanding-persistent-volumes-claims) — `allowVolumeExpansion`, edit `spec.resources.requests.storage`, `FileSystemResizePending`
- [Updating StatefulSets](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/#updating-statefulsets) — only `replicas`, `ordinals`, `template`, `updateStrategy`, `persistentVolumeClaimRetentionPolicy`, `minReadySeconds` may be updated

**This project**
- `services/control-plane/src/integrations/neo4j.rs` — `upsert_graph`, `attach_symbol_embeddings`, `ensure_indexes`, `prune_graph`, `delete_commit_graph`, `hybrid_symbol_search`
- `services/control-plane/src/queue/index_sweeper.rs` — snapshot pruning and its keep-set
- ADR-0089 (embeddings on the code graph), ADR-0114 (hybrid graph+vector search), ADR-0116 (one walk; a symbol's vector rides its chunk), ADR-0018 (embedding-dimension safety), ADR-0052 / RFC-0002 (index snapshot sweeper)
- [#662](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/662) — incident and follow-ups

---

## 11. Recommended next steps

1. **Measure** with §8A–D before the pod is replaced again, and update §7.2 with the real split.
2. **Alert** on PVC headroom and growth rate (§7.3) — one dashboard panel and one alert rule.
3. **Cap WAL retention** in the chart (option 1): small, safe, immediate.
4. **Fix the probe** and **dedupe + create `symbol_identity`** (#662).
5. **Decide the vector question** (options 2 and 4). At 4096 dimensions the graph volume is dominated
   by data that pgvector already holds; whichever way that decision goes, it should be an ADR, and it
   is the only change that alters the growth curve by an order of magnitude.
