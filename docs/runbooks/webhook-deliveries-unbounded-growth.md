# `webhook_deliveries` grows without bound — P0

**Status:** open, not yet fixed. **Filed:** 2026-08-29, after it took down a shared database.

`webhook_deliveries` has no retention. It is the only append-only table in the control plane
without one, and it is now the largest object in the cluster's shared Postgres by a wide margin.

## What it caused

On **2026-08-29 ~10:05 UTC** the shared `lightbridge-main-db` CNPG cluster (namespace `converse`,
`hetzner-prod`) exhausted its 10 GiB volume. PostgreSQL died with `PANIC: could not write to file`
and **both instances went down**.

The blast radius was not this service. Every tenant on that cluster went with it:

| Service | Effect |
|---|---|
| `authz-idp` | **all logins down** |
| `authz-api`, `authz-opa`, `lightbridge-mcp`, `authz-budget`, `authz-usage` | unready |
| `lightbridge-repo-auth` | unready |

The `app` database (lightbridge-authz, which owns authentication for the whole platform) is **10 MB**.
It was taken offline by this table.

Service was restored by growing the volume (`ai-helm#1059` 5→20 GiB, then `ai-helm#1060` 20→40 GiB).
That is a countdown, not a fix — see *Runway* below.

## The measurements

Taken from the live database on 2026-08-29 after recovery.

```
codeintel                      7413 MB     ← 78% of all data on the shared cluster
  webhook_deliveries           5061 MB     ← 68% of codeintel, >50% of the whole volume
    payload_json (column)      4255 MB     ← 84% of the table; avg 4711 bytes/row
  code_chunks                  2317 MB
usage                          1507 MB
app (lightbridge-authz)          10 MB
```

```
rows                    947,014
span                    2026-06-19 .. 2026-08-29  (71 days)
average rate            13,338 rows/day
LAST 7 DAYS             28,111 rows/day           ← 2.1x the average, and accelerating
size per row            ~5.3 KB (incl. indexes)
growth                  ~150 MB/day and rising
```

## Runway

At the current rate, against 40 GiB (~30.5 GB free after recovery), and counting
`usage.usage_events` (~100 MB/day) alongside it:

```
~250 MB/day combined  ->  ~122 days
```

Roughly four months. That is room to fix this deliberately; it is not room to ignore it.

## Why this is not a simple DELETE

Three constraints, all of which a naive `DELETE FROM webhook_deliveries WHERE received_at < …`
would break:

1. **The PRIMARY KEY on `delivery_id` IS the webhook idempotency guarantee.** `http/webhook.rs`
   dedups redeliveries by inserting and letting the PK conflict — there is no separate dedup store
   (`main.rs`: *"the webhook dedups on the `webhook_deliveries` PRIMARY KEY instead"*). Deleting a
   row makes a later redelivery of that id reprocess as if new.
2. **`tasks.webhook_delivery_id` is a foreign key into this table.** 3,590 of 947,014 rows are
   currently referenced. Any delete must exclude them or it errors.
3. **`payload_json` is read back after ingest** — `db/tasks.rs`'s MCP-review quota check reads
   `payload_json->>'caller'`, but only inside a recent window (`A2A_QUOTA_WINDOW_SECS`, default
   **3600s**).

## The fix that fits all three

**Null the payload, keep the row.**

```sql
UPDATE webhook_deliveries
   SET payload_json = '{}'::jsonb
 WHERE received_at < now() - make_interval(days => $1::int)
   AND payload_json <> '{}'::jsonb;
```

- Reclaims **~84% of the table** (4255 MB of 5061 MB) — the row without its payload is ~350 bytes,
  so all 947k rows would fit in roughly 330 MB.
- **Dedup is untouched.** The `delivery_id` row survives, so the PK conflict still fires.
- **The foreign key is untouched.** No row is removed.
- **Safe against the read-back**, provided retention comfortably exceeds the 1-hour quota window.
  A retention measured in days has three orders of magnitude of headroom.

A follow-up `VACUUM FULL` or `pg_repack` is needed to return the freed space to the filesystem;
a plain `UPDATE` only marks it reusable within the table.

Deleting rows outright remains an option for rows older than any plausible redelivery, but it buys
little beyond the payload nulling and costs the dedup guarantee — so it should be a separate,
later decision, not part of the first fix.

## The mechanism already exists

This does not need new infrastructure. `outbox` already has exactly this, from ADR-0059:

- `db/outbox.rs`'s `prune_outbox` — the retention query, with a positive-days guard and an
  `int8`-bound day count narrowed via `$1::int` so an out-of-range value errors loudly rather than
  wrapping.
- `config.rs`'s `outbox_posted_retention_days` (default 7) and `outbox_failed_retention_days`
  (default 30) — the config shape to copy.
- `prune_interval_seconds` — the GC tick the index sweeper (ADR-0052) and the outbox sweeper share.
  A third sweep belongs on the same tick.

The work is: add `webhook_payload_retention_days`, write the sibling of `prune_outbox`, and hang it
off the tick that already runs.

## Wider question this raises

`codeintel` is 78% of a Postgres cluster shared with every lightbridge service, including all
authentication. One unbounded table here is a platform-wide outage. Whether these tenants should
share a cluster at all — or whether per-database quotas should exist so that they cannot take each
other down — is a decision that belongs outside this repo, but it was this table that forced it.
