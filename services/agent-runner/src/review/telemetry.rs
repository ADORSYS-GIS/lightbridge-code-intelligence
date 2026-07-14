//! Run-level telemetry (ADR-0034/0062/0066): the run-start "what will turn 0 actually offer" snapshot,
//! and summing the model client's per-turn token telemetry for the live status projection.

use lci_agent_clients::ControlPlaneClient;
use lci_agent_types::ToolSpec;
use lci_review_agent::model::TurnTelemetry;
use lci_review_agent::tools::MCP_TOOL_PREFIX;
use uuid::Uuid;

use super::tool_surface::{run_start_tool_defs, winddown_tool_defs};
use crate::bootstrap::config::ReviewConfig;

/// Record + submit the run-start telemetry (ADR-0034/0062/0066): a snapshot of what turn 0 will
/// ACTUALLY offer. A FAST run without an allowlist runs every turn on the wind-down write/finish set
/// (the `FastTierGuard` narrows to it), so snapshotting the full surface there would claim
/// retrieval/read_file tools the model is never given — [`run_start_tool_defs`] accounts for that.
/// Best-effort: a submission failure is logged and non-fatal.
pub(crate) async fn submit_run_start_telemetry(
    client: &ControlPlaneClient,
    task_id: Uuid,
    review: &ReviewConfig,
    offered: &[ToolSpec],
    diff_present: bool,
) {
    let winddown_defs = winddown_tool_defs(offered, diff_present);
    let start_defs = run_start_tool_defs(review, offered, &winddown_defs);
    let offered_tools_json = serde_json::Value::Array(
        start_defs
            .iter()
            .map(|spec| {
                let source = if spec.function.name.starts_with(MCP_TOOL_PREFIX) {
                    "mcp"
                } else {
                    "builtin"
                };
                serde_json::json!({ "name": spec.function.name, "source": source })
            })
            .collect(),
    );
    let offered_tool_names: Vec<&str> = start_defs
        .iter()
        .map(|spec| spec.function.name.as_str())
        .collect();
    tracing::info!(
        task_id = %task_id,
        tier = if review.fast { "fast" } else { "deep" },
        model = %review.model,
        tool_count = offered_tool_names.len(),
        tools = ?offered_tool_names,
        "review run: offered tools"
    );
    if let Err(error) = client
        .submit_review_telemetry(task_id, &offered_tools_json, &review.redacted_config_b64())
        .await
    {
        tracing::warn!(%error, task_id = %task_id, "submitting review telemetry failed (non-fatal)");
    }
}

/// Sum the running (prompt, completion) token totals across the per-turn telemetry for the live status
/// projection (RFC-0007 slice 5). Tokens are not carried in the transcript events, so they come from the
/// model client's telemetry side-channel. A negative/absent count clamps to `0` — a status metric must
/// never be a nonsense number.
pub(crate) fn sum_usage(telemetry: &[TurnTelemetry]) -> (u64, u64) {
    let clamp = |value: Option<i64>| u64::try_from(value.unwrap_or(0).max(0)).unwrap_or(0);
    telemetry
        .iter()
        .fold((0, 0), |(prompt, completion), entry| {
            (
                prompt + clamp(entry.prompt_tokens),
                completion + clamp(entry.completion_tokens),
            )
        })
}
