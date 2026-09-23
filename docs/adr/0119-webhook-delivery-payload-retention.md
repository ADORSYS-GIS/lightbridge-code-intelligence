# ADR-0119: A webhook delivery row is permanent; its payload is not

- **Status:** Proposed
- **Date:** 2026-09-23
- **Deciders:** @leghadjeu-christian

## Context and Problem Statement

`webhook_deliveries` is the only append-only table in this control plane with no retention. Every
verified delivery is inserted with its full JSON body and nothing ever removes or shrinks one. On
**2026-08-29 ~10:05 UTC** it exhausted the volume of `lightbridge-main-db`, the CNPG cluster this
service shares with every other lightbridge tenant. PostgreSQL died with `PANIC: could not write to
file`, both instances went down, and every tenant went with them — including the 10 MB `app`
database that owns authentication for the whole platform
([#637](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/637), runbook in
[#638](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/638)).

Measured on the live database after recovery:

```
codeintel                7413 MB   78% of all data on the shared cluster
  webhook_deliveries     5061 MB   >50% of the whole volume
    payload_json         4255 MB   84% of the table, avg 4711 B/row
usage                    1507 MB
app (lightbridge-authz)    10 MB
```

Service was restored by growing the volume twice (`ai-helm#1059` 5→20 GiB, `ai-helm#1060` 20→40
GiB). That moves the date the curve meets the ceiling; it does not change the curve. Re-measured on
**2026-09-17** from the production control plane's own `/metrics` and its log of accepted
deliveries: **~40,100 rows/day**, against 28,111/day in the week before the outage and 13,338/day
averaged over the 71 days before that. It is still accelerating.

The obvious remedy — delete old rows — is unavailable, because the row is load-bearing in three
different ways and only one of them involves the payload:

| What the row does | Needs the row | Needs the payload |
|---|---|---|
| Exactly-once handling. `record_delivery`'s `ON CONFLICT (delivery_id) DO NOTHING` **is** the dedup; there is no other store | yes | no |
| `tasks.webhook_delivery_id` foreign key (3,590 rows referenced at the incident) | yes | no |
| `reserve_mcp_run_slot`'s per-caller quota, which reads `payload_json->>'caller'` | yes | yes, inside its window |

So: **how is this table bounded without weakening redelivery dedup, breaking the foreign key, or
changing a quota decision?**

## Decision Drivers

- Dedup must stay exact. A redelivery that reprocesses posts a second review on someone's PR.
- The foreign key must stay intact; task history is not retention's to delete.
- A quota decision must never change because a row aged.
- Growth must be bounded at any estate size, and by a rule, not by an operator remembering.
- The remedy must not itself threaten the volume it protects: WAL lives on that same disk.
- Prefer the mechanism already running. Three retention sweeps already share one tick.

## Considered Options

- **A — Compact the payload to `{}` past a retention window; keep every row.**
- **B — Delete rows past a retention window.**
- **C — Delete only rows no task references, keeping referenced ones.**
- **D — Time-partition the table and drop whole partitions.**
- **E — Move payloads to object storage and keep a pointer.**
- **F — Keep growing the volume.**

## Decision Outcome

Chosen option: **"A — compact the payload, keep the row"**, because it is the only option that
removes the bytes that have no reader while leaving all three of the row's jobs exactly as they
were. The payload is 84% of the table and is read only while the delivery is being routed; the row
is ~150 bytes and is read for as long as a redelivery or a task can refer to it. The decisions below
are what option A means concretely, labelled `D1`–`D6`.

### D1 — The row is permanent. The payload has a retention window

Compaction sets `payload_json = '{}'::jsonb`. It never deletes a row, never touches `delivery_id`,
`event_name`, `platform` or `received_at`, and therefore cannot be observed by the dedup path or the
foreign key. `{}` rather than `NULL`, because the column is `NOT NULL JSONB` and every reader
already handles an empty object.

`{}` records **that** no payload is retained and nothing about **why**: a delivery stored without
one and a delivery compacted six days later are identical in the table. No platform sends an empty
webhook body, so an empty payload is never evidence of an ingest bug. This is deliberate. A
`compacted_at` marker would be a per-row cost on millions of rows to separate two states that no
reader treats differently.

### D2 — Retention is dispatcher configuration on the GC tick that already runs

`dispatcher.webhook_payload_retention_days`, default **7**, in the file-config shape
`outbox_posted_retention_days` established ([ADR-0059](0059-reconciler-owns-all-github-egress.md)),
swept on the `prune_interval_seconds` tick that the index sweeper
([ADR-0052](0052-index-snapshot-pruning.md)), the outbox sweeper and the A2A sweeper
([ADR-0077](0077-a2a-streaming-event-log.md)) already share. This is a fourth sibling on an existing
tick, not new infrastructure.

Seven days is not a correctness boundary — nothing reads a payload minutes after routing — it is a
debugging convenience, sized to match the outbox's window so the control plane has one answer to
"how long is operational data kept".

A zero or negative value falls back to the default, and the query refuses to run at all below 1 day.
`make_interval(days => 0)` is `now()`: an unguarded zero would compact every row in the table,
including ones still being read. That guard exists twice, in config resolution and in the statement's
own function, so it holds for any caller.

### D3 — One tick compacts a bounded batch, and the bound is about WAL, not lock time

`dispatcher.webhook_payload_sweep_batch`, default **5,000**, oldest first.

The obvious reading is that batching bounds lock duration. The real reason is the volume. A 4.7 KB
payload lives in TOAST storage; compacting a row dirties its TOAST pages, and those writes go to WAL
— on the same disk that filled in #637. Draining the ~2.3M-row backlog in one statement would answer
a disk-full incident with a multi-GB write burst. At 5,000 per 10-minute tick the sweep has ~720,000
rows/day of capacity against ~40,100/day of ingest, so it keeps up with room to spare, and the
backlog drains over roughly three days while checkpoints and archiving recycle WAL as it goes.
Measured at production payload sizes, a batch is ~0.57 s of work.

### D4 — The `mcp.review` quota ledger is never compacted

`mcp.review` rows are not platform deliveries. They are this service's own quota ledger, written by
`reserve_mcp_run_slot`, which counts a caller's recent deep runs by reading `payload_json->>'caller'`
back over `MCP_QUOTA_WINDOW_SECS`. A compacted ledger row inside that window stops matching its
caller and silently stops counting — a quota that loosens as rows age, with no error and nothing in
a log to say so.

The sweep therefore excludes the event name outright, rather than asserting that retention exceeds
the quota window. **The assertion cannot be made honestly:** that window is read in the `mcp` role's
process and the sweep runs in the `dispatcher` — separate Deployments with separate environments, so
an operator raising it on the mcp Deployment alone would leave the dispatcher checking a value it
cannot see. Excluding the event name needs no agreement between roles and holds whatever either knob
is set to. It costs nothing: the ledger payload is four provenance fields, on rows that are
quota-limited by construction.

**Corollary, binding on future work:** any new reader of `payload_json` outside the routing path must
either live inside the retention window by construction, or have its rows excluded here the way the
ledger is. A reader that merely happens to be shorter than the default is not protected by anything.

### D5 — The supporting index is partial, and built without blocking ingest

`webhook_deliveries (received_at) WHERE payload_json <> '{}'::jsonb`, created `CONCURRENTLY` in a
`-- no-transaction` migration.

- **Partial**, so a row leaves the index the moment it is compacted. The index stays sized to the
  retention window instead of growing with the table forever, and it still serves the quota query,
  whose rows are recent and uncompacted.
- **Concurrent**, because a plain `CREATE INDEX` blocks `INSERT` for its whole build, and on this
  table that is the webhook receiver unable to record deliveries.

The statement that uses it materialises its batch as `= ANY(ARRAY(…))` rather than `IN (subquery)`.
This is not style: with `IN`, the planner hash-joined the subquery against a **sequential scan of the
whole table** on every tick. With the array, the outer side is a primary-key lookup. The plan is the
decision; the syntax is how it is held.

### D6 — Reclaiming the space already allocated is an operator action, not a migration

Compaction stops the growth. It does not return the existing multi-GB to the filesystem: PostgreSQL
makes that space reusable inside the table, nothing more. Returning it needs a table rewrite, which
holds an `ACCESS EXCLUSIVE` lock — on the table the webhook receiver writes to, for a platform that
gives a delivery 10 seconds and does not retry a timeout.

That is a scheduled decision with a blast radius, so it is a runbook and a one-shot Job in
`ai-helm-values` ([#449](https://github.com/ADORSYS-GIS/ai-helm-values/pull/449)), not a migration
that fires at whatever moment a deploy happens, and not anything a controller re-applies. The Job
uses `VACUUM FULL`: `pg_repack` would hold the lock only briefly, but it is absent from the cluster
image and the tenant roles are not superusers who could install it — an `ai-helm` image decision, not
this one.

### Consequences

- Good, because the table's growth becomes bounded by a rule that runs unattended, and the largest
  single contributor to a platform-wide outage stops compounding.
- Good, because all three of the row's jobs are provably untouched: a redelivery of a compacted
  delivery is still a duplicate, a task's reference still resolves, and a quota decision cannot move.
- Good, because it reuses a tick, a config shape and a sweeper pattern that already exist; the
  operational surface is two settings and one counter.
- Bad, because a raw webhook body older than the window is gone. Debugging "why did nothing happen
  for this delivery last month" now goes to the forge's own delivery log, not to this database.
- Bad, because `{}` is ambiguous by construction (D1), and becomes the common case rather than the
  exception once payloads are stored only for routed events
  ([#661](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/661)).
- Neutral, because the first days after deployment write materially more WAL than steady state while
  the backlog drains. D3 bounds it; it is still the period to watch.
- Neutral, because nothing here reclaims a byte on the volume until D6's Job is run, which should
  wait until the backlog is drained so the rewrite copies as little live data as possible.
- Neutral, because this does not touch `code_chunks` (2,317 MB) or `usage.usage_events` (a different
  repo, since addressed in `lightbridge-authz#678`), and it does not change the fact that one
  tenant's table can still fill a cluster that all authentication depends on. That is a platform
  decision, recorded as out of scope by #637 and unchanged by this ADR.

## Pros and Cons of the Options

### A — Compact the payload, keep the row

- Good, because it removes ~84% of the table's bytes and none of its guarantees.
- Good, because the sibling mechanism, tick and config shape already exist (ADR-0052/0059/0077).
- Bad, because it leaves ~150 bytes per delivery accumulating forever — bounded growth, not zero.
- Bad, because an `UPDATE` writes a new row version; the reclaim is a separate, locking step (D6).

### B — Delete rows past a retention window

- Good, because it is the smallest statement and removes the row cost too.
- Bad, because the primary key **is** the dedup. A redelivery of a deleted id reprocesses as new,
  which for `pull_request.opened` means a duplicate review posted on the PR.
- Bad, because rows referenced by `tasks` cannot be deleted without a cascade that takes task
  history, reviews, comments and feedback with them.

### C — Delete only rows no task references

- Good, because it keeps the foreign key valid, and only ~0.4% of rows are referenced.
- Bad, because it keeps every one of B's dedup consequences for the other 99.6%.
- Bad, because "delete unless referenced" makes deletion depend on a join that must stay correct
  forever; the invariant is harder to state than "the row is permanent".

### D — Time-partition and drop partitions

- Good, because dropping a partition is instant and returns space to the filesystem at once.
- Bad, because a partitioned table's primary key must contain the partition column, so dedup would
  become `(delivery_id, received_at)` — and a redelivery carries a *new* receipt time, so the
  conflict that blocks reprocessing would no longer fire. The dedup guarantee would have to be
  rebuilt elsewhere before this is even available.
- Bad, because the foreign key from `tasks` would need the same key change.

### E — Move payloads to object storage, keep a pointer

- Good, because the database shrinks to the row and the payload stays available indefinitely.
- Bad, because it buys retention with a new dependency, a second failure mode on the ingest path,
  and a new class of orphan, to keep data whose only reader is a human debugging last month.

### F — Keep growing the volume

- Good, because it needs no code and it is what restored service on the day.
- Bad, because it is a countdown, not a fix; the measured rate has risen 1.4× since the outage.
- Bad, because the next ceiling is met the same way — with every tenant on the cluster down.

## More Information

- [#637](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/637) — the P0, its
  measurements and the outage timeline
- `docs/runbooks/webhook-deliveries-unbounded-growth.md` — the incident runbook (#638)
- [ADR-0052](0052-index-snapshot-pruning.md) — the GC tick this sweep shares
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) — `prune_outbox` and the retention config
  shape copied here
- [ADR-0077](0077-a2a-streaming-event-log.md) — the A2A sweeper, and the bounded-batch precedent
- [ADR-0072](0072-platform-abstraction-layer.md) — why the table is `webhook_deliveries` and carries
  a `platform` column
- [#660](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/660) — this decision, as
  implemented
- [#661](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/661) — the sibling
  change: a payload is stored at ingest only for events a router acts on (~94% of deliveries are
  not). Decided separately; it reduces what this sweep has to compact, and does not change any rule
  above
- [ai-helm-values#449](https://github.com/ADORSYS-GIS/ai-helm-values/pull/449) — D6's reclaim Job
  and runbook
- `ai-helm#1059`, `ai-helm#1060` — the emergency volume growth that bought the time to do this
  properly
