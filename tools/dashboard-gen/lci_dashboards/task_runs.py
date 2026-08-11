"""Task Runs dashboard — forwards the web UI's runs list + detail, with a Loki drill-down.

Variables let an operator filter by status/repo (mirroring the list filters) and paste a task id to
pull that run's structured logs from Loki (mirroring the detail page).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import dashboard, logs, table

from .common import CRI_UNWRAP, LOKI, POSTGRES, Layout, logql, sql

UID = "lci-task-runs"

# The review/index Job runs in its OWN ephemeral pod per task, named `lightbridge-agent-<task.id>`
# (services/control-plane/src/integrations/k8s.rs:344) plus Kubernetes' own Job-controller suffix —
# so `pod=~"lightbridge-agent-$task_id-.*"` selects exactly that run's stdout+stderr with no JSON
# parsing needed. This carries BOTH the native agent-runner tracing (stdout) and the OpenCode
# logger plugin's per-turn reasoning/content/tool lines (stderr) — the whole point of the ADR-0102
# embed. Verified live against Loki (`namespace`/`pod` are real labels here; a prior `{app=~"..."}`
# stream selector referenced a label THIS LOKI DOES NOT HAVE and silently matched zero streams —
# see the Loki `/loki/api/v1/labels` response for the actual label set before changing this again).
#
# The control plane's own task_id-tagged lines ("created review task", "dispatched task to a Job")
# live in SEPARATE long-lived pods, added as a second target below so one panel covers both the
# dispatch side and the run's own execution logs (#483).
#
# Alloy maps `app.kubernetes.io/name` -> `service_name`, and every long-lived `lightbridge-ci-*` pod
# carries that one value in a container named `main`. Selecting on it rather than on
# `pod=~"lightbridge-ci-control-plane-.*"` is deliberate: a task's lifecycle spans several of these
# pods, and a real run's lines came from the control plane AND the dispatcher.
_CONTROL_PLANE_SELECTOR = '{namespace="converse", service_name="lightbridge-ci", container="main"}'

# `|= "${task_id}"` runs before any parser — cheap narrowing over a large stream, and it keeps the
# non-JSON pods sharing this `service_name` (neo4j) from ever reaching `| json`.
#
# `fields_task_id`, not `task_id`: the subscriber is `.json()` with no `.flatten_event(true)`, so
# event fields nest under `fields` and `| json` flattens them with an underscore.
#
# Both `fields_task_id` filters are load-bearing. `$task_id` defaults to empty when an operator
# opens the dashboard directly rather than through the run-detail embed. On an empty value `|= ""`
# matches every line, and `fields_task_id = ""` matches every line whose `fields_task_id` was never
# extracted, because LogQL treats an absent label as equal to "" under `=` — the equality alone
# firehoses the whole stream under the title "Logs for ". `!= ""` keeps only lines that carry a task
# id; the equality then finds none with an empty one. Measured: empty `$task_id` returns 500+ lines
# with the equality alone and 0 with both, while a real task id returns the same 103 either way.
_CONTROL_PLANE_LOGS = (
    f"{_CONTROL_PLANE_SELECTOR} "
    '|= "${task_id}" '
    f"{CRI_UNWRAP} "
    '| json | __error__="" '
    '| fields_task_id != "" '
    '| fields_task_id = "${task_id}"'
)


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
        .with_target(logql('{namespace="lightbridge-agents", pod=~"lightbridge-agent-${task_id}-.*"}'))
        .with_target(logql(_CONTROL_PLANE_LOGS, ref_id="B"))
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
        .with_panel(runs)
        .with_panel(run_logs)
    )
