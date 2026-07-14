//! Resolving the tool surface offered to the model for one run: the diff-presence gate, the per-tier
//! allowlist (ADR-0062), and dynamically-discovered external-knowledge MCP tools (ADR-0066); plus the
//! wind-down narrowing used for the run-start telemetry snapshot.

use std::collections::HashSet;

use lci_agent_clients::ControlPlaneClient;
use lci_agent_types::ToolSpec;
use lci_review_agent::tools::{
    ABORT, ADD_COMMENT, ADD_REVIEW_COMMENT, FINISH, RETRACT_FINDING, tool_defs,
};
use uuid::Uuid;

use crate::bootstrap::config::{McpToolPattern, ReviewConfig, ReviewToolSelector};

/// Resolve the tools offered to the model for one run: built-in tools filtered by the diff gate + the
/// per-tier allowlist, plus any discovered external-knowledge MCP tools that match it (ADR-0066).
/// Returns `(offered, dispatch_discovered)` — `offered` is the full surface (for the turn-0 `TurnFilter`
/// and the telemetry snapshot); `dispatch_discovered` is just the discovered subset the tool registry
/// needs to know how to dispatch. A discovery failure degrades to "no external tools" (non-fatal).
pub(crate) async fn resolve_offered_tools(
    review: &ReviewConfig,
    diff_present: bool,
    client: &ControlPlaneClient,
    task_id: Uuid,
) -> (Vec<ToolSpec>, Vec<ToolSpec>) {
    // Without a diff an inline finding has no line to anchor to, so `add_review_comment` isn't offered.
    let mut offered = tool_defs();
    if !diff_present {
        offered.retain(|spec| spec.function.name != ADD_REVIEW_COMMENT);
    }
    // Per-tier tool allowlist (ADR-0062): its BUILT-IN entries are the authoritative offered set.
    if let Some(allow) = review.tools.as_ref() {
        let builtins: HashSet<&str> = allow
            .iter()
            .filter_map(|selector| match selector {
                ReviewToolSelector::Builtin(builtin) => Some(builtin.as_str()),
                ReviewToolSelector::Mcp(_) => None,
            })
            .collect();
        offered.retain(|spec| builtins.contains(spec.function.name.as_str()));
    }
    // External-knowledge MCP tools (ADR-0066): discovered dynamically. An UNSET allowlist offers ALL
    // discovered; a SET allowlist offers a discovered tool iff some `mcp__` selector matches, and skips
    // discovery entirely when it has none. A discovery failure degrades to "no external tools".
    let mcp_selectors: Option<Vec<&McpToolPattern>> = review.tools.as_ref().map(|allow| {
        allow
            .iter()
            .filter_map(|selector| match selector {
                ReviewToolSelector::Mcp(pattern) => Some(pattern),
                ReviewToolSelector::Builtin(_) => None,
            })
            .collect()
    });
    let discover = match &mcp_selectors {
        None => true,
        Some(selectors) => !selectors.is_empty(),
    };
    let mut dispatch_discovered: Vec<ToolSpec> = Vec::new();
    if discover {
        match client.list_knowledge_tools(task_id).await {
            Ok(discovered) => {
                let matched: Vec<_> = discovered
                    .into_iter()
                    .filter(|tool| match &mcp_selectors {
                        None => true,
                        Some(selectors) => {
                            selectors.iter().any(|pattern| pattern.is_match(&tool.name))
                        }
                    })
                    .collect();
                if !matched.is_empty() {
                    tracing::info!(task_id = %task_id, count = matched.len(), "offering discovered external-knowledge tools");
                    let specs: Vec<ToolSpec> = matched
                        .into_iter()
                        .map(|tool| {
                            ToolSpec::function(tool.name, tool.description, tool.input_schema)
                        })
                        .collect();
                    dispatch_discovered.extend(specs.iter().cloned());
                    offered.extend(specs);
                }
            }
            Err(error) => {
                tracing::warn!(%error, task_id = %task_id, "knowledge-tool discovery failed; continuing without external-knowledge tools");
            }
        }
    }
    (offered, dispatch_discovered)
}

/// The reduced tool set offered once a run enters wind-down (#137), used ONLY for the run-start
/// telemetry snapshot of a FAST run: the write tools + `finish`/`abort`, dropping retrieval/read_file.
/// `add_review_comment`/`retract_finding` are kept only when a diff is present (an inline tool can't
/// anchor without one). Mirrors the engine's convergence narrowing.
pub(crate) fn winddown_tool_defs(base: &[ToolSpec], diff_present: bool) -> Vec<ToolSpec> {
    base.iter()
        .filter(|spec| match spec.function.name.as_str() {
            ADD_REVIEW_COMMENT | RETRACT_FINDING => diff_present,
            ADD_COMMENT | FINISH | ABORT => true,
            _ => false,
        })
        .cloned()
        .collect()
}

/// The tool set turn 0 will ACTUALLY offer for the telemetry snapshot: a FAST run WITHOUT an explicit
/// allowlist runs every turn on the wind-down write/finish set (the `FastTierGuard` narrows to it), so
/// snapshotting the full surface there would record retrieval/read_file/MCP tools the model never gets.
pub(crate) fn run_start_tool_defs<'a>(
    review: &ReviewConfig,
    defs: &'a [ToolSpec],
    winddown_defs: &'a [ToolSpec],
) -> &'a [ToolSpec] {
    if review.fast && review.tools.is_none() {
        winddown_defs
    } else {
        defs
    }
}
