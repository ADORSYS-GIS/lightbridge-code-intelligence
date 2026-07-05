"""Review Cost dashboard — what each AI review costs, straight from the gateway.

Cost comes from the **AI-Gateway (eaig) itself**, not an app-side estimate. The
agent-runner calls the gateway for every review LLM turn; the gateway prices each
call authoritatively (`llm_custom_total_cost`, computed from the per-model pricing
declared in ai-helm `charts/ai-models/values.yaml` — all token types: input,
cached-input, output) and logs it. Alloy ships those access logs to Loki, where
this dashboard aggregates them.

Why not price in the dashboard (the old approach): a hardcoded per-model price map
duplicated the gateway's pricing and silently returned $0 the moment the review
model churned to one not in the map (which is exactly what happened when both tiers
moved to gemini-3p1-flash-lite). Reading the gateway's own figure means **no price
table to maintain and price changes are reflected automatically** — the number is
whatever the gateway actually billed.

Data — the `envoy-ai-gateway` Loki stream, `| json`-parsed (dotted keys → underscores):
  - `gen_ai_usage_custom_total_cost` — billed cost in **micro-USD** (÷1e6 for USD).
  - `gen_ai_usage_total_tokens`      — total tokens for the call.
  - `account_id`      — the repo (`owner/name`); the Authorino internal AuthConfig
                        maps `x-code-intelligence-repo` → `account_id` for LCI calls.
  - `oidc_jti`        — `runid:<task_id>`, one value per review run.
  - `gen_ai_request_model` — the gateway model alias (e.g. `gemini-3p1-flash-lite`).
`oidc_jti=~"runid:.+"` scopes the stream to LCI review traffic (only our runs carry
a runid), excluding LibreChat/other callers on the shared gateway.

⚠️ Forward-only history, bounded by Loki retention (the gateway didn't always log
these fields). The gateway's own cost-by-model / per-user dashboards (ai-helm
`tools/dashboards/envoy_ai_gateway/`) read the same source at coarser granularity.

Edit this generator, then `python tools/dashboard-gen/generate.py` and commit the
regenerated `deploy/observability/dashboards/review-cost.json` (CI diffs it).
"""

from __future__ import annotations

from grafana_foundation_sdk.builders import bargauge, dashboard, stat, table, timeseries

from .common import LOKI, POSTGRES, Layout, logql

UID = "lci-review-cost"

# The LCI-scoped, parsed, filtered gateway stream. `$repo` / `$model` default to `.+`
# (all) via the template vars below, so an unset filter matches every LCI run/model.
_STREAM = (
    '{service_name="envoy-ai-gateway"} | json '
    '| oidc_jti=~"runid:.+" '
    '| account_id=~"$repo" '
    '| gen_ai_request_model=~"$model"'
)


def _unwrap(field: str) -> str:
    """A range-vector unwrapping `field` (drops non-numeric lines via `__error__`)."""
    return f"{_STREAM} | unwrap {field} | __error__=\"\""


# Billed cost (USD) over the panel range: sum the micro-USD field across all matching
# calls, ÷1e6. Cost per interval (for the trend) swaps [$__range] → [$__interval].
_COST_RANGE = f"sum(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__range])) / 1e6"
_COST_INTERVAL = (
    f"sum(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__interval])) / 1e6"
)
# Distinct review runs = distinct `oidc_jti` with any call in range.
_RUNS_RANGE = (
    f"count(sum by (oidc_jti) (count_over_time({_STREAM} [$__range])))"
)
_TOKENS_RANGE = f"sum(sum_over_time({_unwrap('gen_ai_usage_total_tokens')} [$__range]))"


def _stat(title: str, expr: str, layout: Layout, *, unit: str | None = None) -> stat.Panel:
    panel = (
        stat.Panel()
        .title(title)
        .datasource(LOKI)
        .with_target(logql(expr))
        .grid_pos(layout.place(6, 4))
    )
    if unit is not None:
        panel = panel.unit(unit)
    return panel


def dashboard_builder() -> dashboard.Dashboard:
    layout = Layout()

    # --- Template variables ---
    # repo/model still enumerate from Postgres (the app knows its repos + the models
    # it ran); the VALUES are used as Loki label filters. "All" resolves to the regex
    # `.+` so `account_id=~"$repo"` / `gen_ai_request_model=~"$model"` match every LCI
    # run rather than an empty string. There is no "kind" (review/ask) filter here —
    # the gateway log carries no task-kind; this board is review-traffic cost.
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
        .datasource(POSTGRES)
        .query(
            "SELECT __text, __value FROM ("
            "  SELECT 'All' AS __text, '.+' AS __value, 0 AS ord "
            "  UNION ALL "
            "  SELECT DISTINCT coalesce(model, 'unknown') AS __text, "
            "         coalesce(model, 'unknown') AS __value, 1 AS ord "
            "  FROM agent_transcript WHERE role = 'assistant' AND model IS NOT NULL"
            ") t ORDER BY ord, __text"
        )
    )

    # --- Row 1: headline KPIs over the selected range (all from the gateway logs) ---
    total_cost = _stat("Billed cost (range)", _COST_RANGE, layout, unit="currencyUSD")
    reviews = _stat("Reviews (range)", _RUNS_RANGE, layout)
    avg_cost = _stat(
        "Avg cost / review (range)",
        f"({_COST_RANGE}) / clamp_min({_RUNS_RANGE}, 1)",
        layout,
        unit="currencyUSD",
    )
    total_tokens = _stat("Total tokens (range)", _TOKENS_RANGE, layout)

    # --- Row 2: by-model + by-repo breakdown ---
    by_model = (
        table.Panel()
        .title("Billed cost by model (range)")
        .datasource(LOKI)
        .unit("currencyUSD")
        .with_target(
            logql(
                "sum by (gen_ai_request_model) "
                f"(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__range])) / 1e6"
            )
        )
        .grid_pos(layout.place(12, 8))
    )
    by_repo = (
        bargauge.Panel()
        .title("Billed cost by repository (range)")
        .datasource(LOKI)
        .unit("currencyUSD")
        .with_target(
            logql(
                "sum by (account_id) "
                f"(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__range])) / 1e6"
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # --- Row 3: spend trends ---
    cost_per_day = (
        timeseries.Panel()
        .title("Billed cost per interval")
        .datasource(LOKI)
        .unit("currencyUSD")
        .with_target(logql(_COST_INTERVAL))
        .grid_pos(layout.place(12, 8))
    )
    cost_per_day_by_model = (
        timeseries.Panel()
        .title("Billed cost per interval, by model")
        .datasource(LOKI)
        .unit("currencyUSD")
        .with_target(
            logql(
                "sum by (gen_ai_request_model) "
                f"(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__interval])) / 1e6"
            )
        )
        .grid_pos(layout.place(12, 8))
    )

    # --- Row 4: per-run cost (one row per review run × its repo/model) ---
    per_run = (
        table.Panel()
        .title("Review runs × billed cost (range)")
        .datasource(LOKI)
        .unit("currencyUSD")
        .with_target(
            logql(
                "topk(500, sum by (oidc_jti, account_id, gen_ai_request_model) "
                f"(sum_over_time({_unwrap('gen_ai_usage_custom_total_cost')} [$__range])) / 1e6)"
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
        .with_panel(total_cost)
        .with_panel(reviews)
        .with_panel(avg_cost)
        .with_panel(total_tokens)
        .with_panel(by_model)
        .with_panel(by_repo)
        .with_panel(cost_per_day)
        .with_panel(cost_per_day_by_model)
        .with_panel(per_run)
    )
