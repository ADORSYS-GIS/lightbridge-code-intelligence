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
acceptable, and the detail panels additionally guard on a non-empty `${task_id}` so an unset
selection yields no rows rather than a cast/decode error. `task_id` is a Postgres QUERY variable — a
dropdown of recent review runs (id -> a human label of repo #target · tier · time), refreshed on
time-range change — so the operator picks a run instead of pasting a raw uuid. Because the value is
now DB-constrained (it can only be an id that the variable query itself emitted), the injection
surface is effectively removed; the detail panels nonetheless keep interpolating via Grafana's
`:sqlstring` format (a single-quoted, SQL-escaped literal) as defense-in-depth, and the `<> ''` guard
still short-circuits the zero-runs case where the dropdown is empty.
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import dashboard, table, timeseries
from grafana_foundation_sdk.models.dashboard import DynamicConfigValue, VariableRefresh

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

    # Pick a run to light up the three detail panels; an empty dropdown (no runs in range) renders
    # nothing. QueryVariable + the __value/__text column convention: __value is the raw task id fed
    # to the detail panels, __text is the readable label. Refreshed ON_TIME_RANGE_CHANGED so the
    # list honours the dashboard's time window (the query is $__timeFilter-scoped). A pure-SDK path —
    # the SDK's QueryVariable already exposes datasource/query/refresh, and passes rawSql through
    # verbatim, so no post-processing of the model is needed.
    task_id_var = (
        dashboard.QueryVariable("task_id")
        .label("Task ID (run detail)")
        .datasource(POSTGRES)
        .refresh(VariableRefresh.ON_TIME_RANGE_CHANGED)
        .query(
            "SELECT t.id::text AS \"__value\", "
            "coalesce(r.owner || '/' || r.name, t.repository_id::text) || ' #' || t.target_id "
            "  || ' · ' || t.tier || ' · ' || to_char(t.created_at, 'MM-DD HH24:MI') AS \"__text\" "
            "FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id "
            "WHERE t.run_config_b64 IS NOT NULL AND $__timeFilter(t.created_at) "
            "ORDER BY t.created_at DESC LIMIT 200"
        )
    )

    runs = (
        table.Panel()
        .title("Review runs")
        .description(
            "Per review run: repository, PR/target, tier, status, and the key knobs projected from "
            "the redacted config blob (run_config_b64) plus the tools offered to the model at turn 0 "
            "(run_tools). Indexing runs are excluded (they persist neither). Pick a run from the "
            "Task ID dropdown to inspect it in full below."
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

    # --- Run detail (only when task_id is set). `${task_id:sqlstring}` interpolates the selected
    # dropdown value as a single-quoted, SQL-escaped literal; the `<> ''` guard short-circuits the
    # decode when nothing is selected (empty dropdown), so an unset id returns no rows rather than
    # erroring. The tools/config panels are DECOMPOSED into flat one-row-per-item tables so they read
    # without hovering a collapsed json-view cell; only the ~23KB system_prompt still needs a
    # text-wrapped cell, and the config `value` column gets a wrap override so nested JSON text
    # (arrays/objects rendered by jsonb_each_text) wraps instead of truncating. ---
    _WRAP_CELL = [
        DynamicConfigValue(id_val="custom.cellOptions", value={"type": "auto", "wrapText": True})
    ]

    # One row per config setting: `jsonb_each_text` flattens the redacted config (minus the giant
    # system_prompt) into (setting, value) pairs; nested values (the `tools` array, `resilience` /
    # `extra` objects) render as compact JSON text in their value cell — readable per-row, and the
    # value column wraps so nothing truncates.
    detail_config = (
        table.Panel()
        .title("Run config for $task_id (system_prompt omitted)")
        .description(
            "The full redacted ReviewConfig for the selected run, one row per setting, WITHOUT the "
            "~23KB system_prompt (see the next panel for that alone). Nested values (tools array, "
            "resilience/extra objects) appear as compact JSON text."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                "WITH cfg AS ("
                "  SELECT ((convert_from(decode(t.run_config_b64, 'base64'), 'UTF8'))::jsonb "
                "    - 'system_prompt') AS c "
                "  FROM tasks t "
                "  WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring} "
                "  AND t.run_config_b64 IS NOT NULL"
                ") "
                "SELECT kv.key AS \"setting\", kv.value AS \"value\" "
                "FROM cfg, jsonb_each_text(cfg.c) kv "
                "ORDER BY kv.key"
            )
        )
        .override_by_name("value", _WRAP_CELL)
        .grid_pos(layout.place(12, 12))
    )

    # One row per offered tool: (tool, source). Default table cells are perfect for these scalar
    # columns, so no cell override is needed.
    detail_tools = (
        table.Panel()
        .title("Offered tools for $task_id")
        .description(
            "The exact tool set offered to the model at turn 0 for the selected run — the per-tier "
            "allowlist resolved with any MCP-discovered external-knowledge tools. One row per tool: "
            "name and source (builtin|mcp)."
        )
        .datasource(POSTGRES)
        .with_target(
            sql(
                "SELECT e->>'name' AS \"tool\", e->>'source' AS \"source\" "
                "FROM tasks t, jsonb_array_elements(t.run_tools) e "
                "WHERE ${task_id:sqlstring} <> '' AND t.id::text = ${task_id:sqlstring} "
                "AND t.run_tools IS NOT NULL "
                "ORDER BY e->>'name'"
            )
        )
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
