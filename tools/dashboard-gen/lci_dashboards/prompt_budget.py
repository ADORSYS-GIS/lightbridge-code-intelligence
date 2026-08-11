"""Prompt Budget dashboard — answers one question: what should ``review.max_diff_chars`` be raised
to? (prod: 120000 on the ``fast`` preset, 300000 on ``deep`` — the binding constraint on both tiers.)

Data source: the ``lightbridge-agents`` namespace pod logs in Loki (the same review/index-run Job
pods ``task_runs.py`` already reads, but aggregated across ALL runs rather than scoped to one task).
The event itself is ``log_prompt_metrics`` in ``services/review-agent/src/prompt.rs`` (PR #598,
issue #597) — read that function for the authoritative field list; this module's comments name the
fields it uses, not the full set. It fires once per review run and carries:

  - ``preset`` — ``"fast"`` / ``"deep"`` / ``"ultra"`` / an operator-defined tier.
  - Diff accounting: ``diff_source_bytes``, ``diff_rendered_bytes``, ``diff_budget_bytes``,
    ``diff_omitted_bytes``, ``diff_files_total``, ``diff_files_rendered``, ``diff_files_omitted``,
    ``diff_files_low_signal``.
  - Four static-context-block triples (input/rendered/budget bytes each): ``priors_*``,
    ``memory_*``, ``instructions_*``, ``repo_config_*``.

``diff_source_bytes`` is the real demand — the raw diff minus low-signal files (lockfiles,
generated code) that are dropped regardless of budget. Its high percentiles are the direct answer
to "raise it to what?" — everything a bigger cap could plausibly let through. ``diff_omitted_bytes``
is what the cap actually threw away on a given run; ``diff_files_omitted > 0`` means the run lost
whole-file coverage, not just trimmed a hunk. ``diff_budget_bytes`` is the *effective* per-run
budget after the ADR-0070 window-proportional share (``min(max_diff_chars, 0.25 * context_window *
4)``) — comparing it against the operator's configured ``max_diff_chars`` shows whether the cap
itself or the window share is what's actually binding.

--- Gotcha #1: pod log lines arrive in two shapes, CRI-wrapped or not ---

Alloy tails these pods with ``loki.source.file`` straight off ``/var/log/pods/...``, so unless the
pipeline applies a ``stage.cri`` the kubelet's envelope survives into Loki and a stored line looks
like:

    2026-08-08T04:01:44.123Z stdout F {"timestamp":"...","level":"INFO","fields":{...},"target":"..."}

A bare ``| json`` fails with ``JSONParserErr`` on that — it can't parse past the leading
``<ts> <stream> <flag>`` prefix. ``| cri`` is an ingest-pipeline stage, not a LogQL parser, so it
is not an option either. The fix used here is ``common.CRI_UNWRAP``: a ``| regexp`` stage whose
CRI prefix is an OPTIONAL group, capturing the remainder into ``content``, then ``| line_format``
to replace the log line with just that capture, THEN ``| json`` — see ``_CRI_JSON`` below. The
prefix is optional because Loki holds both shapes at the same time — an ingest-side change applies
only to new lines, while stored ones keep their shape until they age out of retention — so a single
query spans both and must handle either. ``common.py`` carries the detail.

--- Gotcha #2: event fields are nested under `fields`, not top-level ---

``lci-observability`` builds its subscriber with ``tracing_subscriber::fmt::layer().json()`` and
**no** ``.flatten_event(true)`` (confirmed by reading ``services/observability/src/lib.rs:75`` —
the fmt layer is constructed with `.json()` alone). That means one `tracing::info!(preset, ...)`
call serializes as ``{"fields":{"message":"...","preset":"fast","diff_source_bytes":123,...}}``,
NOT ``{"preset":"fast","diff_source_bytes":123,...}`` at the top level. LogQL's ``| json`` stage
flattens nested objects by joining keys with ``_``, so every field referenced in this module is
prefixed accordingly: the Loki label is ``fields_preset``, ``fields_diff_source_bytes``,
``fields_priors_rendered_bytes``, and so on — never the bare field name.

--- Disambiguating from a similarly-worded event ---

``build_messages`` (same file) also logs a DIFFERENT event, "prompt budgets: window-proportional
caps active (ADR-0070)", only when the window share actually shrinks a block below its ceiling.
That is NOT this dashboard's event. The ``|= "prompt budget usage"`` line filter below matches only
``log_prompt_metrics``'s message ("prompt budget usage: per-block byte accounting...") and is
evaluated on the raw CRI-wrapped text BEFORE any parsing — a cheap substring match that narrows the
huge mixed pod stream (agent tracing + the OpenCode logger plugin's per-turn stderr + indexing-run
noise) down to exactly the lines this dashboard needs, before spending cycles on `pattern`/`json`.

--- Grouping ---

Every aggregation below groups by ``fields_preset``, rather than computing per-pod-stream numbers
and combining them afterwards — this matters specifically for the percentile queries: computing a
percentile per pod (each review run gets its own ephemeral pod, ``lightbridge-agent-<task-id>-*``)
and then averaging or maxing those per-pod percentiles together would NOT equal the true percentile
over the merged raw samples (percentile-of-percentiles is not a percentile).

**How that grouping is spelled depends on which aggregation it is — this is NOT uniform, and getting
it wrong is a LogQL *parse error*, not a silently-wrong or empty result.** Confirmed against a real
Loki instance (grafana/loki 3.1.1 and 3.5.7, see "Verification status" below) and against the
Grafana docs: *"Except for `sum_over_time`, `absent_over_time`, `rate` and `rate_counter`, unwrapped
range aggregations support grouping."*

- ``quantile_over_time`` / ``avg_over_time`` (``_quantile`` / ``_avg`` below) DO support a native
  trailing ``by (fields_preset)`` clause directly on the range aggregation, e.g.
  ``quantile_over_time(0.99, <expr> [5m]) by (fields_preset)``.
- ``sum_over_time`` (``_sum`` below) does NOT, despite being on the same "unwrapped range
  aggregation" list as the two above — it's the docs' explicit exception. ``count_over_time``
  (``_count`` below) is a *plain* log range aggregation (no ``unwrap``), and none of that family
  (``count_over_time``, ``bytes_over_time``, ``rate``, ``bytes_rate``) support grouping at all.
  Both instead use a Prometheus-style vector-aggregation wrapper — ``sum by (fields_preset)
  (sum_over_time(...))`` / ``sum by (fields_preset) (count_over_time(...))`` — which computes the
  same per-preset total (there's exactly one series per distinct label set already, so the outer
  ``sum by`` doesn't combine multiple values, it just regroups) and is legal LogQL.

--- The mechanism behind the diff-less exclusion (Question 1) ---

``fields_diff_files_total != ""`` in ``_ALL_DIFF_RUNS`` below relies on a non-obvious fact about
how ``log_prompt_metrics`` serializes: every diff field is passed as ``diff.map(|d| d.field)`` —
an ``Option<T>``. ``tracing``'s ``Value`` impl for ``Option<T>`` **omits the key entirely** when the
value is ``None`` — it does NOT emit ``"diff_source_bytes":null``. So a diff-less (``ask``) run's
JSON literally has no ``diff_*`` keys at all, e.g. ``{"fields":{"message":"...","preset":"fast"}}``.
``| json`` then never creates a ``fields_diff_files_total`` label on that line. Confirmed against
real Loki: a pipeline label filter (post-``json``, as opposed to a stream-selector matcher) against
a label that was never extracted drops the line under **both** `!=` and a `=~ ".+"` variant — LogQL
does not fall back to "absent label compares equal to empty string" here (that convention is a
Prometheus/LogQL *stream selector* behaviour, not a post-parse pipeline label filter). Do not "fix"
this filter to `=~ ".+"` later thinking it closes a gap — both forms behave identically for this
purpose, and the risk is someone assuming the current `!=` form is broken when it isn't.

--- ``avg_over_time`` silently divides by the wrong count ---

A second, more dangerous bug (dangerous because it's silent, not a parse error) was found the same
way as the grouping issue: pushing TWO diff-bearing "fast" runs with different `diff_budget_bytes`
(60 000 and 20 000 — true average 40 000) into the same preset group alongside a diff-less "fast" run
(no `diff_budget_bytes` field at all). The dashboard's original unwrap pattern —
``| unwrap fields_diff_budget_bytes | __error__=""`` — returned **26 666.67** (== 80 000 / 3), not
40 000 (== 80 000 / 2). Confirmed against real Loki: ``avg_over_time`` divides its sum by the count
of ALL log lines it attempted to `unwrap` over the range — including ones the *post*-unwrap
``__error__=""`` filter later drops from the sum for having no value to unwrap — not the count of
lines that actually contributed a value. Any preset group mixing diff-bearing and diff-less runs (or
runs with/without a given static-context block), which is the everyday case in production, silently
understated every ``avg_over_time`` panel by exactly that ratio. ``sum_over_time`` and
``quantile_over_time`` were confirmed NOT to share this bug on the same mixed data (a sum doesn't
divide by a count at all; a percentile reads off the sorted list of actual values, unaffected by how
many other lines existed). Fix, applied in ``_unwrap`` below: filter for the field's presence
(``| {field} != ""``) BEFORE ``unwrap``, not only after — this makes the line never reach `unwrap` in
the first place rather than reach it and get discounted post hoc, and the denominator becomes
correct. Re-verified after the fix: the same mixed data now returns exactly 40 000.

--- Verification status ---

✅ Empirically verified, in two rounds, against a real, local Loki (grafana/loki 3.1.1 and 3.5.7 —
every cross-checked behaviour was confirmed identical on both) using genuine
``tracing_subscriber::fmt::layer().json()`` output captured from a real call to
``lci_review_agent::prompt::build_messages`` (not hand-written JSON). Round 1 found the grouping
parse errors above (4 panels) and a bot-claimed absent-label issue that turned out to be a false
positive; round 2, after fixing the grouping, found and fixed the ``avg_over_time`` denominator bug
above (5 panels: the effective-budget-avg panel and all four static-context-block avg panels) using a
second, deliberately multi-sample dataset designed to make averaging bugs visible (a single sample
per group, as round 1 used, cannot distinguish a correct average from a broken one).

- The CRI-unwrap chain (``common.CRI_UNWRAP`` + ``| json``) correctly strips the ``<ts> stdout F ``
  prefix and parses the remainder even though the embedded JSON itself contains spaces (the
  ``content`` capture takes everything after the third field, not up to the next space). Verified
  in this round against a ``| pattern`` form; the stage is now the equivalent optional-group
  ``| regexp``, re-checked against the production stream for the same result.
- Nested ``fields`` flattens to ``fields_<name>`` labels as documented in "Gotcha #2".
- ``|= "prompt budget usage"`` cleanly disambiguates from the sibling
  ``"prompt budgets: window-proportional caps active (ADR-0070)"`` event — zero lines matched both.
- The diff-less exclusion (``!= ""`` on an absent label) behaves as documented above.
- Every panel's query, taken from the generated dashboard JSON (not retyped from this source) after
  BOTH fixes, parses without error and returns the numbers expected from known synthetic input on
  real Loki — including the average panels, re-checked against a multi-sample dataset specifically
  chosen so a wrong denominator would produce a visibly wrong number, not a coincidentally-right one.

⚠️ Residual, honestly-unclosed gap: this was verified against **synthetic data pushed directly to a
local Loki**, not against the real production stream. The stream *labels* used
(``namespace="lightbridge-agents"``, ``pod=~"lightbridge-agent-.*"``) match what ``task_runs.py``
already established works against prod, but that specific claim was not re-verified here, and
real-world data volume/cardinality (many concurrent pods, long time ranges, Alloy's actual JSON
serialization of any fields not covered by this event) is untested. Confirm the p95 panel renders
plausible numbers on the first real deploy before trusting a sizing decision off this dashboard.

Edit this generator, then ``python tools/dashboard-gen/generate.py`` and commit the regenerated
``deploy/observability/dashboards/prompt-budget.json`` (CI diffs it).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, text, timeseries
from grafana_foundation_sdk.models.text import TextMode

from .common import CRI_UNWRAP, LOKI, Layout, logql

UID = "lci-prompt-budget"

# Real Loki labels on this stream (see task_runs.py's own comment on this — a prior `{app=~"..."}`
# selector referenced a label THIS LOKI DOES NOT HAVE and silently matched zero streams). Aggregate
# across ALL review/index runs, unlike task_runs.py's single-task `pod=~"lightbridge-agent-$id-.*"`.
_SELECTOR = '{namespace="lightbridge-agents", pod=~"lightbridge-agent-.*"}'

# CRI unwrap + JSON parse (see "Gotcha #1" in the module docstring). `<_>` discards the CRI
# timestamp/stream/flag fields; `content` captures the embedded JSON payload verbatim so `| json`
# has clean input. `| __error__=""` after `| json` drops any line that still fails to parse (e.g. a
# non-JSON line that happened to also contain the filter substring below) before it can pollute an
# aggregation.
_CRI_JSON = (
    f"{_SELECTOR} "
    '|= "prompt budget usage" '
    f"{CRI_UNWRAP} "
    '| json | __error__=""'
)

# `$preset` narrows to one tier ("All" -> `.+`, matching every preset including any
# operator-defined one beyond fast/deep/ultra). `fields_preset` per "Gotcha #2" above.
_STREAM = f'{_CRI_JSON} | fields_preset =~ "^$preset$"'


def _unwrap(field: str) -> str:
    """A range-vector unwrapping `field` off the parsed prompt-budget event stream.

    `{field} != ""` filters out lines where the field was never extracted (absent from the JSON —
    e.g. a diff-less `ask` run has no `diff_*` fields at all) BEFORE `unwrap`, not just via the
    post-unwrap `__error__=""` that used to be the only guard here. This is load-bearing, not
    belt-and-suspenders duplication — confirmed against a real Loki instance (see the module
    docstring's "avg_over_time silently divides by the wrong count" section): `avg_over_time`
    divides its sum by the count of ALL log lines Loki attempted to unwrap over the range, including
    ones the post-unwrap `__error__=""` filter later drops from the sum — not the count of lines that
    actually contributed a value. Any preset group mixing diff-bearing and diff-less runs (the
    everyday case) silently understated every average until this pre-filter was added.
    `sum_over_time`/`quantile_over_time` were confirmed NOT to have this bug (summing doesn't divide;
    a percentile is read off the actual sorted value list, not a raw line count) — the pre-filter is
    applied uniformly anyway since this is the one place all three functions share it, and it's
    strictly correct either way. The post-unwrap `__error__=""` stays as a defensive backstop for a
    genuinely-non-numeric value, which `!= ""` alone would not catch.
    """
    return f'{_STREAM} | {field} != "" | unwrap {field} | __error__=""'


def _quantile(q: float, field: str, rng: str = "$__range") -> str:
    """True per-preset quantile of `field` over `rng` (native `by` grouping — see the module
    docstring's "Grouping" section for why this must NOT be an outer wrap around per-pod
    percentiles)."""
    return f"quantile_over_time({q}, {_unwrap(field)} [{rng}]) by (fields_preset)"


def _sum(field: str, rng: str = "$__range") -> str:
    """`sum_over_time` does NOT support a native `by (...)` grouping clause — confirmed against a
    real Loki instance (see the module docstring's "Grouping" section) and against the Grafana docs:
    "Except for `sum_over_time`, `absent_over_time`, `rate` and `rate_counter`, unwrapped range
    aggregations support grouping." `sum_over_time(...) by (fields_preset)` is a LogQL parse error
    ("grouping not allowed for sum_over_time aggregation"), not an empty/degenerate result — so the
    fix is a Prometheus-style vector-aggregation wrapper (`sum by (...) (...)`) around the range
    aggregation instead of a grouping clause on the range aggregation itself.
    """
    return f"sum by (fields_preset) (sum_over_time({_unwrap(field)} [{rng}]))"


def _avg(field: str, rng: str = "$__range") -> str:
    """`avg_over_time` IS one of the unwrapped range aggregations that supports native `by (...)`
    grouping (confirmed against real Loki — see the module docstring), unlike `_sum`/`_count` above."""
    return f"avg_over_time({_unwrap(field)} [{rng}]) by (fields_preset)"


def _count(expr: str, rng: str = "$__range") -> str:
    """`count_over_time` is a plain (non-`unwrap`) log range aggregation, and none of those
    (`count_over_time`, `bytes_over_time`, `rate`, `bytes_rate`) support a native `by (...)` clause
    at all — confirmed against real Loki the same way as `_sum` above. Same vector-aggregation-wrapper
    fix."""
    return f"sum by (fields_preset) (count_over_time({expr} [{rng}]))"


# Coverage-loss ratio: what share of diff-bearing runs lost whole-file coverage (`diff_files_omitted
# > 0`), vs. the total number of diff-bearing runs (`fields_diff_files_total != ""` excludes
# diff-less `ask` runs, which never emit diff fields at all — an absent field isn't a zero). Both
# sides group `by (fields_preset)` so the division matches per-preset series against each other.
_OMITTED_RUNS = f'{_STREAM} | fields_diff_files_omitted > 0'
_ALL_DIFF_RUNS = f'{_STREAM} | fields_diff_files_total != ""'
_COVERAGE_LOSS_PCT = f"100 * ({_count(_OMITTED_RUNS)}) / ({_count(_ALL_DIFF_RUNS)})"


_HOW_TO_READ = """\
### How to read this dashboard

**The number that answers "raise `review.max_diff_chars` to what?" is the p95 (or p99) of
`diff_source_bytes`, by preset** — the two bar-gauge panels below. `diff_source_bytes` is the real
diff demand *after* low-signal files (lockfiles, generated code) are already dropped: it is
everything a bigger cap could plausibly let through.

1. **Pick a percentile you're comfortable never truncating below.** p95 is a reasonable default;
   use p99 if losing whole-file coverage on the long tail matters more than a modestly bigger
   prompt.
2. **Read it off the preset you're sizing**, not a blended number — `fast` (120000 in prod) and
   `deep` (300000) have different models and context windows, so they must be sized independently.
   Use the `preset` variable to isolate one, or leave "All" to compare both side by side.
3. **Cross-check "Coverage loss — runs with omitted files".** If a meaningful share of runs on a
   preset already have `diff_files_omitted > 0`, the current cap isn't just trimming a hunk, it's
   dropping whole files the reviewer never saw — that's a stronger signal to raise the cap than the
   percentile alone.
4. **Cross-check "Effective diff budget avg" against the configured `max_diff_chars`.**
   `diff_budget_bytes` is the *effective* per-run budget after the ADR-0070 window-proportional
   share (`min(max_diff_chars, 0.25 × context_window × 4)`). If it sits well below the configured
   cap, the window share — not `max_diff_chars` — is the actual binding constraint, and raising the
   operator config alone won't move anything.
5. **Check the static-context-block panels before enlarging the diff share.** Priors, repo memory,
   instructions, and repo config all compete for the same context window budget as the diff; a
   bigger diff share shrinks theirs.

✅ **Empirically verified** (CRI-unwrap chain, `fields_` flattening, event disambiguation, the
diff-less exclusion, and every panel's grouping syntax) against a real local Loki using genuine
`tracing`-emitted log output — see "Verification status" in this dashboard's generator,
`tools/dashboard-gen/lci_dashboards/prompt_budget.py`, for the full method and versions tested.
⚠️ Not yet re-verified against the real production stream — confirm the p95 panel renders plausible
numbers on the first real deploy before trusting a sizing decision off this dashboard.
"""


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    preset_var = (
        dashboard.CustomVariable("preset")
        .label("Preset")
        .values("All : .+,fast,deep,ultra")
    )

    how_to_read = (
        text.Panel()
        .title("How to read this dashboard")
        .mode(TextMode.MARKDOWN)
        .content(_HOW_TO_READ)
        .grid_pos(layout.place(24, 9))
    )

    # --- The headline numbers: real diff demand, by preset ---
    p95_source = (
        bargauge.Panel()
        .title("Diff demand p95 (diff_source_bytes) — size the cap around this")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_quantile(0.95, "fields_diff_source_bytes")))
        .grid_pos(layout.place(12, 8))
    )
    p99_source = (
        bargauge.Panel()
        .title("Diff demand p99 (diff_source_bytes) — near-zero-truncation margin")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_quantile(0.99, "fields_diff_source_bytes")))
        .grid_pos(layout.place(12, 8))
    )

    p95_trend = (
        timeseries.Panel()
        .title("Diff demand p95 (diff_source_bytes) over time, by preset")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_quantile(0.95, "fields_diff_source_bytes", rng="$__interval")))
        .grid_pos(layout.place(24, 8))
    )

    # --- Supporting KPIs (range, filtered by $preset) ---
    runs_sampled = (
        stat.Panel()
        .title("Runs sampled (range)")
        .datasource(LOKI)
        .with_target(logql(_count(_STREAM)))
        .grid_pos(layout.place(6, 4))
    )
    p50_source = (
        stat.Panel()
        .title("Diff demand p50 (diff_source_bytes)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_quantile(0.50, "fields_diff_source_bytes")))
        .grid_pos(layout.place(6, 4))
    )
    avg_budget = (
        stat.Panel()
        .title("Effective diff budget avg (post ADR-0070 window share)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_avg("fields_diff_budget_bytes")))
        .grid_pos(layout.place(6, 4))
    )
    coverage_loss = (
        stat.Panel()
        .title("Coverage loss — runs with omitted files")
        .datasource(LOKI)
        .unit("percent")
        .with_target(logql(_COVERAGE_LOSS_PCT))
        .grid_pos(layout.place(6, 4))
    )

    # --- What the cap is actually throwing away ---
    discarded_trend = (
        timeseries.Panel()
        .title("Bytes discarded per interval (diff_omitted_bytes), by preset")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_sum("fields_diff_omitted_bytes", rng="$__interval")))
        .grid_pos(layout.place(12, 8))
    )
    files_omitted_total = (
        bargauge.Panel()
        .title("Files omitted, total (range), by preset")
        .datasource(LOKI)
        .with_target(logql(_sum("fields_diff_files_omitted")))
        .grid_pos(layout.place(12, 8))
    )

    # --- What else competes for the same context window (ADR-0070 static blocks) ---
    priors_avg = (
        stat.Panel()
        .title("Priors block avg rendered bytes (range)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_avg("fields_priors_rendered_bytes")))
        .grid_pos(layout.place(6, 4))
    )
    memory_avg = (
        stat.Panel()
        .title("Repo memory block avg rendered bytes (range)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_avg("fields_memory_rendered_bytes")))
        .grid_pos(layout.place(6, 4))
    )
    instructions_avg = (
        stat.Panel()
        .title("Instructions block avg rendered bytes (range)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_avg("fields_instructions_rendered_bytes")))
        .grid_pos(layout.place(6, 4))
    )
    repo_config_avg = (
        stat.Panel()
        .title("Repo config block avg rendered bytes (range)")
        .datasource(LOKI)
        .unit("bytes")
        .with_target(logql(_avg("fields_repo_config_rendered_bytes")))
        .grid_pos(layout.place(6, 4))
    )

    return (
        dashboard.Dashboard("Lightbridge — Prompt Budget")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("5m")
        .time("now-7d", "now")
        .with_variable(preset_var)
        .with_panel(how_to_read)
        .with_panel(p95_source)
        .with_panel(p99_source)
        .with_panel(p95_trend)
        .with_panel(runs_sampled)
        .with_panel(p50_source)
        .with_panel(avg_budget)
        .with_panel(coverage_loss)
        .with_panel(discarded_trend)
        .with_panel(files_omitted_total)
        .with_panel(priors_avg)
        .with_panel(memory_avg)
        .with_panel(instructions_avg)
        .with_panel(repo_config_avg)
    )
