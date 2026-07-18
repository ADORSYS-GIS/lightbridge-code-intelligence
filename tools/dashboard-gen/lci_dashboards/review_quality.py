"""Review-quality dashboard — surfaces what the review agent actually produced.

Everything here is Postgres-sourced on purpose: the agent runs as a one-shot Kubernetes Job, so its
output (findings, reactions) can't be pull-scraped into Prometheus. It is already persisted —
``reviews.findings`` (ADR-0032 priority/category), ``review_feedback`` (ADR-0035 reactions) — and
was simply never charted. This dashboard reads it.

Per-run token usage is no longer charted here: the DB run-transcript table was torn out
(ADR-0100, logs-as-observability). Authoritative per-review token/cost figures come from the
AI-Gateway (eaig) billing logs in Loki, not from Postgres.
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, table, timeseries

from .common import POSTGRES, Layout, sql

UID = "lci-review-quality"

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

    return (
        dashboard.Dashboard("Lightbridge — Review quality")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("1m")
        .time("now-30d", "now")
        .with_panel(reviews_finalized)
        .with_panel(total_findings)
        .with_panel(p0_findings)
        .with_panel(findings_by_priority)
        .with_panel(findings_by_category)
        .with_panel(feedback)
        .with_panel(per_review)
    )
