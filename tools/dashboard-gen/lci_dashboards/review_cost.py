"""Review Cost dashboard — what each AI review costs, by task and by model.

Lets admins estimate how expensive a single AI review is: per-task time taken,
model(s) used, tokens, and an estimated cost; plus by-model averages/p95 so you
can answer "a review on <model> typically costs ~$X".

Data: `agent_transcript` (per-turn `prompt_tokens` / `completion_tokens` / `model`,
populated on assistant turns only — tool rows are NULL) joined to `tasks`
(timing, repo, kind). LCI records TOKENS, not cost, so cost is computed here from
a per-model price map.

⚠️ Pricing is an ESTIMATE, not the authoritative billed amount:
  - Prices are $/1M tokens from ai-helm `charts/ai-models/values.yaml` (the
    gateway's `pricing.standard`), mirrored in `_PRICE_IN` / `_PRICE_OUT` below.
    Keep them in sync when the gateway pricing changes.
  - LCI has no cached-vs-uncached prompt-token split, so this uses the STANDARD
    input price (no cache discount). The gateway applies a much lower
    `cachedInputPer1M` on cache hits, so this figure is an UPPER BOUND — the real
    billed cost (visible on the AI-Gateway cost dashboards) is typically lower.
  - `reasoning_tokens` is a subset of `completion_tokens` (NOT additive), so it's
    already covered by the output price — do not add it separately.
  - Unknown/older models (NULL model, pre-migration-0017) price at $0 and surface
    in the by-model table as `unknown` so the gap is visible, not hidden.

Edit this generator, then `python tools/dashboard-gen/generate.py` and commit the
regenerated `deploy/observability/dashboards/review-cost.json` (CI diffs it).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import (
    bargauge,
    dashboard,
    heatmap,
    stat,
    table,
    timeseries,
)
from grafana_foundation_sdk.models.heatmap import HeatmapColorMode, HeatmapColorScale

from .common import POSTGRES, Layout, sql

UID = "lci-review-cost"

# Per-model price, $ per 1,000,000 tokens. Source: ai-helm
# charts/ai-models/values.yaml -> <model>.pricing.standard.{inputPer1M,outputPer1M}.
# adorsys-reviewer      = MiniMax M2.7        ($0.25 in / $1.00 out);
# adorsys-reviewer-pro  = GLM-5.2             ($0.95 in / $3.00 out);
# gemini-3p1-flash-lite = Gemini 3.1 Flash Lite ($0.25 in / $1.50 out).
#
# ⚠️ This map is a HAND-MIRROR of gateway pricing and goes stale the moment the review model churns
# to one not listed here — an unlisted model prices at $0 (ELSE), which is exactly why this dashboard
# showed no cost after both tiers moved to gemini-3p1-flash-lite. Keep it in sync when the model
# changes, OR read the authoritative billed cost from the AI-Gateway (eaig) instead: the gateway
# meters + prices every request (`llm_custom_total_cost`) and — once ai-helm#582 logs the
# `code_intelligence_repo`/`target` attribution headers — that cost is sliceable per repo/PR in Loki,
# with no price table to maintain. This SQL estimate is an upper bound (standard input price, no cache
# discount); prefer the gateway figure for real spend.
_PRICE_IN = (
    "CASE coalesce(tr.model, 'unknown') "
    "WHEN 'adorsys-reviewer' THEN 0.25 "
    "WHEN 'adorsys-reviewer-pro' THEN 0.95 "
    "WHEN 'gemini-3p1-flash-lite' THEN 0.25 "
    "ELSE 0 END"
)
_PRICE_OUT = (
    "CASE coalesce(tr.model, 'unknown') "
    "WHEN 'adorsys-reviewer' THEN 1.00 "
    "WHEN 'adorsys-reviewer-pro' THEN 3.00 "
    "WHEN 'gemini-3p1-flash-lite' THEN 1.50 "
    "ELSE 0 END"
)

# One priced row per assistant turn: task_id, model, tokens, and estimated $ cost.
# Tool rows (role='tool') carry no tokens/model, so we restrict to assistant turns.
_PRICED_CTE = f"""priced AS (
  SELECT tr.task_id,
         coalesce(tr.model, 'unknown')           AS model,
         coalesce(tr.prompt_tokens, 0)::bigint    AS prompt_tokens,
         coalesce(tr.completion_tokens, 0)::bigint AS completion_tokens,
         coalesce(tr.prompt_tokens, 0) / 1e6 * ({_PRICE_IN})
       + coalesce(tr.completion_tokens, 0) / 1e6 * ({_PRICE_OUT}) AS cost_usd
  FROM agent_transcript tr
  WHERE tr.role = 'assistant'
)"""

# Reusable filter predicates (Grafana substitutes the ${var} template values).
# repo/kind filter the task; model filters the priced turn.
_F_REPO = "('${repo}' = '' OR (r.owner || '/' || r.name) = '${repo}')"
_F_KIND = "('${kind}' = 'All' OR t.kind = '${kind}')"
_F_MODEL = "('${model}' = 'All' OR p.model = '${model}')"


def _stat(title: str, raw: str, layout: Layout, *, unit: str | None = None) -> stat.Panel:
    panel = stat.Panel().title(title).datasource(POSTGRES).with_target(sql(raw)).grid_pos(
        layout.place(6, 4)
    )
    if unit is not None:
        panel = panel.unit(unit)
    return panel


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    # --- Template variables ---
    repo_var = (
        dashboard.QueryVariable("repo")
        .label("Repository")
        .datasource(POSTGRES)
        .query(
            "SELECT __text, __value FROM ("
            "  SELECT 'All' AS __text, '' AS __value, 0 AS ord "
            "  UNION ALL "
            "  SELECT owner || '/' || name AS __text, owner || '/' || name AS __value, 1 AS ord "
            "  FROM repositories"
            ") t ORDER BY ord, __text"
        )
    )
    model_var = (
        dashboard.QueryVariable("model")
        .label("Model")
        .datasource(POSTGRES)
        .query(
            "SELECT __text, __value FROM ("
            "  SELECT 'All' AS __text, 'All' AS __value, 0 AS ord "
            "  UNION ALL "
            "  SELECT DISTINCT coalesce(model, 'unknown') AS __text, "
            "         coalesce(model, 'unknown') AS __value, 1 AS ord "
            "  FROM agent_transcript WHERE role = 'assistant'"
            ") t ORDER BY ord, __text"
        )
    )
    kind_var = dashboard.CustomVariable("kind").label("Kind").values("All,review,ask")

    # --- Row 1: headline KPIs over the selected range ---
    base_from = (
        "FROM priced p "
        "JOIN tasks t ON t.id = p.task_id "
        "LEFT JOIN repositories r ON r.id = t.repository_id "
        f"WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL}"
    )

    total_cost = _stat(
        "Total cost (range)",
        f"WITH {_PRICED_CTE} "
        f"SELECT round(coalesce(sum(p.cost_usd), 0)::numeric, 2) AS cost {base_from}",
        layout,
        unit="currencyUSD",
    )
    reviews = _stat(
        "Reviews (range)",
        f"WITH {_PRICED_CTE} "
        f"SELECT count(DISTINCT p.task_id) AS reviews {base_from}",
        layout,
    )
    avg_cost = _stat(
        "Avg cost / review (range)",
        f"WITH {_PRICED_CTE}, per_task AS ("
        f"  SELECT p.task_id, sum(p.cost_usd) AS cost {base_from} GROUP BY p.task_id"
        ") SELECT round(coalesce(avg(cost), 0)::numeric, 4) AS avg_cost FROM per_task",
        layout,
        unit="currencyUSD",
    )
    total_tokens = _stat(
        "Total tokens (range)",
        f"WITH {_PRICED_CTE} "
        f"SELECT coalesce(sum(p.prompt_tokens + p.completion_tokens), 0) AS tokens {base_from}",
        layout,
    )

    # --- Row 2: by-model estimation (the "how costly per model" answer) ---
    by_model = (
        table.Panel()
        .title("Cost per review, by model (range)")
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"WITH {_PRICED_CTE}, per_task AS ("
                "  SELECT p.task_id, p.model, "
                "         sum(p.prompt_tokens + p.completion_tokens) AS tokens, "
                "         sum(p.cost_usd) AS cost "
                "  FROM priced p JOIN tasks t ON t.id = p.task_id "
                "  LEFT JOIN repositories r ON r.id = t.repository_id "
                f"  WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "  GROUP BY p.task_id, p.model"
                ") SELECT model AS \"model\", "
                "count(*) AS \"review-runs\", "
                "round(avg(cost)::numeric, 4) AS \"avg $/review\", "
                "round((percentile_cont(0.5) WITHIN GROUP (ORDER BY cost))::numeric, 4) AS \"p50 $\", "
                "round((percentile_cont(0.95) WITHIN GROUP (ORDER BY cost))::numeric, 4) AS \"p95 $\", "
                "round(avg(tokens)::numeric, 0) AS \"avg tokens\", "
                "round(sum(cost)::numeric, 2) AS \"total $\" "
                "FROM per_task GROUP BY model ORDER BY \"total $\" DESC"
            )
        )
        .grid_pos(layout.place(12, 8))
    )
    avg_by_model = (
        bargauge.Panel()
        .title("Avg cost per review by model (range)")
        .datasource(POSTGRES)
        .unit("currencyUSD")
        .with_target(
            sql(
                f"WITH {_PRICED_CTE}, per_task AS ("
                "  SELECT p.task_id, p.model, sum(p.cost_usd) AS cost "
                "  FROM priced p JOIN tasks t ON t.id = p.task_id "
                "  LEFT JOIN repositories r ON r.id = t.repository_id "
                f"  WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "  GROUP BY p.task_id, p.model"
                ") SELECT model AS metric, round(avg(cost)::numeric, 4) AS value "
                "FROM per_task GROUP BY model ORDER BY value DESC"
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # --- Row 3: spend trends ---
    cost_per_day = (
        timeseries.Panel()
        .title("Estimated review cost per day")
        .datasource(POSTGRES)
        .unit("currencyUSD")
        .with_target(
            sql(
                f"WITH {_PRICED_CTE} "
                "SELECT $__timeGroupAlias(t.created_at, $__interval), "
                "round(sum(p.cost_usd)::numeric, 4) AS \"cost\" "
                "FROM priced p JOIN tasks t ON t.id = p.task_id "
                "LEFT JOIN repositories r ON r.id = t.repository_id "
                f"WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "GROUP BY 1 ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(12, 8))
    )
    cost_per_day_by_model = (
        timeseries.Panel()
        .title("Estimated review cost per day, by model")
        .datasource(POSTGRES)
        .unit("currencyUSD")
        .with_target(
            sql(
                f"WITH {_PRICED_CTE} "
                "SELECT $__timeGroupAlias(t.created_at, $__interval), p.model AS \"model\", "
                "round(sum(p.cost_usd)::numeric, 4) AS \"cost\" "
                "FROM priced p JOIN tasks t ON t.id = p.task_id "
                "LEFT JOIN repositories r ON r.id = t.repository_id "
                f"WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "GROUP BY 1, 2 ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # --- Cost-per-review distribution over time ---
    # One (time, cost) point per review; `calculate(True)` lets Grafana bucket the
    # cost on the Y axis and count reviews per (day, cost-bucket) cell — so the
    # SPREAD and outliers show up (e.g. most reviews cheap, a band of expensive
    # ones), which the daily-total line and the p50/p95 table can't convey.
    # Exponential color so rare expensive-review cells stay visible.
    cost_heatmap = (
        heatmap.Panel()
        .title("Per-review cost distribution over time")
        .datasource(POSTGRES)
        .calculate(True)
        .color(
            heatmap.HeatmapColorOptions()
            .mode(HeatmapColorMode.SCHEME)
            .scheme("Oranges")
            .scale(HeatmapColorScale.EXPONENTIAL)
            .steps(64)
        )
        .y_axis(heatmap.YAxisConfig().unit("currencyUSD"))
        .with_target(
            sql(
                f"WITH {_PRICED_CTE}, per_task AS ("
                "  SELECT t.id, t.created_at, sum(p.cost_usd) AS cost "
                "  FROM priced p JOIN tasks t ON t.id = p.task_id "
                "  LEFT JOIN repositories r ON r.id = t.repository_id "
                f"  WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "  GROUP BY t.id, t.created_at"
                ") SELECT created_at AS \"time\", cost FROM per_task ORDER BY 1",
                fmt="time_series",
            )
        )
        .grid_pos(layout.place(24, 8))
    )

    # --- Row 4: the per-review-task table (one row per review × prices) ---
    per_task_table = (
        table.Panel()
        .title("Review tasks × cost (range)")
        .datasource(POSTGRES)
        .with_target(
            sql(
                f"WITH {_PRICED_CTE} "
                "SELECT t.created_at AS \"created\", "
                "coalesce(r.owner || '/' || r.name, t.repository_id::text) AS \"repository\", "
                "t.target_type AS \"target\", t.target_id AS \"number\", "
                "t.kind AS \"kind\", t.status AS \"status\", "
                "round(extract(epoch FROM (t.completed_at - t.started_at))::numeric, 1) "
                "  AS \"duration_s\", "
                "string_agg(DISTINCT p.model, ', ') AS \"model\", "
                "sum(p.prompt_tokens) AS \"prompt_tokens\", "
                "sum(p.completion_tokens) AS \"completion_tokens\", "
                "sum(p.prompt_tokens + p.completion_tokens) AS \"total_tokens\", "
                "round(sum(p.cost_usd)::numeric, 4) AS \"cost_usd\" "
                "FROM tasks t "
                "JOIN priced p ON p.task_id = t.id "
                "LEFT JOIN repositories r ON r.id = t.repository_id "
                f"WHERE $__timeFilter(t.created_at) AND {_F_REPO} AND {_F_KIND} AND {_F_MODEL} "
                "GROUP BY t.id, r.owner, r.name, t.repository_id, t.created_at, "
                "t.target_type, t.target_id, t.kind, t.status, t.completed_at, t.started_at "
                "ORDER BY t.created_at DESC, t.id DESC LIMIT 500"
            )
        )
        .grid_pos(layout.place(24, 12))
    )

    return (
        dashboard.Dashboard("Lightbridge — Review Cost")
        .uid(UID)
        .tags(["lightbridge", "generated", "cost"])
        .refresh("30s")
        .time("now-30d", "now")
        .with_variable(repo_var)
        .with_variable(model_var)
        .with_variable(kind_var)
        .with_panel(total_cost)
        .with_panel(reviews)
        .with_panel(avg_cost)
        .with_panel(total_tokens)
        .with_panel(by_model)
        .with_panel(avg_by_model)
        .with_panel(cost_per_day)
        .with_panel(cost_per_day_by_model)
        .with_panel(cost_heatmap)
        .with_panel(per_task_table)
    )
