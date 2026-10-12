# 5. Glossary

Every term here is used in files `01`–`04`; each entry names where it's defined or used so you can
go straight to the source.

**A2A task / `a2a_tasks`** — the mapping row between a server-generated A2A `taskId` and the
underlying `tasks` row that does the real work (`0025_a2a_tasks.sql`). Carries its own optimistic
concurrency `version` column, a deliberately separate layer from `run_epoch` — see **version
(A2A)**. File `02` §2.6.

**Advisory lock (`pg_advisory_xact_lock`)** — a Postgres session/transaction-scoped lock keyed on an
arbitrary integer, used by `reserve_mcp_run_slot`
(`services/control-plane/src/db/tasks.rs:827-872`) to serialize the count-then-insert quota check per
caller identity, auto-released on commit or rollback. File `02` §2.4 (in passing).

**Claim (verb)** — a dispatcher (or reconciler) atomically taking ownership of a queue row via `SELECT
... FOR UPDATE SKIP LOCKED` plus an `UPDATE` in the same statement. See **lease**, **`FOR UPDATE SKIP
LOCKED`**. File `02` §2.2 and §2.3.

**`CheckpointRuntime`** — the opt-in agent-loop runtime that journals each step's result to
`durable_step` so a crashed run can replay already-completed steps instead of re-paying them
(ADR-0087). Off by default in production; contrasted with `Passthrough`. File `02` §2.6, file `01`
(the `replay` role).

**Cosine distance / `<=>`** — pgvector's operator computing `1 - cosine_similarity` between two
vectors; `0` means identical direction, up to `2` means opposite. `search_code_chunks` converts it
back to a `[0,1]`-ish similarity via `1.0 - (embedding <=> $1::vector)`. File `04` §4.3.

**Dedup key (`dedup_key`)** — the stable string that makes an `outbox` row's enqueue idempotent via
`ON CONFLICT (dedup_key) DO NOTHING` — e.g. `<task_id>:review`, `<task_id>:reaction:eyes`. File `02`
§2.3.

**Dispatcher** — the role that claims `queued` tasks and launches one Kubernetes Job per task
(ADR-0004). Any number of replicas can run safely because claiming uses `FOR UPDATE SKIP LOCKED`.
File `01` §1.4.

**Durable step (`durable_step`)** — the crash-recovery journal table for `CheckpointRuntime`, keyed
`(task_id, run_epoch, step_name)` (ADR-0087). Execution state, not domain data; self-purges on
success, TTL-swept by the `replay` role otherwise. File `02` §2.6.

**Entry point (`entry_point`)** — which trigger created a task: `pr_open`, `mention`, or `a2a`
(added by `0033_task_preset.sql`). Kept separate from **preset** because preset names are
operator-defined and can't reliably signal intent. File `02` §2.2.

**Exact scan** — a full, non-approximate comparison of a query vector against every row in scope,
as opposed to an approximate-nearest-neighbor (ANN) index lookup. `search_code_chunks` runs an exact
scan because its 4096-dim column exceeds pgvector's HNSW dimension limit — made affordable by
scoping every query to one `(repository_id, commit_sha)` snapshot first. File `04` §4.2–4.3.

**`FOR UPDATE SKIP LOCKED`** — the Postgres row-locking clause that lets several concurrent
transactions each grab a *different* candidate row from the same queue table without blocking on each
other: a row another transaction already has locked is simply skipped rather than waited for. Used
by both the task-queue claim (`claim_next_task`) and the outbox claim (`claim_outbox_batch`) — with
different concurrency guarantees in each case (see the outbox's single-replica caveat). File `02`
§2.2 and §2.3.

**Generic label/relationship type (`:Symbol` / `:REL`)** — the Neo4j modeling choice of one node
label and one relationship type for every structural-graph entity, with the specific kind carried as
a `label`/`relation` *property* instead — necessary because Cypher can't parameterize labels or
relationship types. File `03` §3.3.

**Hollow index** — an index that reports "this repo is indexed" while the commit retrieval is pinned
to has zero actual chunks, because the indexed commit and the retrieval-scope commit silently
disagreed. The bug ADR-0050 fixes by making both reference the same `latest_indexed_commit` call.
File `04` §4.5.

**Hybrid search** — `hybrid_symbol_search` in `neo4j.rs`: fulltext (lexical) and vector (semantic)
search over the same `:Symbol` label, fused by weighted reciprocal rank. File `03` §3.7.

**Idempotency index (`tasks_idempotency_idx`)** — the unique index on `(repository_id, target_type,
target_id, command_text, head_sha, run_epoch)` with `NULLS NOT DISTINCT` that collapses duplicate
webhook-triggered tasks into one row. File `02` §2.2.

**Lease (`lease_owner` / `lease_expires_at`)** — the short-lived claim a dispatcher takes on a task
row when it transitions it to `running`, identifying which replica owns it and until when. An
expired lease is a signal for the reaper to *reconcile* (check the Job's real liveness), not proof
the task is dead. File `02` §2.2.

**Mediated write / mediated tool** — the ADR-0037 pattern where the agent runner never writes
directly to GitHub or Postgres; it calls a tool (`add_review_comment`, `set_summary`, ...) that the
control plane buffers (`pending_review_actions`) and later flushes itself. The root pattern ADR-0002
names as the system's trust boundary. File `02` §2.6, file `01` §1.1.

**Outbox (`outbox`, née `github_outbox`)** — the transactional-outbox table of forge-egress intent
rows; the reconciler is the sole consumer that posts them (ADR-0059). File `02` §2.3.

**Platform abstraction (ADR-0072)** — the migration (`0024_platform_abstraction.sql`) that
generalized GitHub-specific tables/columns (`github_deliveries`, `github_outbox`,
`github_repo_id`, ...) to platform-neutral names (`webhook_deliveries`, `outbox`,
`platform_repo_id`, ...) with a `platform` column defaulting to `'github'`, so GitHub, GitLab, and
Bitbucket share one schema. File `02` §2.1.

**Preset (`preset`)** — the resolved review configuration name for a task (`fast`/`deep` by platform
default, or any operator-defined name), resolved per repo config and entry point (ADR-0103). Renamed
from the hardcoded `tier` column by `0033_task_preset.sql`. File `02` §2.2.

**Reciprocal rank fusion (RRF) / weighted RRF** — a method for combining several ranked result lists
into one by summing `1 / (constant + rank)` per item across lists, using each branch's *rank* rather
than its raw (incomparable) score. `hybrid_symbol_search` uses `RRF_CONSTANT = 60.0`, Neo4j's
documented default. File `03` §3.7.

**Reconciler (née poller, ADR-0058)** — the single-replica role that drains the outbox (sole GitHub/
GitLab/Bitbucket egress, ADR-0059) and polls reactions back into `review_feedback` (ADR-0035). File
`01` §1.4, file `02` §2.3.

**Run epoch (`run_epoch`)** — an integer discriminator on `tasks` that lets an explicit re-run of the
same logical work (same repo/target/command/head_sha) create a new row instead of colliding with the
idempotency index; incremented via `COALESCE(MAX(run_epoch), -1) + 1` in `create_explicit_task`. File
`02` §2.2.

**Snapshot (`(repository_id, commit_sha)` / `(repo_id, commit)`)** — the unit every structural and
semantic query is scoped to: one repository's state as of one specific commit. Two snapshots of the
same repo (different commits) are entirely separate sets of rows/nodes; re-indexing a new commit
never mutates an old snapshot in place. File `03` §3.3, file `04` throughout.

**Snapshot write lock (`:SnapshotWriteLock`)** — a transient Neo4j node, backed by a uniqueness
constraint, that `upsert_graph` merges-then-deletes at the start of every write to serialize
concurrent writers to the same `(repo_id, commit)` snapshot — otherwise a bare `MERGE` on `:Symbol`
can race and duplicate a node. File `03` §3.3.1.

**Trust boundary (ADR-0002)** — the architectural decision that the Rust control plane, not the
agent runner, owns every GitHub write and every durable state mutation; the agent may read and
propose but never acts directly. The organizing idea behind file `01`'s role split, the outbox
(file `02`), and why the control plane (not the agent Job) holds the Neo4j/Postgres credentials
(files `02`–`04`).

**Version (A2A, `a2a_tasks.version`)** — an optimistic-concurrency counter bumped only via a
`WHERE version = <expected>` CAS update, closing a lost-update race the A2A SDK's own `TaskStore`
API doesn't protect against on its own (a blind upsert). Deliberately a different mechanism from
**run epoch**, which is review-run idempotency, not update concurrency. File `02` §2.6.

**Waiting for index (`waiting_for_index`, ADR-0055)** — the `TaskStatus` a review/ask task starts in
when an `index` task is already in flight for its repository, so the dispatcher never claims it
against a half-written snapshot. Released back to `queued` when the blocking index task reaches any
terminal status. File `02` §2.5.
