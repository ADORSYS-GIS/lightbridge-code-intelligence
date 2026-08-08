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

--- Gotcha #1: pod logs are still CRI-wrapped ---

Alloy tails these pods with ``loki.source.file`` straight off ``/var/log/pods/...`` and its
pipeline has NO ``stage.cri``, so every line in Loki still looks like:

    2026-08-08T04:01:44.123Z stdout F {"timestamp":"...","level":"INFO","fields":{...},"target":"..."}

A bare ``| json`` fails with ``JSONParserErr`` on this — it can't parse past the leading
``<ts> <stream> <flag>`` prefix. ``| cri`` is not a valid stage on this Loki version either
(``task_runs.py`` hit this first and left it as a documented follow-up). The fix used here: a
``| pattern`` stage that captures everything after the three leading CRI fields (discarded via
``<_>``) into a named ``content`` field, ``| line_format`` to replace the log line with just that
capture, THEN ``| json`` — see ``_CRI_JSON`` below.

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

Every aggregation below groups explicitly ``by (fields_preset)`` (the Loki-documented native
grouping clause for unwrapped range aggregations, e.g. ``quantile_over_time(0.99, <expr> [5m]) by
(host)`` in the Loki docs) rather than computing per-pod-stream numbers and combining them
afterwards. This matters specifically for the percentile queries: computing a percentile per pod
(each review run gets its own ephemeral pod, ``lightbridge-agent-<task-id>-*``) and then averaging
or maxing those per-pod percentiles together would NOT equal the true percentile over the merged
raw samples (percentile-of-percentiles is not a percentile) — the native ``by`` clause avoids that
fallacy by computing one true quantile per preset group directly from the underlying values.

--- Honesty / verification status ---

⚠️ These queries are written against the documented CRI + ``fields_`` shape above, reasoned through
by hand — this repo has NO live Loki access from the environment that generated this dashboard, so
NONE of the LogQL below has been run against real data. Treat every panel as unverified until an
operator confirms at least the headline percentile panel renders real numbers. An empty panel here
is the expected failure mode to check for, not a sign everything is fine.

Edit this generator, then ``python tools/dashboard-gen/generate.py`` and commit the regenerated
``deploy/observability/dashboards/prompt-budget.json`` (CI diffs it).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, text, timeseries
from grafana_foundation_sdk.models.text import TextMode

from .common import LOKI, Layout, logql

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
    "| pattern `<_> <_> <_> <content>` "
    "| line_format `{{.content}}` "
    '| json | __error__=""'
)

# `$preset` narrows to one tier ("All" -> `.+`, matching every preset including any
# operator-defined one beyond fast/deep/ultra). `fields_preset` per "Gotcha #2" above.
_STREAM = f'{_CRI_JSON} | fields_preset =~ "^$preset$"'


def _unwrap(field: str) -> str:
    """A range-vector unwrapping `field` off the parsed prompt-budget event stream, dropping
    unwrap-time conversion errors (a line where the field was absent or non-numeric)."""
    return f'{_STREAM} | unwrap {field} | __error__=""'


def _quantile(q: float, field: str, rng: str = "$__range") -> str:
    """True per-preset quantile of `field` over `rng` (native `by` grouping — see the module
    docstring's "Grouping" section for why this must NOT be an outer wrap around per-pod
    percentiles)."""
    return f"quantile_over_time({q}, {_unwrap(field)} [{rng}]) by (fields_preset)"


def _sum(field: str, rng: str = "$__range") -> str:
    return f"sum_over_time({_unwrap(field)} [{rng}]) by (fields_preset)"


def _avg(field: str, rng: str = "$__range") -> str:
    return f"avg_over_time({_unwrap(field)} [{rng}]) by (fields_preset)"


def _count(expr: str, rng: str = "$__range") -> str:
    return f"count_over_time({expr} [{rng}]) by (fields_preset)"


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

⚠️ **Unverified.** These LogQL queries are written against the documented CRI-wrapped +
`fields_`-nested log shape (see this dashboard's generator, `tools/dashboard-gen/lci_dashboards/\
prompt_budget.py`) but have not been run against live Loki. Confirm the p95 panel actually renders
numbers before making a sizing decision off this dashboard — an empty panel here means the query is
wrong, not that there's no data.
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
