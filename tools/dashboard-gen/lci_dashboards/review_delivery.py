"""Review delivery dashboard — review state, outbox delivery, and finding payloads."""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, table, timeseries
from grafana_foundation_sdk.models.dashboard import DynamicConfigValue, VariableRefresh

from .common import POSTGRES, Layout, sql

UID = "lci-review-delivery"

_WRAP_CELL = [
    DynamicConfigValue(id_val="custom.cellOptions", value={"type": "auto", "wrapText": True})
]


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    platform_var = (
        dashboard.CustomVariable("platform")
        .label("Platform")
        .values("All,github,gitlab,bitbucket")
    )
    task_id_var = (
        dashboard.QueryVariable("task_id")
        .label("Task ID (drill-down)")
        .datasource(POSTGRES)
        .refresh(VariableRefresh.ON_TIME_RANGE_CHANGED)
        .query(
            "SELECT t.id::text AS \"__value\", "
            "coalesce(r.owner || '/' || r.name, t.repository_id::text) || ' #' || t.target_id "
            "  || ' · ' || t.status || ' · ' || to_char(t.created_at, 'MM-DD HH24:MI') AS \"__text\" "
            "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
            "WHERE $__timeFilter(t.created_at) AND t.command_text <> 'index' "
            "ORDER BY t.created_at DESC, t.id DESC LIMIT 300"
        )
    )

    stat_2xx = (
        stat.Panel()
        .title("2xx Posted")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM outbox o "
                "WHERE o.kind = 'review' AND o.status = 'posted' AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}')"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    stat_4xx = (
        stat.Panel()
        .title("4xx Client Errors")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM outbox o "
                "WHERE o.kind = 'review' AND o.status = 'failed' AND o.last_error ~ '[4][0-9]{2}' "
                "AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}')"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    stat_401_403 = (
        stat.Panel()
        .title("401/403 Auth Failures")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM outbox o "
                "WHERE o.kind = 'review' AND o.status = 'failed' AND o.last_error ~ '401|403' "
                "AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}')"
            )
        )
        .grid_pos(layout.place(6, 4))
    )
    stat_5xx = (
        stat.Panel()
        .title("5xx Server Errors")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT count(*) FROM outbox o "
                "WHERE o.kind = 'review' AND o.status = 'failed' AND o.last_error ~ '[5][0-9]{2}' "
                "AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}')"
            )
        )
        .grid_pos(layout.place(6, 4))
    )

    task_state = (
        bargauge.Panel()
        .title("Review task state")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT t.status AS metric, count(*) AS value "
                "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
                "WHERE $__timeFilter(t.created_at) AND t.command_text <> 'index' "
                "AND ('${platform}' = 'All' OR r.platform::text = '${platform}') "
                "GROUP BY t.status ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(6, 8))
    )
    outbox_state = (
        bargauge.Panel()
        .title("Outbox state by kind")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT o.kind || ' / ' || o.status AS metric, count(*) AS value "
                "FROM outbox o "
                "WHERE $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}') "
                "GROUP BY o.kind, o.status ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(6, 8))
    )
    review_gap = (
        bargauge.Panel()
        .title("Succeeded reviews by delivery state")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT CASE "
                "  WHEN rv.task_id IS NOT NULL THEN 'persisted review' "
                "  WHEN EXISTS (SELECT 1 FROM outbox o WHERE o.task_id = t.id AND o.kind = 'review' "
                "               AND o.status = 'pending') THEN 'queued/backing off' "
                "  WHEN EXISTS (SELECT 1 FROM outbox o WHERE o.task_id = t.id AND o.kind = 'review' "
                "               AND o.status = 'failed') THEN 'dead-lettered' "
                "  WHEN t.error_detail IS NOT NULL THEN 'no post with detail' "
                "  ELSE 'no review row/outbox' END AS metric, "
                "count(*) AS value "
                "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
                "LEFT JOIN reviews rv ON rv.task_id = t.id "
                "WHERE t.status = 'succeeded' AND t.command_text <> 'index' AND $__timeFilter(t.created_at) "
                "AND ('${platform}' = 'All' OR r.platform::text = '${platform}') "
                "GROUP BY 1 ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(6, 8))
    )

    http_status_breakdown = (
        bargauge.Panel()
        .title("HTTP status breakdown")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT coalesce(substring(o.last_error FROM '\\b([45]\\d{2})\\b'), 'other') AS metric, "
                "count(*) AS value "
                "FROM outbox o "
                "WHERE o.kind = 'review' AND o.status IN ('pending', 'failed') AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}') "
                "GROUP BY 1 ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(6, 8))
    )

    outbox_over_time = (
        timeseries.Panel()
        .title("Outbox deliveries over time")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT $__timeGroupAlias(o.created_at, $__interval), "
                "o.kind || ' / ' || o.status AS \"series\", count(*) AS \"count\" "
                "FROM outbox o "
                "WHERE $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}') "
                "GROUP BY 1, 2 ORDER BY 1, 2",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(24, 8))
    )

    failed_outbox = (
        table.Panel()
        .title("Review post failures and retries")
        .description("Review outbox rows that failed or are backing off, with the platform response kept in last_error.")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT o.created_at AS \"queued\", o.id AS \"outbox id\", o.platform, "
                "coalesce(o.owner || '/' || o.repo, t.repository_id::text) AS \"repository\", "
                "t.target_type AS \"target\", t.target_id AS \"number\", t.id AS \"task id\", "
                "o.status, o.attempts, o.next_attempt_at AS \"next attempt\", "
                "o.last_error AS \"last response\", "
                "jsonb_array_length(coalesce(o.payload->'comments', '[]'::jsonb)) AS \"inline\", "
                "coalesce(jsonb_array_length(o.payload->'findings_json'), 0) AS \"findings\" "
                "FROM outbox o LEFT JOIN tasks t ON t.id = o.task_id "
                "WHERE o.kind = 'review' AND o.status IN ('pending','failed') AND $__timeFilter(o.created_at) "
                "AND ('${platform}' = 'All' OR o.platform::text = '${platform}') "
                "ORDER BY o.created_at DESC, o.id DESC LIMIT 100"
            )
        )
        .override_by_name("last response", _WRAP_CELL)
        .grid_pos(layout.place(24, 10))
    )

    findings = (
        table.Panel()
        .title("Review findings from persisted and queued reviews")
        .description("Shows findings already persisted in reviews plus findings still sitting in review outbox payloads.")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "WITH persisted AS ("
                "  SELECT rv.created_at, 'persisted' AS source, NULL::bigint AS outbox_id, "
                "         t.id AS task_id, r.platform::text AS platform, r.owner || '/' || r.name AS repository, "
                "         t.target_type, t.target_id, f "
                "  FROM reviews rv JOIN tasks t ON t.id = rv.task_id "
                "  LEFT JOIN repositories r ON r.id = t.repository_id "
                "  CROSS JOIN LATERAL jsonb_array_elements(rv.findings) f "
                "  WHERE $__timeFilter(rv.created_at)"
                "), queued AS ("
                "  SELECT o.created_at, o.status AS source, o.id AS outbox_id, "
                "         t.id AS task_id, o.platform::text AS platform, o.owner || '/' || o.repo AS repository, "
                "         t.target_type, t.target_id, f "
                "  FROM outbox o LEFT JOIN tasks t ON t.id = o.task_id "
                "  CROSS JOIN LATERAL jsonb_array_elements(coalesce(o.payload->'findings_json', '[]'::jsonb)) f "
                "  WHERE o.kind = 'review' AND $__timeFilter(o.created_at)"
                ") "
                "SELECT created_at AS \"time\", source, outbox_id AS \"outbox id\", platform, repository, "
                "target_type AS \"target\", target_id AS \"number\", task_id AS \"task id\", "
                "coalesce(nullif(f->>'priority',''), nullif(f->>'severity',''), 'P2') AS \"priority\", "
                "coalesce(nullif(f->>'category',''), 'correctness') AS \"category\", "
                "f->>'file' AS \"file\", f->>'line' AS \"line\", f->>'title' AS \"title\", "
                "f->>'body' AS \"body\" "
                "FROM (SELECT * FROM persisted UNION ALL SELECT * FROM queued) x "
                "WHERE ('${platform}' = 'All' OR x.platform = '${platform}') "
                "ORDER BY created_at DESC LIMIT 300"
            )
        )
        .override_by_name("body", _WRAP_CELL)
        .grid_pos(layout.place(24, 12))
    )

    selected_task = (
        table.Panel()
        .title("Selected task state: $task_id")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT t.created_at AS \"created\", t.started_at AS \"started\", t.completed_at AS \"completed\", "
                "coalesce(r.owner || '/' || r.name, t.repository_id::text) AS \"repository\", "
                "r.platform, t.target_type AS \"target\", t.target_id AS \"number\", "
                "t.command_text AS \"command\", t.kind, t.preset, t.entry_point, t.status, "
                "t.attempts, t.error_detail AS \"detail\", t.head_sha, t.id AS \"task id\" "
                "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
                "WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring}"
            )
        )
        .override_by_name("detail", _WRAP_CELL)
        .grid_pos(layout.place(24, 6))
    )

    selected_outbox = (
        table.Panel()
        .title("Selected task outbox: $task_id")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT o.created_at AS \"queued\", o.posted_at AS \"posted\", o.id AS \"outbox id\", "
                "o.platform, o.kind, o.status, o.attempts, o.next_attempt_at AS \"next attempt\", "
                "o.platform_ref_id AS \"platform id\", o.last_error AS \"last response\", "
                "jsonb_array_length(coalesce(o.payload->'comments', '[]'::jsonb)) AS \"inline\", "
                "coalesce(jsonb_array_length(o.payload->'findings_json'), 0) AS \"findings\", "
                "o.payload::text AS \"payload\" "
                "FROM outbox o "
                "WHERE ${task_id:sqlstring} <> '' AND o.task_id::text = ${task_id:sqlstring} "
                "ORDER BY o.created_at DESC, o.id DESC"
            )
        )
        .override_by_name("last response", _WRAP_CELL)
        .override_by_name("payload", _WRAP_CELL)
        .grid_pos(layout.place(24, 11))
    )

    selected_comments = (
        table.Panel()
        .title("Selected task review comment payloads: $task_id")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT o.id AS \"outbox id\", c.ord AS \"index\", c.item->>'path' AS \"path\", "
                "c.item->>'line' AS \"line\", c.item->>'start_line' AS \"start line\", "
                "c.item->>'body' AS \"body\" "
                "FROM outbox o "
                "CROSS JOIN LATERAL jsonb_array_elements(coalesce(o.payload->'comments', '[]'::jsonb)) "
                "WITH ORDINALITY AS c(item, ord) "
                "WHERE ${task_id:sqlstring} <> '' AND o.task_id::text = ${task_id:sqlstring} "
                "AND o.kind = 'review' ORDER BY o.created_at DESC, c.ord"
            )
        )
        .override_by_name("body", _WRAP_CELL)
        .grid_pos(layout.place(12, 10))
    )

    selected_findings = (
        table.Panel()
        .title("Selected task findings: $task_id")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "WITH source AS ("
                "  SELECT 'persisted' AS source, rv.findings AS findings FROM reviews rv "
                "  WHERE ${task_id:sqlstring} <> '' AND rv.task_id::text = ${task_id:sqlstring} "
                "  UNION ALL "
                "  SELECT o.status AS source, coalesce(o.payload->'findings_json', '[]'::jsonb) AS findings "
                "  FROM outbox o WHERE ${task_id:sqlstring} <> '' AND o.task_id::text = ${task_id:sqlstring} "
                "  AND o.kind = 'review'"
                ") "
                "SELECT s.source, f->>'file' AS \"file\", f->>'line' AS \"line\", "
                "coalesce(nullif(f->>'priority',''), nullif(f->>'severity',''), 'P2') AS \"priority\", "
                "coalesce(nullif(f->>'category',''), 'correctness') AS \"category\", "
                "f->>'title' AS \"title\", f->>'body' AS \"body\" "
                "FROM source s CROSS JOIN LATERAL jsonb_array_elements(s.findings) f "
                "ORDER BY source, file, line"
            )
        )
        .override_by_name("body", _WRAP_CELL)
        .grid_pos(layout.place(12, 10))
    )

    posted_inline_comments = (
        table.Panel()
        .title("Selected task: posted inline comments + finding detail")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT rc.kind, rc.file, rc.line, rc.platform_comment_id AS \"comment id\", "
                "coalesce(nullif(f->>'priority',''), 'P2') AS priority, "
                "coalesce(nullif(f->>'category',''), 'correctness') AS category, "
                "f->>'title' AS title, f->>'body' AS body "
                "FROM review_comments rc "
                "JOIN reviews rv ON rv.task_id = rc.task_id "
                "CROSS JOIN LATERAL jsonb_array_elements(rv.findings) f "
                "WHERE ${task_id:sqlstring} <> '' AND rc.task_id::text = ${task_id:sqlstring} "
                "AND f->>'file' = rc.file AND (f->>'line')::int = rc.line "
                "ORDER BY rc.file, rc.line"
            )
        )
        .override_by_name("body", _WRAP_CELL)
        .grid_pos(layout.place(24, 10))
    )

    return (
        dashboard.Dashboard("Lightbridge — Review delivery")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("1m")
        .time("now-7d", "now")
        .description(
            "Review delivery and investigation dashboard: task state, outbox status, platform response "
            "errors, queued review payloads, and persisted or pending findings."
        )
        .with_variable(platform_var)
        .with_variable(task_id_var)
        .with_panel(stat_2xx)
        .with_panel(stat_4xx)
        .with_panel(stat_401_403)
        .with_panel(stat_5xx)
        .with_panel(task_state)
        .with_panel(outbox_state)
        .with_panel(review_gap)
        .with_panel(http_status_breakdown)
        .with_panel(outbox_over_time)
        .with_panel(failed_outbox)
        .with_panel(findings)
        .with_panel(selected_task)
        .with_panel(selected_outbox)
        .with_panel(selected_comments)
        .with_panel(selected_findings)
        .with_panel(posted_inline_comments)
    )
