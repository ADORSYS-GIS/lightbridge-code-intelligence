# 4. pgvector and dual retrieval

Files `02` and `03` covered Postgres-as-queue and Neo4j-as-graph. This chapter is the second half of
ADR-0003's "dual retrieval": `code_chunks`, the pgvector table that answers *semantic* questions
("where is similar behavior implemented") the way file `03`'s graph answers *structural* ones ("what
calls this function"). It's grounded in
`services/control-plane/src/db/code_chunks.rs`, migrations `0004` and `0005`, and
[ADR-0003](../docs/adr/0003-dual-retrieval-neo4j-pgvector.md) /
[ADR-0050](../docs/adr/0050-retrieval-pins-to-latest-indexed-snapshot.md).

## 4.1 Why two stores — two different questions, not one store done twice

ADR-0003 states the problem directly: "Good code review needs two different kinds of recall:
*structural* (what calls this function, which tests cover it, what imports what) and *semantic*
(where is similar behavior implemented, which docs discuss this feature). A single store handles one
of these well and the other poorly." Its decision table is blunt about the tradeoff: "Neo4j only —
strong on topology, weak as a semantic similarity store," "pgvector only — easy operationally, but
loses relationships and topology." The chosen option keeps both, "used as complementary indexes
rather than interchangeable ones" — the indexing pipeline fans out to both from a single parse pass
over the same repo checkout, so a reviewer asking "who else calls this" gets the graph, and a
reviewer asking "has something like this been built before in this repo" gets the vector store, and
neither question is answerable well by the other store.

## 4.2 The `code_chunks` table

### Where it started: 1536 dimensions, HNSW-indexed

`0004_code_chunks.sql` created it to match `text-embedding-3-small` (ADR-0018):

```sql
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS code_chunks (
    id            BIGSERIAL PRIMARY KEY,
    repository_id BIGINT  NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    commit_sha    TEXT    NOT NULL,
    file_path     TEXT    NOT NULL,
    language      TEXT    NOT NULL,
    chunk_type    TEXT    NOT NULL,
    symbol_name   TEXT,
    start_line    INT     NOT NULL,
    end_line      INT     NOT NULL,
    content       TEXT    NOT NULL,
    embedding     vector(1536) NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (repository_id, commit_sha, file_path, start_line, end_line)
);

CREATE INDEX IF NOT EXISTS code_chunks_embedding_idx
    ON code_chunks USING hnsw (embedding vector_cosine_ops);

CREATE INDEX IF NOT EXISTS code_chunks_commit_idx
    ON code_chunks (repository_id, commit_sha);
```

One row per semantic unit — a function, a class, an impl block — or a windowed fallback chunk for
code the parser can't decompose that finely. The `UNIQUE (repository_id, commit_sha, file_path,
start_line, end_line)` constraint is what makes re-indexing the same commit an upsert rather than a
pile of duplicate rows (§4.4 shows the `ON CONFLICT` that relies on it). `code_chunks_embedding_idx`
is an **HNSW** approximate-nearest-neighbor index, standard pgvector practice for fast similarity
search at scale.

### Why that HNSW index is gone: the 4096-dimension switch

`0005_embedding_4096.sql`, quoted in full because every line is load-bearing:

```sql
-- Switch the embedding column to 4096 dimensions for `qwen3-embedding-8b` — the model the eaig
-- gateway actually serves (text-embedding-3-small / 1536 from migration 0004 is not configured
-- there; verified live 2026-06-19). Supersedes the dimension in ADR-0018.
--
-- No ANN index: 4096 exceeds pgvector's HNSW limit (2000 for `vector`, 4000 for `halfvec`), so we
-- can't index it. Search is already scoped to a single (repository_id, commit_sha) via
-- `code_chunks_commit_idx`, so an exact cosine scan over that small subset is fast enough.

-- Existing 1536-d rows are invalid for the new model; clear them (re-indexing repopulates).
TRUNCATE TABLE code_chunks;

DROP INDEX IF EXISTS code_chunks_embedding_idx;

ALTER TABLE code_chunks DROP COLUMN IF EXISTS embedding;
ALTER TABLE code_chunks ADD COLUMN embedding vector(4096) NOT NULL;
```

This is a real, documented pgvector limit, not a project-specific choice: HNSW indexes in pgvector
cap out at 2000 dimensions for the plain `vector` type (4000 for the lower-precision `halfvec`), and
`qwen3-embedding-8b` — the model actually configured on the organization's embeddings gateway, as
opposed to the `text-embedding-3-small` ADR-0018 had originally assumed — produces 4096-dimensional
vectors. 4096 > 2000, so an HNSW index on this column is **not possible**, full stop; the migration
drops the index rather than leaving a dead one around. The comment's justification for why this is
survivable rather than a performance cliff is the same scoping discipline file `03` showed for Neo4j:
every real query against this table is already filtered to one `(repository_id, commit_sha)`
snapshot via `code_chunks_commit_idx`, so the *exact* (not approximate) cosine scan `search_code_chunks`
runs (§4.3) only ever scans one repo's one snapshot's chunks — typically hundreds to a few thousand
rows — never the whole table. An exact scan over a few thousand rows is fast; an exact scan over
every chunk ever indexed, across every repo and every historical commit, would not be.

Note also: `TRUNCATE TABLE code_chunks` — switching dimensions is destructive by construction (a
1536-float vector literally cannot be reinterpreted as a 4096-float one), so every previously indexed
repository loses its semantic index and has to be re-indexed from scratch after this migration lands.
This isn't a one-time historical fact either — it's a live operational hazard the running code still
guards against on every startup. `db::reconcile_embedding_dimension`
(`services/control-plane/src/db/mod.rs:91-139`) checks the *live* column width (`pg_attribute.
atttypmod`) against the configured `embeddings.dimension` on every boot, and if they disagree:

```rust
if !allow_clear {
    bail!(
        "embedding dimension changed ({current} → {dimension}) but \
         embeddings.allow_reindex_on_dim_change is false; refusing to wipe code_chunks. \
         Set the flag to reindex from scratch, or revert the dimension."
    );
}
```

A config typo that would silently wipe the semantic index instead makes the process refuse to start
— the operator has to explicitly set `embeddings.allow_reindex_on_dim_change` to confirm "yes, truncate
and re-index everything" before the TRUNCATE+ALTER (run atomically in one transaction,
`db/mod.rs:126-137`, "so a failed ALTER can't leave the table truncated but still at the old width")
is allowed to run again.

## 4.3 The search query, verbatim

`search_code_chunks` (`services/control-plane/src/db/code_chunks.rs:252-274`):

```rust
pub async fn search_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
    query_embedding: &[f32],
    limit: i64,
) -> Result<Vec<CodeChunkHit>, sqlx::Error> {
    let emb = vector_literal(query_embedding);
    sqlx::query_as::<_, CodeChunkHit>(
        "SELECT file_path, language, chunk_type, symbol_name, start_line, end_line, content, \
                1.0 - (embedding <=> $1::vector) AS score \
         FROM code_chunks \
         WHERE repository_id = $2 AND commit_sha = $3 \
         ORDER BY embedding <=> $1::vector \
         LIMIT $4",
    )
    .bind(&emb)
    .bind(repository_id)
    .bind(commit_sha)
    .bind(limit)
    .fetch_all(pool)
    .await
}
```

`<=>` is pgvector's **cosine distance** operator — `0` for identical direction, up to `2` for
opposite. `1.0 - (embedding <=> $1::vector) AS score` converts that distance into a similarity in
roughly `[-1, 1]` (in practice `[0, 1]` for embeddings that cluster in one hemisphere, which text
embedding models typically do), where higher means closer — the doc comment on `CodeChunkHit::score`
confirms: "Cosine similarity in `[0,1]`; higher is closer." `ORDER BY embedding <=> $1::vector` sorts
ascending by *distance* (closest first) — note it is **not** ordering by the computed `score` column,
it's re-deriving the same comparison the `SELECT` already computed, which is how pgvector lets the
planner potentially use an index on that expression (moot here since there is no index on this
column, per §4.2, but it's the idiomatic form regardless).

`WHERE repository_id = $2 AND commit_sha = $3` is the snapshot scope — the same discipline as every
Neo4j query in file `03`. The doc comment on the function is explicit about *why* this scoping is
non-negotiable: "a task only ever sees its own repo's index — the caller never picks the scope (trust
boundary)." The caller here is the internal retrieval API the agent's mediated search tool calls;
the scope itself is resolved server-side from the task's own repository and pinned commit, never
taken as a parameter the caller could widen.

### The embedding literal — no extra crate needed

`vector_literal` (`code_chunks.rs:219-231`):

```rust
fn vector_literal(v: &[f32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(v.len() * 12 + 2);
    s.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{f}");
    }
    s.push(']');
    s
}
```

pgvector's text representation of a vector is just `[f0,f1,f2,...]`; this function renders that text
by hand and relies on the `$N::vector` cast in the SQL to parse it server-side. No pgvector-specific
Rust crate is pulled in anywhere — both ingestion (`upsert_code_chunks`, which does the same
`$10::vector` cast) and search go through this one hand-rolled formatting function.

## 4.4 Writing: the upsert

`upsert_code_chunks` (`services/control-plane/src/db/code_chunks.rs:174-215`), one `INSERT ...
ON CONFLICT ... DO UPDATE` per chunk inside a single transaction:

```sql
INSERT INTO code_chunks
(repository_id, commit_sha, file_path, language, chunk_type, symbol_name,
 start_line, end_line, content, embedding)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector)
ON CONFLICT (repository_id, commit_sha, file_path, start_line, end_line)
DO UPDATE SET
  language    = EXCLUDED.language,
  chunk_type  = EXCLUDED.chunk_type,
  symbol_name = EXCLUDED.symbol_name,
  content     = EXCLUDED.content,
  embedding   = EXCLUDED.embedding
```

The `ON CONFLICT` target is exactly the `UNIQUE` constraint from §4.2's `0004` migration — re-running
the indexer against a commit it already indexed updates the existing rows for any span whose bounds
are unchanged, rather than erroring or duplicating.

## 4.5 Snapshot pinning — ADR-0050

This is the chapter's second major idea, and it only makes sense once you've seen how `commit_sha`
scoping works in §4.3: *which* commit does retrieval actually pin to?

### The naive answer was wrong twice

ADR-0050's Context section narrates the history concretely. The *first* naive approach checked
"does this repo have any indexed chunks at all" with no commit scoping — which meant an index built
at commit A would report "indexed" even while a search pinned to commit B (a PR head, say) returned
**zero** hits: a hollow index, observed live on a real dogfood run (`7c15f9bb`). The fix for that
(an earlier change, #188) made the check commit-scoped — index at commit A, search at commit A — but
that just moved the cost: since the semantic index is maintained on the **default branch** by
re-index-on-push, it only ever has chunks *at the default-branch commit*, never at a PR's head. Pin
retrieval to the PR head and `repo_has_index(head_sha)` is *always* false, so "every PR review (and
every push to a PR) runs a full re-index before reviewing" — observed on run `590af322`: "~1170
chunks + graph re-indexed, ~3.5 min, on a repo that was already indexed."

### The actual fix: pin to the latest snapshot, not the PR head

`latest_indexed_commit` (`services/control-plane/src/db/code_chunks.rs:18-55`) — **rewritten by
[ADR-0120](../docs/adr/0120-snapshot-keyed-by-indexed-commit.md) / PR #672** since this chapter was
first drafted; see the callout right after this quote for what changed and why:

```rust
pub async fn latest_indexed_commit(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let completed: Option<String> = sqlx::query_scalar(
        "SELECT s.commit_sha FROM index_snapshots s \
         WHERE s.repository_id = $1 \
           AND EXISTS (SELECT 1 FROM code_chunks c \
                       WHERE c.repository_id = s.repository_id AND c.commit_sha = s.commit_sha) \
         ORDER BY s.completed_at DESC LIMIT 1",
    )
    .bind(repository_id)
    .fetch_optional(pool)
    .await?;
    if completed.is_some() {
        return Ok(completed);
    }

    // Repositories indexed before snapshots were recorded have no such row, and their newest rows
    // are the whole index.
    sqlx::query_scalar(
        "SELECT commit_sha FROM code_chunks WHERE repository_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(repository_id)
    .fetch_optional(pool)
    .await
}
```

**What ADR-0120 fixed, and why the quote above now has two queries, not one**: before PR #672, this
function only ran the second query — `ORDER BY created_at DESC, id DESC` straight over `code_chunks`
— and that query's `commit_sha` column was itself unreliable. Both halves of the index (`code_chunks`
rows here, `:Symbol` nodes in Neo4j) derived their write-time key as
`context.head_sha.as_deref().unwrap_or(&context.default_branch)`, and an **index** task's own
`create_index_task` never sets `head_sha` — it's `NULL` until the runner actually clones the repo and
reads the real commit. So every default-branch index's rows landed under the literal string
`"main"`, not a real commit SHA: every push merged into that one snapshot (writes are
`ON CONFLICT … DO UPDATE`, never delete-then-insert), ADR-0052's pruning sweep always kept `"main"` as
the latest snapshot so it was never reaped, and the snapshot silently became the union of every
version of the code ever indexed. Measured on this repo's own last 30 pushes replayed into one such
snapshot: 1,107 of 6,032 stored `:Symbol` nodes (18%) were stale — present in the index, gone from the
code at HEAD.

The fix is in two parts, both load-bearing: the runner now reads the commit it actually indexed from
the checkout itself (`clone::head_commit`) instead of trusting the task's `head_sha`/`default_branch`
fallback, and a new `index_snapshots` table (migration `0042`) records that real commit in the *same
transaction* as the terminal "succeeded" status — so a snapshot only gets a completion marker if the
whole index run committed cleanly, and a still-running index (no row yet) can never be mistaken for
the latest one. `latest_indexed_commit`'s first query reads that table; the `EXISTS` guards against a
completion marker surviving something that wiped the underlying `code_chunks` rows (a repo purge, a
pgvector dimension-change `TRUNCATE`, a pruning sweep that outran the marker). The second query is
pure backward compatibility — a repo indexed before migration `0042` shipped has `code_chunks` rows
but no `index_snapshots` row yet, so the old commit-column read is the fallback, not the primary path.

The doc comment states the governing principle, unchanged by the rewrite: **both the "is this repo
indexed at all" skip-check and the retrieval scope reference this exact same function**, so "the skip
decision and the search scope always reference a commit that *provably has chunks*." There is
structurally no way for them to disagree, because they're not two separate decisions keyed on
possibly-different commits anymore — they're the same `latest_indexed_commit(repository_id)` call,
used twice.

**Why pinning to the latest snapshot — not the PR head — is semantically correct, not just a
performance hack**: ADR-0050 explains this is only valid because a review's retrieval need is for
*base/repo context* (callers, related code, prior art), while "the PR's own changes are already in
the prompt as the diff" (ADR-0025, "reviews reuse the base index"). Retrieval exists to answer "what
does the rest of the repo look like," and the rest of the repo, as of the latest default-branch
index, is exactly that — the PR's own new lines don't need to be *retrieved*, they're handed to the
reviewer directly as the diff text. So reusing a slightly-behind default-branch snapshot instead of
re-indexing the PR head loses nothing a reviewer actually needed.

## 4.6 Pruning old snapshots (ADR-0052, the pgvector half)

Because every default-branch push writes a brand new `(repository_id, commit_sha)` snapshot and
nothing automatically reaps the old ones, `code_chunks` would otherwise grow without bound — "the
index sweeper keeps only the in-use snapshots per repo and prunes the rest"
(`code_chunks.rs:101-102`, moved down from the pre-ADR-0120 line numbers by the new
`index_snapshots`-table code above it). Three functions work together:

- **`repos_with_stale_snapshots`** (`code_chunks.rs:71-78`) — `GROUP BY repository_id HAVING
  count(DISTINCT commit_sha) > 1`, so the sweeper only visits repos that actually have more than one
  snapshot; a steady-state repo costs one grouped count, not a per-repo delete attempt.
- **`in_use_commits`** / **`has_active_index_task`** (`code_chunks.rs:85-111`) — together compute the
  "don't delete this" keep-set: every commit a currently-non-terminal task is pinned to
  (`in_use_commits`), plus a whole-repo skip if an `index` task is still writing a *new* snapshot
  right now (`has_active_index_task` — an in-flight index task carries a `NULL head_sha`, so
  `in_use_commits` structurally can't protect the commit it's mid-writing; the sweeper defers the
  entire repo by one cycle instead).
- **`prune_code_chunks`** (`code_chunks.rs:117-136`) — the actual delete, scoped by `commit_sha <>
  ALL($2)` against the keep-list, with one more safety margin: `AND created_at < now() - interval
  '10 minutes'`. That 10-minute grace window exists because a just-finished index's task row might not
  have flipped to a terminal status yet — belt-and-suspenders on top of `in_use_commits` rather than
  relying on it alone. Like file `03`'s `prune_graph`, an empty `keep` list is a no-op, never "delete
  everything."

## 4.7 What to read next

- `05-glossary.md` — snapshot, exact scan vs. ANN, cosine distance, and every other term this chapter
  used.
- `01-control-plane-roles.md` — the trust-boundary reasoning (ADR-0002) that explains why none of
  this chapter's queries are ever reachable with a repository/commit scope the caller chose, rather
  than one the server resolved.
