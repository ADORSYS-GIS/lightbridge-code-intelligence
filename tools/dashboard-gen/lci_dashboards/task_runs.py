"""Task Runs dashboard — forwards the web UI's runs list + detail, with a Loki drill-down.

Variables let an operator filter by status/repo (mirroring the list filters) and paste a task id to
pull that run's structured logs from Loki (mirroring the detail page).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import dashboard, logs, table

from .common import LOKI, POSTGRES, Layout, logql, sql

UID = "lci-task-runs"

# Stream selector is environment-specific; exposed as a textbox so it can be tuned without
# regenerating. `| json` parses the control plane's structured logs so we can filter by task_id.
DEFAULT_STREAM = '{app=~"lightbridge.*"}'


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    status_var = (
        dashboard.CustomVariable("status")
        .label("Status")
        .values("All,queued,running,succeeded,failed,timed_out,cancelled")
    )
    repo_var = (
        dashboard.QueryVariable("repo")
        .label("Repository")
        .datasource(POSTGRES)
        # Empty value == "All"; explicit ord weight guarantees the sentinel is always first even
        # when repository names start with a letter before 'A'.
        .query(
            "SELECT __text, __value FROM ("
            "  SELECT 'All' AS __text, '' AS __value, 0 AS ord "
            "  UNION ALL "
            "  SELECT owner || '/' || name AS __text, owner || '/' || name AS __value, 1 AS ord "
            "  FROM repositories"
            ") t ORDER BY ord, __text"
        )
    )
    task_id_var = (
        dashboard.TextBoxVariable("task_id").label("Task ID (for logs)").default_value("")
    )
    stream_var = (
        dashboard.TextBoxVariable("stream").label("Loki stream").default_value(DEFAULT_STREAM)
    )

    runs = (
        table.Panel()
        .title("Task runs")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT t.created_at AS \"created\", "
                "coalesce(r.owner || '/' || r.name, t.repository_id::text) AS \"repository\", "
                "t.target_type AS \"target\", t.target_id AS \"number\", t.command_text AS \"command\", "
                "t.status, t.attempts, t.head_sha, t.started_at, t.completed_at, t.job_name, t.id "
                "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
                "WHERE $__timeFilter(t.created_at) "
                "AND ('${status}' = 'All' OR t.status = '${status}') "
                "AND ('${repo}' = '' OR (r.owner || '/' || r.name) = '${repo}') "
                "ORDER BY t.created_at DESC LIMIT 500"
            )
        )
        .grid_pos(layout.place(24, 13))
    )

    run_logs = (
        logs.Panel()
        # explicit id pinned so the apps/web d-solo embed URL is deterministic (run-logs-embed.tsx).
        # High constant (100) avoids collision with Grafana's auto-assigned ids for the other
        # id-less panels, which start at 1.
        .id(100)
        .title("Logs for $task_id")
        .datasource(LOKI)
        .show_time(True)
        .wrap_log_message(True)
        .with_target(logql('${stream} | json | task_id = `${task_id}`'))
        .grid_pos(layout.place(24, 11))
    )

    return (
        dashboard.Dashboard("Lightbridge — Task Runs")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("30s")
        .time("now-7d", "now")
        .with_variable(status_var)
        .with_variable(repo_var)
        .with_variable(task_id_var)
        .with_variable(stream_var)
        .with_panel(runs)
        .with_panel(run_logs)
    )
