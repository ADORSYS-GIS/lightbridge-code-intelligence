"""Overview dashboard — the at-a-glance KPIs that the web UI's landing page used to show."""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, table, timeseries

from .common import LOKI, POSTGRES, Layout, logql, sql

UID = "lci-overview"

# 24h total tokens from the AI-Gateway (eaig) billing stream — the same envoy-ai-gateway Loki
# stream review-cost / review-quality read (ADR-0100 moved per-run tokens off the dropped DB
# run-transcript table onto these logs). `oidc_jti=~"runid:.+"` scopes to LCI review traffic;
# the gateway logs a single total per call (no input/output/reasoning split). Fixed 24h window to
# match the sibling "(24h)" KPIs, independent of the dashboard's selected range.
_TOKENS_24H = (
    "sum(sum_over_time("
    '{service_name="envoy-ai-gateway"} | json | oidc_jti=~"runid:.+" '
    '| unwrap gen_ai_usage_total_tokens | __error__="" [24h]))'
)


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    queued = (
        stat.Panel()
        .title("Queued")
        .datasource(POSTGRES)
        .with_target(sql("SELECT count(*) FROM tasks WHERE status = 'queued'"))
        .grid_pos(layout.place(6, 4))
    )
    running = (
        stat.Panel()
        .title("Running")
        .datasource(POSTGRES)
        .with_target(sql("SELECT count(*) FROM tasks WHERE status = 'running'"))
        .grid_pos(layout.place(6, 4))
    )
    failed = (
        stat.Panel()
        .title("Failed (24h)")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM tasks "
                "WHERE status = 'failed' AND created_at > now() - interval '24 hours'"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    deliveries = (
        stat.Panel()
        .title("Deliveries (24h)")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM github_deliveries "
                "WHERE received_at > now() - interval '24 hours'"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    tokens_24h = (
        stat.Panel()
        .title("Tokens (24h)")
        .datasource(LOKI)
        .with_target(logql(_TOKENS_24H))
        .grid_pos(layout.place(6, 4))
    )
    created = (
        timeseries.Panel()
        .title("Tasks created")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT $__timeGroupAlias(created_at, $__interval), count(*) AS \"tasks\" "
                "FROM tasks WHERE $__timeFilter(created_at) GROUP BY 1 ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(12, 8))
    )
    by_status = (
        bargauge.Panel()
        .title("Tasks by status")
        .datasource(POSTGRES)
        .with_target(
            sql("SELECT status AS metric, count(*) AS value FROM tasks GROUP BY status ORDER BY value DESC")
        )
        .grid_pos(layout.place(12, 8))
    )

    duration_p95 = (
        timeseries.Panel()
        .title("Run duration p95 by command")
        .datasource(POSTGRES)
        .unit("s")
        .with_target(
            sql(
                "SELECT $__timeGroupAlias(completed_at, $__interval), command_text AS \"command\", "
                "percentile_cont(0.95) WITHIN GROUP "
                "(ORDER BY extract(epoch FROM (completed_at - started_at))) AS \"p95\" "
                "FROM tasks WHERE $__timeFilter(completed_at) "
                "AND started_at IS NOT NULL AND completed_at IS NOT NULL "
                "GROUP BY 1, 2 ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(24, 8))
    )

    recent = (
        table.Panel()
        .title("Recent runs")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT t.created_at AS \"created\", "
                "coalesce(r.owner || '/' || r.name, t.repository_id::text) AS \"repository\", "
                "t.target_type AS \"target\", t.target_id AS \"number\", t.status, t.attempts "
                "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
                "ORDER BY t.created_at DESC LIMIT 20"
            )
        )
        .grid_pos(layout.place(24, 9))
    )

    return (
        dashboard.Dashboard("Lightbridge — Overview")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("30s")
        .time("now-24h", "now")
        .with_panel(queued)
        .with_panel(running)
        .with_panel(failed)
        .with_panel(deliveries)
        .with_panel(tokens_24h)
        .with_panel(created)
        .with_panel(by_status)
        .with_panel(duration_p95)
        .with_panel(recent)
    )
