"""Review-quality dashboard — surfaces what the review agent actually produced.

Two datasources, on purpose:

- **Findings & reactions are Postgres-sourced.** The agent runs as a one-shot Kubernetes Job, so
  its output can't be pull-scraped into Prometheus. It is already persisted — ``reviews.findings``
  (ADR-0032 priority/category), ``review_feedback`` (ADR-0035 reactions) — and was simply never
  charted. These panels read it.
- **Token / model usage is Loki-sourced** from the AI-Gateway (eaig) billing stream — the *same*
  ``envoy-ai-gateway`` stream ``review_cost.py`` reads. The DB run-transcript that used to carry
  per-run tokens/model was torn out (ADR-0100, logs-as-observability), so the authoritative
  per-review token/model figures now come from the gateway logs, not Postgres.

Gateway stream fields used here (see ``review_cost.py`` for the full contract):
  - ``gen_ai_usage_total_tokens`` — total tokens per call. The gateway logs a **single total**;
    it does NOT break tokens out into input / output / reasoning. So the token time-series is a
    total, not a stacked input/output/reasoning split — that split would be fabricated here.
  - ``gen_ai_request_model`` — the gateway model alias (drives model distribution).
  - ``account_id`` — the repo (``owner/name``); filtered by ``$repo``.
  - ``oidc_jti`` — ``runid:<task_id>``, one value per review run (per-review grouping key).

Reasoning tokens: intentionally NOT charted. The gateway stream carries no reasoning-token field
(only the ``gen_ai_usage_total_tokens`` total above), and the one place a reasoning slice exists —
the logger plugin's ``session.updated`` line, nested ``properties.info.tokens.reasoning`` — is
known to report ``reasoning:0`` for eaig and is deferred to a follow-up (#472). Rebuilding a
reasoning panel from a field the stream can't back would be fabrication, so it is omitted.

The token panels are scoped by the ``$repo`` / ``$model`` template variables (mirroring
``review_cost.py``); those variables do not affect the Postgres findings/reactions panels.
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, table, timeseries
from grafana_foundation_sdk.models.dashboard import VariableRefresh

from .common import LOKI, POSTGRES, Layout, logql, sql

UID = "lci-review-quality"

# --- AI-Gateway (eaig) token stream (mirrors review_cost.py's _STREAM) ---
# The LCI-scoped, JSON-parsed, filtered gateway stream. `$repo` / `$model` default to `.+` (all)
# via the template vars below, so an unset filter matches every LCI run/model. `oidc_jti=~"runid:.+"`
# scopes to LCI review traffic (only our runs carry a runid), excluding other gateway callers.
_STREAM = (
    '{service_name="envoy-ai-gateway"} | json '
    '| oidc_jti=~"runid:.+" '
    '| account_id=~"$repo" '
    '| gen_ai_request_model=~"$model"'
)


def _unwrap(field: str) -> str:
    """A range-vector unwrapping `field` (drops non-numeric lines via `__error__`)."""
    return f"{_STREAM} | unwrap {field} | __error__=\"\""


# Total tokens over the panel range (KPI) and per interval (trend). The gateway logs only a total
# per call, so this is a single total series — NOT an input/output/reasoning split.
_TOKENS_RANGE = f"sum(sum_over_time({_unwrap('gen_ai_usage_total_tokens')} [$__range]))"
_TOKENS_INTERVAL = f"sum(sum_over_time({_unwrap('gen_ai_usage_total_tokens')} [$__interval]))"
# Distinct review runs per model = distinct `oidc_jti` (any call in range) grouped by model.
_RUNS_BY_MODEL = (
    "count by (gen_ai_request_model) "
    f"(sum by (oidc_jti, gen_ai_request_model) (count_over_time({_STREAM} [$__range])))"
)

# Effective triage priority for a finding row `f` (a `jsonb_array_elements(findings)` element),
# mirroring Finding::priority in services/control-plane/src/review.rs: explicit P0/P1/P2, else the
# legacy `severity` shimmed (error/critical→P0, warning→P1, else→P2), else P2.
_PRIORITY_EXPR = (
    "CASE "
    "WHEN upper(coalesce(f->>'priority','')) IN ('P0','P1','P2') THEN upper(f->>'priority') "
    "WHEN lower(coalesce(f->>'severity','')) IN ('error','critical') THEN 'P0' "
    "WHEN lower(coalesce(f->>'severity','')) = 'warning' THEN 'P1' "
    "ELSE 'P2' END"
)
# Effective category; defaults to 'correctness' when absent (Finding::category).
_CATEGORY_EXPR = "coalesce(nullif(f->>'category',''), 'correctness')"

# Findings exploded to one row per finding (column `f` = the finding object, so the bare `f->>...`
# in _PRIORITY_EXPR/_CATEGORY_EXPR resolves to it), joined to review time for $__timeFilter.
_FINDINGS_CTE = (
    "WITH rf AS ("
    "  SELECT rv.task_id, rv.created_at, je AS f "
    "  FROM reviews rv, jsonb_array_elements(rv.findings) je"
    ")"
)


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    # --- Template variables (scope the Loki token panels; mirror review_cost.py) ---
    # `repo` enumerates from Postgres (the app knows its repos), baking "All" → `.+` into the SQL.
    # `model` is sourced from the Loki gateway stream the token panels read (`gen_ai_request_model`),
    # with Grafana-native include_all + all_value ".+". These variables do NOT touch the Postgres
    # findings/reactions panels below.
    repo_var = (
        dashboard.QueryVariable("repo")
        .label("Repository")
        .datasource(POSTGRES)
        .query(
            "SELECT __text, __value FROM ("
            "  SELECT 'All' AS __text, '.+' AS __value, 0 AS ord "
            "  UNION ALL "
            "  SELECT owner || '/' || name AS __text, owner || '/' || name AS __value, 1 AS ord "
            "  FROM repositories"
            ") t ORDER BY ord, __text"
        )
    )
    model_var = (
        dashboard.QueryVariable("model")
        .label("Model")
        .datasource(LOKI)
        .query("label_values(gen_ai_request_model)")
        .include_all(True)
        .all_value(".+")
        .refresh(VariableRefresh.ON_TIME_RANGE_CHANGED)
    )

    reviews_finalized = (
        stat.Panel()
        .title("Reviews finalized")
        .datasource(POSTGRES)
        .with_target(sql("SELECT count(*) FROM reviews WHERE $__timeFilter(created_at)"))
        .grid_pos(layout.place(6, 4))
    )
    total_findings = (
        stat.Panel()
        .title("Findings")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM reviews rv, jsonb_array_elements(rv.findings) f "
                "WHERE $__timeFilter(rv.created_at)"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    p0_findings = (
        stat.Panel()
        .title("P0 findings")
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"SELECT count(*) FROM ({_FINDINGS_CTE} "
                f"SELECT 1 FROM rf WHERE $__timeFilter(rf.created_at) AND {_PRIORITY_EXPR} = 'P0') s"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    # Loki-sourced token KPI (was Postgres DB run-transcript before ADR-0100). Total tokens billed
    # across all matching gateway calls in range.
    tokens_total = (
        stat.Panel()
        .title("Tokens (range)")
        .datasource(LOKI)
        .with_target(logql(_TOKENS_RANGE))
        .grid_pos(layout.place(6, 4))
    )
    findings_by_priority = (
        timeseries.Panel()
        .title("Findings by priority")
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"{_FINDINGS_CTE} "
                f"SELECT $__timeGroupAlias(rf.created_at, $__interval), {_PRIORITY_EXPR} AS \"priority\", "
                "count(*) AS \"findings\" "
                "FROM rf WHERE $__timeFilter(rf.created_at) GROUP BY 1, 2 ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(12, 8))
    )
    findings_by_category = (
        bargauge.Panel()
        .title("Findings by category")
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"{_FINDINGS_CTE} "
                f"SELECT {_CATEGORY_EXPR} AS metric, count(*) AS value "
                "FROM rf WHERE $__timeFilter(rf.created_at) GROUP BY 1 ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # Loki token trend. The gateway logs one total per call, so this is total tokens per interval —
    # not an input/output/reasoning split (the stream carries no such breakdown; see module docstring).
    tokens_over_time = (
        timeseries.Panel()
        .title("Token usage (total)")
        .datasource(LOKI)
        .with_target(logql(_TOKENS_INTERVAL))
        .grid_pos(layout.place(12, 8))
    )
    models_used = (
        bargauge.Panel()
        .title("Runs by model")
        .datasource(LOKI)
        .with_target(logql(_RUNS_BY_MODEL))
        .grid_pos(layout.place(12, 8))
    )
    feedback = (
        bargauge.Panel()
        .title("Reviewer reactions")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT reaction AS metric, count(*) AS value "
                "FROM review_feedback rf JOIN tasks t ON t.id = rf.task_id "
                "WHERE $__timeFilter(t.created_at) GROUP BY reaction ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # Postgres per-review table — the non-token columns #471 kept (reviewed/repo/pr/findings/
    # inline/deferred/out_of_scope). Token/model live in Loki keyed by oidc_jti (a different
    # datasource), so they can't be columns here; see the Loki per-run table below.
    per_review = (
        table.Panel()
        .title("Recent reviews")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT rv.created_at AS \"reviewed\", "
                "coalesce(r.owner || '/' || r.name, t.repository_id::text) AS \"repository\", "
                "t.target_id AS \"pr\", "
                "jsonb_array_length(rv.findings) AS \"findings\", "
                "rv.inline_count AS \"inline\", rv.deferred_count AS \"deferred\", "
                "rv.out_of_scope_count AS \"out_of_scope\" "
                "FROM reviews rv JOIN tasks t ON t.id = rv.task_id "
                "LEFT JOIN repositories r ON r.id = t.repository_id "
                "WHERE $__timeFilter(rv.created_at) "
                "ORDER BY rv.created_at DESC LIMIT 50"
            )
        )
        .grid_pos(layout.place(24, 10))
    )

    # Loki per-run model + tokens (restores the per-review model/token columns removed in #471,
    # now from the gateway stream keyed by oidc_jti = `runid:<task_id>`). Mirrors review_cost.py's
    # per_run table. No reasoning column — the gateway stream carries no reasoning-token field.
    per_run_tokens = (
        table.Panel()
        .title("Review runs × tokens × model (range)")
        .datasource(LOKI)
        .with_target(
            logql(
                "topk(500, sum by (oidc_jti, account_id, gen_ai_request_model) "
                f"(sum_over_time({_unwrap('gen_ai_usage_total_tokens')} [$__range])))"
            )
        )
        .grid_pos(layout.place(24, 12))
    )

    return (
        dashboard.Dashboard("Lightbridge — Review quality")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("1m")
        .time("now-30d", "now")
        .with_variable(repo_var)
        .with_variable(model_var)
        .with_panel(reviews_finalized)
        .with_panel(total_findings)
        .with_panel(p0_findings)
        .with_panel(tokens_total)
        .with_panel(findings_by_priority)
        .with_panel(findings_by_category)
        .with_panel(tokens_over_time)
        .with_panel(models_used)
        .with_panel(feedback)
        .with_panel(per_review)
        .with_panel(per_run_tokens)
    )
