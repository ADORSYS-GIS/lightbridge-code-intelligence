//! Prometheus metrics. A single global recorder is installed once; both the `serve` and
//! `dispatcher` roles expose its text rendering at `/metrics` (scraped by Alloy). Instrumentation
//! itself uses the `metrics` facade macros (`counter!`, `histogram!`) sprinkled across the code.

use std::sync::OnceLock;

use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder (idempotent) and return a clonable render handle.
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("install prometheus recorder")
        })
        .clone()
}

// --- Instrumentation helpers (keep metric names in one place; see the Operations dashboard) ---

/// An HTTP request: count by method/route/status, latency by method/route.
///
/// `method` and `status` are `&'static str` (callers use a static match) so label values are
/// zero-allocation. `path` is the matched route template and is allocated once here.
pub fn http_request(method: &'static str, path: &str, status: &'static str, seconds: f64) {
    let path_owned = path.to_string();
    counter!(
        "http_requests_total",
        "method" => method,
        "path" => path_owned.clone(),
        "status" => status,
    )
    .increment(1);
    histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => path_owned,
    )
    .record(seconds);
}

/// An accepted (verified, non-duplicate) webhook delivery, labelled by platform + event type.
/// `platform` is `"github"` or `"gitlab"` (ADR-0072).
pub fn webhook_delivery(platform: &str, event: &str) {
    counter!(
        "lci_webhook_deliveries_total",
        "platform" => platform.to_string(),
        "event" => event.to_string(),
    )
    .increment(1);
}

pub fn webhook_signature_failure(platform: &str) {
    counter!("lci_webhook_signature_failures_total", "platform" => platform.to_string())
        .increment(1);
}

pub fn webhook_duplicate(platform: &str) {
    counter!("lci_webhook_duplicate_deliveries_total", "platform" => platform.to_string())
        .increment(1);
}

pub fn task_created() {
    counter!("lci_tasks_created_total").increment(1);
}

/// An `opened` PR whose automatic fast-tier review was skipped because the author is a bot
/// (RFC-0003, `review.skip_bot_authored_prs`). Distinct from the approval-gate skip.
pub fn review_skipped_bot_author() {
    counter!("lci_review_skipped_bot_author_total").increment(1);
}

/// A call through an external-knowledge MCP tool (ADR-0066): `tool` is the configured MCP server's
/// name (e.g. `brave-search`, `context7`) — bounded cardinality (one label value per configured
/// server), not the individual downstream tool. `outcome` is `ok`, `error` (upstream/network
/// failure), `unknown_tool` (no configured server matches the requested name), or `invalid_request`
/// (a malformed `mcp__<server>__<tool>` name). `tool` is dynamic (config-driven, not a compile-time
/// set of providers) so it's owned here; `outcome` stays `&'static str` (callers pass literals).
pub fn knowledge_tool_call(tool: &str, outcome: &'static str) {
    let tool = tool.to_string();
    counter!("lci_knowledge_tool_calls_total", "tool" => tool, "outcome" => outcome).increment(1);
}

/// A dispatch attempt outcome: `launched` or `failed`. Callers pass string literals so this is
/// zero-allocation.
pub fn dispatch_outcome(outcome: &'static str) {
    counter!("lci_dispatch_jobs_total", "outcome" => outcome).increment(1);
}

/// Seconds spent launching the Kubernetes Job for a claimed task.
pub fn dispatch_launch_seconds(seconds: f64) {
    histogram!("lci_dispatch_launch_seconds").record(seconds);
}

/// A reaper reconciliation outcome for a stuck task (RFC-0001 Phase 2): `renewed`, `succeeded`,
/// `requeued`, `failed`, or `cancelled` (a closed PR's Job stopped). String literals → zero-alloc.
pub fn reap_outcome(outcome: &'static str) {
    counter!("lci_reaper_tasks_total", "outcome" => outcome).increment(1);
}

/// An index-sweeper cycle outcome (RFC-0002 / ADR-0052): `pruned` (a repo had stale snapshots reaped),
/// `clean` (nothing to prune), or `error`. String literals → zero-alloc.
pub fn index_prune_outcome(outcome: &'static str) {
    counter!("lci_index_prune_total", "outcome" => outcome).increment(1);
}

/// Rows reaped by the index sweeper across one cycle: `code_chunks` rows + Neo4j `Symbol` nodes.
pub fn index_prune_deleted(chunks: u64, graph_nodes: u64) {
    counter!("lci_index_prune_chunks_deleted_total").increment(chunks);
    counter!("lci_index_prune_graph_nodes_deleted_total").increment(graph_nodes);
}

/// Terminal `outbox` rows pruned across one sweep (ADR-0059 GC): delivered (`posted`) +
/// dead-lettered (`failed`) rows past their retention window.
pub fn outbox_prune_deleted(posted: u64, failed: u64) {
    counter!("lci_outbox_prune_posted_deleted_total").increment(posted);
    counter!("lci_outbox_prune_failed_deleted_total").increment(failed);
}

/// An outbox delivery outcome (ADR-0059/0071): `posted` (success) or `failed` (retry/dead-letter),
/// labelled by `platform` (`"github"` / `"gitlab"`) and `kind` (`review` / `reply` / `reaction` /
/// `failure_notice`). Bounded cardinality — 2 platforms × 4 kinds × 2 outcomes = 16 series.
pub fn outbox_delivery(platform: &str, kind: &str, outcome: &'static str) {
    counter!(
        "lci_outbox_delivery_total",
        "platform" => platform.to_string(),
        "kind" => kind.to_string(),
        "outcome" => outcome,
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_metrics_appear_in_the_render() {
        let handle = install();
        webhook_delivery("github", "pull_request");
        webhook_delivery("gitlab", "Merge Request Hook");
        webhook_signature_failure("github");
        dispatch_outcome("launched");
        outbox_delivery("gitlab", "review", "posted");
        let rendered = handle.render();
        assert!(rendered.contains("lci_webhook_deliveries_total"));
        assert!(rendered.contains("platform=\"github\""));
        assert!(rendered.contains("platform=\"gitlab\""));
        assert!(rendered.contains("event=\"pull_request\""));
        assert!(rendered.contains("event=\"Merge Request Hook\""));
        assert!(rendered.contains("lci_webhook_signature_failures_total"));
        assert!(rendered.contains("outcome=\"launched\""));
        assert!(rendered.contains("lci_outbox_delivery_total"));
        assert!(rendered.contains("kind=\"review\""));
        assert!(rendered.contains("outcome=\"posted\""));
    }
}
