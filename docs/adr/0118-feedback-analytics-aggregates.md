# ADR-0118: Reviewer feedback is a product surface fed by control-plane aggregates, not Grafana iframes

- **Status:** Proposed
- **Date:** 2026-09-20
- **Deciders:** @leghadjeu-christian

## Context and Problem Statement

Every 👍/👎 a human leaves on a comment the bot posted is polled off the forge and reconciled into
`review_feedback` ([ADR-0035](0035-review-feedback-signal.md)), and is already fed back into the
reviewer's prompt as "previously rejected here" ([ADR-0044](0044-feedback-memory-m1.md)). None of it
is visible anywhere a user of the product can reach — only in a Grafana behind a separate OAuth2
proxy ([ADR-0046](0046-observability-dashboard-deployment.md)), at estate scope.

The console that [ADR-0115](0115-retire-apps-web-move-console-to-lci-ui.md) moved to `apps/lci` in
`ADORSYS-GIS/converse-frontends` could not show it:

- **Nothing in the app can express a time window.** Its Overview page calls `GET /tasks` with no
  parameters, which this control plane caps at 100 rows
  (`services/control-plane/src/queue/tasks.rs:16`), then aggregates those rows in JavaScript. On any
  estate busy enough to matter, "the last 14 days" is the last day and a half and "Total runs" reads
  `100` permanently. There is no range picker because there is nothing a range picker could do: the
  endpoint underneath has no window parameter and no aggregate.
- **Per-repository analytics was two Grafana `d-solo` iframes** — "Billed cost" and "Tokens used".
  Those are the only two panels in the generated board set that are genuinely repository-scoped;
  `review-quality.py`'s own comment records that its Postgres findings and reactions panels are
  **not** filtered by `$repo` and were left out of the embed "rather than guessed at".

This control plane offers no aggregate endpoint at all: `/tasks`, `/tasks/{id}`,
`/tasks/{id}/review`, `/tasks/{id}/feedback` and `/repositories` are all row readers, and the
feedback one is per task. **"How were the comments we posted for this repository last month
received" is currently answerable only by fetching every task and every task's feedback.**

Where should that question be answered — in Grafana, in the browser, or in this control plane's own
read API?

## Decision Drivers

- The answer must be windowed and correct at any estate size — not a chart drawn over whatever 100
  rows came back.
- The 👍/👎 signal should be visible to the people who produce it, in the product, not only to an
  operator with Grafana access.
- Fast, with a number behind the word: the query must be index-servable and stay that way.
- Honest: no figure may imply a precision the underlying sample does not have (reaction coverage is
  unmeasured and self-selecting; reconcile time is not reaction time).
- Billed cost and tokens live in the AI-Gateway's Loki billing stream and are **not in this
  database at all** since [ADR-0100](0100-retire-db-transcript-logs-as-observability.md) retired the
  DB run transcript. Whatever is built must not appear able to answer money.

## Considered Options

- **A — An aggregate read endpoint on the control plane**, consumed by a first-class page in
  `apps/lci`.
- **B — Keep embedding Grafana**, and add the missing panels there.
- **C — Aggregate in the Next.js layer** over a bigger page of `GET /tasks`.
- **D — A daily rollup table** from the start.
- **E — A single composite, operator-tunable "review quality" score** as the headline figure.

## Decision Outcome

Chosen option: **"A — an aggregate read endpoint on the control plane"**, because the data is
already here, the window is a SQL predicate rather than a client-side filter, and it is the only
option that makes the review pipeline's own signal reachable from the product without a second
origin, a second auth hop and a theme we do not control. The decisions below are what "option A"
means concretely; they are labelled `D1`–`D8` and cited by that label from the code in both
repositories.

**Scope, deliberately narrow (owner directive, 2026-09-20).** The first shipment reports **reactions
only**. Everything that reports what a review *found* — findings by priority and category, run
outcomes, durations, and the `review_findings` projection that fed them — is **deferred** to
[#667](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/667), which records the
removed schema and code in full. D5 and D6 below are written to that reduced scope; #667 is the
record of what a later, wider scope would restore.

### D1 — Reviewer feedback is a first-class product surface in `apps/lci`

Two scopes, estate-wide (`/feedback`) and per-repository (a Feedback tab beside Overview / Graph /
Settings). The estate/per-entity split is the shape that console already uses for the same question,
and the per-repo view is the drill-down target of the estate view's repository table. The repository
Overview keeps its facts (branch, platform, run count, approval provenance) and loses the iframes.

### D2 — The data contract is an aggregated read API on the control plane; the app never aggregates a row listing

One bearer-protected endpoint, gated on `task:read` — the permission the underlying rows already
need — repo-scoped or estate-scoped:

```
GET /api/v2/analytics/feedback?repository_id=&from=&to=&bucket=
```

`repository_id` omitted means estate. `from`/`to` are RFC3339 with `to` exclusive. `bucket` takes a
constrained interval grammar (`<n> minute|hour|day`) and every parameter is **rejected loudly**
rather than silently defaulted: a window parameter that quietly becomes something else answers a
different question than the chart drawn from it claims. Windows are capped at 400 days and 1,000
buckets.

Two properties are decided here:

- **The comparison window is computed server-side, in the same response.** One `FILTER (WHERE …)`
  pair over a doubled window costs less than a second round trip, so every headline number arrives
  with its own equal-length previous window.
- **The whole page is one request** — not one per panel. At this cardinality the board family *is*
  the dedupe unit.

Corollary, binding on the consumer: **no screen computes a KPI by aggregating a paged row
listing.** The Overview page described above is the counter-example this rule exists to delete.

### D3 — The page is declarative, but as a typed module in `apps/lci` — not as a second `dashboards.yaml`

Panels are declared as data and rendered through the panel kit `packages/ui-web` already ships. The
console's `dashboards.yaml` **engine** is deliberately not reused: its schema and resolver are built
around the usage backend's vocabulary and a client-side query layer `apps/lci` does not have. The
condition for revisiting is stated so it can be met — extract that engine behind a data-source seam
when either a third LCI dashboard page appears, or an operator asks to change LCI panels without a
rebuild. This decision binds the consumer repository, not this one; it is recorded here so the whole
design reads in one place.

### D4 — Range is the page's primary control, comparison is implicit, bucket is derived

- **Range**: this week · last week · this month · last month · last 7 / 30 / 90 days.
- **Comparison**: always on, never a knob. Every headline number carries a delta against the
  immediately preceding window of equal length, and names the dates it compared against.
- **Bucket**: derived from the range (≤ 7 d → 1 hour, ≤ 90 d → 1 day, else 7 days), never chosen
  separately. A second knob that can contradict the first is a way to draw a wrong chart, not a
  feature.

### D5 — Feedback is a small set of honest indicators. There is no composite score

Reported: inline comments posted in the window; how many of them drew a 👍 or 👎; the counts of each;
the people who reacted; the acceptance rate `👍 / (👍 + 👎)`; the mix of every reaction kind; and,
estate-wide, the same figures per repository.

**Not** reported: a single weighted "review quality" number. It would have to invent weights across
incommensurable things, and it would rest on a reaction sample whose coverage nobody has measured
and whose bias is obvious — people react to what annoys them. If a score is later wanted, its
weights must be operator configuration and must be printed beside the number, never compiled in.

Four honesty rules travel with the numbers and are enforced in code, not left to review:

1. **The acceptance rate divides by `👍 + 👎`, never by `count(*)`.** `review_feedback` stores every
   reaction the forge allows (`heart`, `rocket`, `eyes`, …); they are shown beside the rate, never
   inside it.
2. **Reaction timestamps are reconcile times, not reaction times.** The forge emits no webhook for
   reactions; a singleton reconciler polls (default every 300 s, ADR-0035/ADR-0058). So a reaction
   is counted on **the comment it was left on, by when that comment was posted** — a figure that
   does not move when the poller happens to run.
3. **Feedback older than the poll window is frozen.** `list_pollable_comments` only considers
   comments whose task is younger than `RECONCILER_WINDOW_DAYS` (default 14). A range longer than
   the poll window says that its left edge is stale rather than drawing a curve that implies
   continuous reconciliation.
4. **Coverage is shown with the rate.** "42 of 900 comments drew a reaction", not a bare percentage
   that implies everyone answered.

### D6 — The jsonb findings column stays the audit record, and nothing projects it yet

`reviews.findings` remains the verbatim record of what the agent said, and **no projection of it is
built**. Nothing on this surface reads what a finding said, so a reaction needs no route back to the
finding it stands on, and the schema stays as it was.

What that leaves untouched, deliberately and worth stating because it is a real defect: this repo's
own ADR-0044 feedback memory (`rejected_findings_for_repo`) recovers the finding behind a 👎 through
a best-effort jsonb join, and the Grafana generator does the same join with the opposite cast —
`feedback.rs` casts the jsonb line to `int`:

```sql
JOIN LATERAL jsonb_array_elements(r.findings) finding
  ON finding->>'file' = rc.file AND (finding->>'line')::int = rc.line
```

while `tools/dashboard-gen/lci_dashboards/feedback.py:27` casts the column to `text`:

```sql
WHERE rc.kind = 'inline' AND f->>'file' = rc.file AND f->>'line' = rc.line::text
```

`rejected_findings_for_repo`'s own doc comment concedes the match is best-effort and that "a
path-normalization mismatch just misses a row", so every "👎 by category" figure in existence is
under-counted by an unknown amount. A typed `review_findings` projection would make that match
indexed and stop the two call sites disagreeing — it would **not** fix path normalization, which
needs the finding's identity recorded on the comment at post time. Both are deferred to
[#667](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/667); this decision does
not make the existing defect worse, and does not fix it either.

Migration `0040` therefore carries exactly one index: `review_comments (created_at)`, the column the
cohort ranges over. A daily rollup table is **explicitly deferred** for the same reason it always
was — it buys speed at the cost of a staleness surface and a backfill obligation, and the trigger to
build one is a measured p95, stated in D7, not a guess.

### D7 — "Fast" is a number with a test behind it

**p95 under 200 ms for a 30-day single-repository window, and under 500 ms for a 90-day estate
window**, measured on the control plane. The plan for the windowed statement is pinned by an
`EXPLAIN` assertion in the test suite (sequential scans priced out), so an index that stops being
used fails CI rather than quietly degrading the page.

### D8 — Grafana keeps operations; the app takes the product surface

After D1 lands, the two per-repo iframes are deleted. The run-logs embed on a run's page **stays** —
it is a Loki log viewer ([ADR-0102](0102-grafana-loki-embedded-run-logs.md)), not an analytics
panel, and rebuilding it here buys nothing.

The generated boards in `deploy/observability/` remain the operator's tool: RED metrics, ingress and
queue health, and — the part that cannot move — **billed cost and token usage, which live in the
AI-Gateway's Loki billing stream and are not in this database at all** since ADR-0100. A
control-plane-fed page can report volume, outcome and feedback. It cannot report money, and it must
not appear to.

### Consequences

- Good, because a window question gets a windowed answer at any estate size, instead of a chart
  drawn over whatever 100 rows happened to come back.
- Good, because the 👍/👎 signal becomes visible to the people who produce it — the same data the
  agent already consumes privately under ADR-0044.
- Good, because one fewer origin, one fewer auth hop, one fewer theme surface, and the figures stop
  disappearing when `NEXT_PUBLIC_GRAFANA_URL` is unset, which is the default.
- Good, because the schema change is one index. No trigger on the audit table, no backfill inside a
  migration transaction, and nothing new to keep in sync with `Finding::priority`.
- Bad, because the page cannot say *what* was rejected — only how much. "The bot is noisy" gets a
  number, not the list of findings behind it, until #667 is done.
- Bad, because numbers will not match Grafana's feedback board, which buckets by reconcile time.
  The difference is deliberate (D5, rule 2) but reviewers comparing the two will see different
  totals.
- Bad, because the app loses its only per-repository cost view. If money must stay visible in the
  product, the "Billed cost" embed has to come back as an explicitly labelled Grafana panel in its
  own zone, rather than the page implying the control plane could answer it.
- Neutral, because `serve` and the reconciler each read `RECONCILER_WINDOW_DAYS` from their own env;
  deploy both roles with the same value or the reported poll window is wrong.
- Neutral, because D5 rests on a number nobody has measured yet — reaction coverage. That is what
  the spike measures, and until it does, the acceptance rate is shown with its denominator.

## Pros and Cons of the Options

### A — An aggregate read endpoint on the control plane

- Good, because the window becomes a SQL predicate over an indexed column, correct at any row count.
- Good, because it is the only option that makes reactions reachable from the product.
- Good, because the previous-window comparison is free in the same statement.
- Bad, because it is new API surface the consumer repository is blocked on.

### B — Keep embedding Grafana, and add the missing panels there

- Good, because it is the cheapest option and it is where the SQL already is.
- Bad, because only two existing panels are repository-scoped, and making the rest so means adding a
  `$repo` variable to boards whose own comments record that this was considered and declined.
- Bad, because it leaves the product's core signal behind a second origin, a second auth hop and a
  theme the app does not control.
- Bad, because it does nothing about the Overview page, which has no Grafana in it at all.

### C — Aggregate in the Next.js layer over a bigger page of `GET /tasks`

- Good, because it needs no backend change, so it ships first.
- Bad, because it is the current bug with a larger constant: raising the 100-row cap moves the
  failure rather than fixing it.
- Bad, because per-task feedback would need one request per run.

### D — A daily rollup table from the start

- Good, because it gives the fastest possible reads.
- Bad, because it adds a staleness surface, a backfill obligation and a second source of truth for
  numbers the indexed query can already serve inside the D7 budget. Premature.

### E — A single composite, operator-tunable "review quality" score

- Good, because a single headline number is what people ask for.
- Bad, because the weights would be invented across incommensurable units.
- Bad, because the reaction sample feeding them is unmeasured and self-selecting; a number that
  looks authoritative and is not is worse than four honest ones.

## Amendments — as implemented

This record was relocated here from `converse-frontends`
`docs/adr/0018-lci-review-analytics-page.md` (where it was numbered 0018, colliding with this repo's
own [ADR-0018](0018-openai-compatible-embeddings.md)) so that the decision lives with the control
plane that owns the data. These record where the shipped code differs from the decisions as first
written; each is a deliberate choice, not drift.

- **A1 (D1) — feedback is its own route**, not a replacement for the app's Overview page, by the
  owner's direction. The Overview keeps its existing behaviour, including its client-side
  aggregation of the latest 100 tasks. D2's corollary therefore has one outstanding
  counter-example, named rather than fixed.
- **A2 (D7) — the responses are not cached** (`cache: 'no-store'`, not `revalidate: 60`). Next's
  data cache does not key on the `authorization` header, so a cached body could reach a caller this
  control plane would refuse. The 60-second cache D7 assumed is not worth that.
- **A3 (D4) — no Custom range and no repository picker** on the estate view. The existing date-range
  field cannot express weeks or months; the repository table's rows are the drill-down instead.
- **A4 (D5, rule 2) — feedback is counted on the comment it was left on, by when that comment was
  posted**, not by `review_feedback.created_at`. The poll-window limit of rule 3 is surfaced as a
  page-level note rather than a cap, so the picked window never silently shrinks.
- **A5 (D5, D6) — the scope reduction of 2026-09-20.** The first version of this record, and the
  first implementation, also covered review analytics: a second endpoint (`/analytics/reviews`), the
  `review_findings` projection with its trigger and backfill, findings by priority and category, run
  outcomes and durations, and the finding-linked feedback panels (👎 by category, most down-voted
  findings). All of it was written, reviewed and tested before being cut back to reactions only.
  [#667](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/667) carries the
  removed SQL and code verbatim, and the acceptance criteria for restoring it.

## References

- [ADR-0032](0032-review-finding-priority-and-category.md) — finding priority and category, which a
  restored projection would resolve at write time (#667)
- [ADR-0035](0035-review-feedback-signal.md) — the polled 👍/👎 signal and why there is no webhook
- [ADR-0044](0044-feedback-memory-m1.md) — the feedback memory whose best-effort join D6 describes
- [ADR-0046](0046-observability-dashboard-deployment.md) — why the generated boards read Postgres
- [ADR-0100](0100-retire-db-transcript-logs-as-observability.md) — tokens and cost are Loki-only
- [ADR-0102](0102-grafana-loki-embedded-run-logs.md) — the run-logs embed D8 keeps
- [ADR-0115](0115-retire-apps-web-move-console-to-lci-ui.md) — the console this page lives in
- `converse-frontends` ADR 0008 / 0010 / 0011 / 0013 (visual direction, primitive stack, URL-first
  state, information architecture), ADR 0014 (`apps/lci`'s scaffolding), ADR 0015 (the panel-type
  vocabulary D3 reuses and the engine it does not)
- The spike that measures what D5 rests on:
  <https://github.com/ADORSYS-GIS/converse-frontends/issues/516>
- The deferred review analytics:
  <https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/667>
- The consumer: <https://github.com/ADORSYS-GIS/converse-frontends/pull/517>
