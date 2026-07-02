"""Review-runs dashboard — per-run audit of what each review run actually got.

Reads the run-level telemetry persisted at run start (migration 0023, PR #270): `tasks.run_tools`
(the tool set OFFERED to the model at turn 0 — a JSON array of `{name, source: builtin|mcp}`) and
`tasks.run_config_b64` (base64 of the secret-redacted resolved `ReviewConfig` JSON, including the
verbatim ~23KB `system_prompt`). Both are NULL for indexing runs, so every query filters on
`run_config_b64 IS NOT NULL` to keep to review runs only.

The config projections mirror `ReviewConfig::redacted_json()` in
services/agent-runner/src/bootstrap/config.rs — that method's field names are the contract, not a
guess. Projected knobs: `model`, `tier`, `temperature`, `top_p`, `max_tokens`, `max_turns`,
`context_window`, `stream`.

`decode(..., 'base64')` throws on malformed input; every row here is runner-written so that is
acceptable, and the detail panels additionally guard on a non-empty `${task_id}` so an unset textbox
yields no rows rather than a cast/decode error. The detail panels interpolate the textbox via
Grafana's `:sqlstring` format (a single-quoted, SQL-escaped literal) — deliberately stricter than
the house `'${var}'` pattern, because this variable is FREE TEXT while the dropdown variables on the
other dashboards are enum-constrained.
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import dashboard, table, timeseries
from grafana_foundation_sdk.models.dashboard import DynamicConfigValue

from .common import POSTGRES, Layout, sql

UID = "lci-review-runs"

# Decode the base64 config blob to jsonb ONCE per row. MATERIALIZED is load-bearing: PG >= 12 inlines
# a plain single-reference CTE into the outer query, which re-expands `cfg` into one
# convert_from(decode(...)) call PER PROJECTED COLUMN — seven decodes per row instead of one, for
# every row in the time window before the LIMIT sort. MATERIALIZED forces the CTE to be computed
# first so the decode really does run once per row. Grafana passes rawSql through verbatim, so the
# keyword is safe. `cfg` is the decoded object; the giant `system_prompt` is only pulled in the
# detail panels.
_RUNS_CTE = (
    "WITH runs AS MATERIALIZED ("
    "  SELECT t.id, t.created_at, t.repository_id, t.target_type, t.target_id, "
    "         t.tier, t.status, t.run_tools, "
    "         (convert_from(decode(t.run_config_b64, 'base64'), 'UTF8'))::jsonb AS cfg "
    "  FROM tasks t "
    "  WHERE t.run_config_b64 IS NOT NULL AND $__timeFilter(t.created_at)"
    ")"
)


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    # Paste a task id to light up the three detail panels; empty (the default) renders nothing.
    task_id_var = (
        dashboard.TextBoxVariable("task_id").label("Task ID (run detail)").default_value("")
    )

    runs = (
        table.Panel()
        .title("Review runs")
        .description(
            "Per review run: repository, PR/target, tier, status, and the key knobs projected from "
            "the redacted config blob (run_config_b64) plus the tools offered to the model at turn 0 "
            "(run_tools). Indexing runs are excluded (they persist neither). Paste a task id into the "
            "Task ID variable to inspect one run in full below."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"{_RUNS_CTE} "
                "SELECT runs.created_at AS \"created\", "
                "coalesce(r.owner || '/' || r.name, runs.repository_id::text) AS \"repository\", "
                "runs.target_type AS \"target\", runs.target_id AS \"number\", "
                "runs.tier, runs.status, "
                "runs.cfg->>'model' AS \"model\", "
                "runs.cfg->>'temperature' AS \"temp\", "
                "runs.cfg->>'top_p' AS \"top_p\", "
                "runs.cfg->>'max_tokens' AS \"max_tokens\", "
                "runs.cfg->>'max_turns' AS \"max_turns\", "
                "runs.cfg->>'context_window' AS \"ctx_window\", "
                "runs.cfg->>'stream' AS \"stream\", "
                "coalesce(jsonb_array_length(runs.run_tools), 0) AS \"offered\", "
                "(SELECT string_agg(e->>'name', ', ' ORDER BY e->>'name') "
                " FROM jsonb_array_elements(runs.run_tools) e) AS \"offered tools\", "
                "runs.id AS \"task id\" "
                "FROM runs LEFT JOIN repositories r ON r.id = runs.repository_id "
                # id DESC tie-breaks same-timestamp rows so the ordering is deterministic.
                "ORDER BY runs.created_at DESC, runs.id DESC LIMIT 200"
            )
        )
        .grid_pos(layout.place(24, 12))
    )

    # runs-per-tier over time — cheap, reuses the same decode-free filter (tier is a plain column).
    runs_by_tier = (
        timeseries.Panel()
        .title("Review runs by tier")
        .description("Review runs started per interval, split by tier (fast / deep).")
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT $__timeGroupAlias(t.created_at, $__interval), "
                "t.tier AS \"tier\", count(*) AS \"runs\" "
                "FROM tasks t "
                "WHERE t.run_config_b64 IS NOT NULL AND $__timeFilter(t.created_at) "
                "GROUP BY 1, 2 ORDER BY 1, 2",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(24, 7))
    )

    # --- Run detail (only when task_id is set). `${task_id:sqlstring}` interpolates as a
    # single-quoted, SQL-escaped literal (free-text textbox, so escaping matters); the `<> ''` guard
    # short-circuits the decode when the textbox is empty, so an unset/blank id returns no rows
    # rather than erroring. Cell overrides matter here: default table cells collapse multi-line
    # content to one truncated line, so jsonb_pretty output gets a json-view cell and the ~23KB
    # prompt a text-wrapped cell — without them the payload is only reachable via Inspect → Data. ---
    _JSON_CELL = [DynamicConfigValue(id_val="custom.cellOptions", value={"type": "json-view"})]
    _WRAP_CELL = [
        DynamicConfigValue(id_val="custom.cellOptions", value={"type": "auto", "wrapText": True})
    ]

    detail_config = (
        table.Panel()
        .title("Run config for $task_id (system_prompt omitted)")
        .description(
            "The full redacted ReviewConfig for the pasted task, pretty-printed WITHOUT the ~23KB "
            "system_prompt (see the next panel for that alone)."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT jsonb_pretty("
                "  (convert_from(decode(t.run_config_b64, 'base64'), 'UTF8'))::jsonb "
                "  - 'system_prompt') AS \"config (no system_prompt)\" "
                "FROM tasks t "
                "WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring} "
                "AND t.run_config_b64 IS NOT NULL"
            )
        )
        .override_by_name("config (no system_prompt)", _JSON_CELL)
        .grid_pos(layout.place(12, 12))
    )

    detail_tools = (
        table.Panel()
        .title("Offered tools for $task_id")
        .description(
            "The exact tool set offered to the model at turn 0 for the pasted task — the per-tier "
            "allowlist resolved with any MCP-discovered external-knowledge tools. Pretty-printed "
            "array of {name, source: builtin|mcp}."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT jsonb_pretty(t.run_tools) AS \"offered tools\" "
                "FROM tasks t "
                "WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring} "
                "AND t.run_tools IS NOT NULL"
            )
        )
        .override_by_name("offered tools", _JSON_CELL)
        .grid_pos(layout.place(12, 12))
    )

    detail_prompt = (
        table.Panel()
        .title("System prompt for $task_id")
        .description(
            "The verbatim reviewer system_prompt this run was configured with — the exact ~23KB of "
            "guidance the model received, decoded from the redacted config blob."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT ((convert_from(decode(t.run_config_b64, 'base64'), 'UTF8'))::jsonb "
                "  ->>'system_prompt') AS \"system_prompt\" "
                "FROM tasks t "
                "WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring} "
                "AND t.run_config_b64 IS NOT NULL"
            )
        )
        .override_by_name("system_prompt", _WRAP_CELL)
        .grid_pos(layout.place(24, 14))
    )

    return (
        dashboard.Dashboard("Lightbridge — Review runs")
        .uid(UID)
        .tags(["lightbridge", "generated"])
        .refresh("1m")
        .time("now-7d", "now")
        .description(
            "Per-run audit of the review agent: for each review run, the tools offered to the model "
            "and the resolved (redacted) config it ran with. Sourced from tasks.run_tools / "
            "tasks.run_config_b64 (migration 0023)."
        )
        .with_variable(task_id_var)
        .with_panel(runs)
        .with_panel(runs_by_tier)
        .with_panel(detail_config)
        .with_panel(detail_tools)
        .with_panel(detail_prompt)
    )
