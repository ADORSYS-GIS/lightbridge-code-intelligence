//! Resolving the tool surface offered to the model for one run: the diff-presence gate, the per-tier
//! allowlist (ADR-0062), and dynamically-discovered external-knowledge MCP tools (ADR-0066); plus the
//! wind-down narrowing used for the run-start telemetry snapshot.

use std::collections::HashSet;

use lci_agent_clients::ControlPlaneClient;
use lci_agent_types::ToolSpec;
#[cfg(test)]
use lci_review_agent::tools::FINISH;
use lci_review_agent::tools::{ADD_REVIEW_COMMENT, RUN_SAST, tool_defs};
use uuid::Uuid;

use crate::bootstrap::config::{McpToolPattern, ReviewConfig, ReviewTool, ReviewToolSelector};

/// Whether an operator EXPLICITLY listed `run_sast` in this tier's `review.tools` allowlist (ADR-0073).
/// `run_sast` is opt-in per tier and never rides the "no allowlist" default surface, so this predicate
/// gates it — on the native surface below AND, reused, on the OpenCode MCP surface
/// ([`crate::review::opencode`]) so both paths honor the same rule.
pub(crate) fn sast_explicitly_listed(review: &ReviewConfig) -> bool {
    review.tools.as_ref().is_some_and(|allow| {
        allow
            .iter()
            .any(|selector| matches!(selector, ReviewToolSelector::Builtin(ReviewTool::RunSast)))
    })
}

/// Whether an operator EXPLICITLY listed a `mcp__github__…` selector in this preset's `review.tools`
/// allowlist (ADR-0105, story #498) — the same "opt-in via allowlist" gate `sast_explicitly_listed`
/// uses, and for the same reason: unlike `sast`, GitHub MCP genuinely spawns a second, freshly
/// credentialed subprocess, so the decision to pay that cost per task must be explicit, never implied
/// by an unset allowlist. Checked against the selector's raw config string (`mcp__github__…`), not a
/// discovered tool name — this predicate decides whether to spawn `github-mcp-server` at all, before
/// any tool discovery could happen.
pub(crate) fn github_mcp_explicitly_listed(review: &ReviewConfig) -> bool {
    review.tools.as_ref().is_some_and(|allow| {
        allow.iter().any(|selector| match selector {
            ReviewToolSelector::Mcp(pattern) => pattern.as_str().starts_with("mcp__github__"),
            ReviewToolSelector::Builtin(_) => false,
        })
    })
}

/// Resolve the tools offered to the model for one run: built-in tools filtered by the diff gate + the
/// per-tier allowlist, plus any discovered external-knowledge MCP tools that match it (ADR-0066).
/// Returns `(offered, dispatch_discovered)` — `offered` is the full surface (for the turn-0 `TurnFilter`
/// and the telemetry snapshot); `dispatch_discovered` is just the discovered subset the tool registry
/// needs to know how to dispatch. A discovery failure degrades to "no external tools" (non-fatal).
pub(crate) async fn resolve_offered_tools(
    review: &ReviewConfig,
    diff_present: bool,
    sast_enabled: bool,
    client: &ControlPlaneClient,
    task_id: Uuid,
) -> (Vec<ToolSpec>, Vec<ToolSpec>) {
    // Without a diff an inline finding has no line to anchor to, so `add_review_comment` isn't offered;
    // `run_sast` has nothing to scope a scan to either. `run_sast` is ALSO dropped whenever SAST itself
    // is off (ADR-0073) — otherwise an operator's allowlist naming it while `sast.enabled=false` would
    // advertise a tool the registry never actually registers (`tool_registry`'s `sast` arg stays `None`),
    // turning a dispatch attempt into an "unknown tool" refusal instead of a clean "not offered."
    //
    // `run_sast` is ALSO, unlike every other built-in, excluded from the "no allowlist" default surface
    // (`review.tools` unset — deep's default is otherwise literally every built-in). ADR-0073 requires an
    // operator to list it EXPLICITLY on both tiers for it to be offered; without this exclusion, shipping
    // this feature would have silently turned SAST on for any deep tier that already had
    // `sast.enabled=true` but no `review.deep.tools` allowlist configured, instead of "silently disabled
    // until the ai-helm-values change lands" as the ADR promises.
    let sast_explicitly_listed = sast_explicitly_listed(review);
    let mut offered = tool_defs();
    if !diff_present {
        offered.retain(|spec| spec.function.name != ADD_REVIEW_COMMENT);
    }
    if !sast_explicitly_listed || !diff_present || !sast_enabled {
        offered.retain(|spec| spec.function.name != RUN_SAST);
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

#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::bootstrap::config::ResilienceConfig;

    /// A minimal deep-tier `ReviewConfig` — only the fields `resolve_offered_tools` reads vary between
    /// tests (`tools`); the rest are placeholder values a real config would never leave at.
    fn review_config(tools: Option<Vec<ReviewToolSelector>>) -> ReviewConfig {
        ReviewConfig {
            base_url: "https://gateway.internal/v1".to_string(),
            api_key: "key".to_string(),
            model: "m".to_string(),
            system_prompt: "You are a reviewer.".to_string(),
            max_diff_chars: 60_000,
            max_turns: 40,
            max_batch_size: 8,
            max_files_read: 30,
            max_searches: 15,
            max_batches: 6,
            max_coverage_bounces: 3,
            max_cycles: 8,
            context_window: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            extra: serde_json::Map::new(),
            stream: false,
            resilience: ResilienceConfig::default(),
            tools,
            opencode_overlay: None,
        }
    }

    fn selectors(names: &[&str]) -> Vec<ReviewToolSelector> {
        serde_json::from_value(serde_json::json!(names)).unwrap()
    }

    // ADR-0105 / story #498: `github_mcp_explicitly_listed` is the same "opt-in via allowlist" gate
    // `sast_explicitly_listed` already proves out — GitHub MCP must never ride an unset or
    // unrelated-mcp allowlist.
    #[test]
    fn github_mcp_explicitly_listed_requires_a_github_selector() {
        assert!(!github_mcp_explicitly_listed(&review_config(None)));
        assert!(!github_mcp_explicitly_listed(&review_config(Some(
            selectors(&["finish", "abort"])
        ))));
        assert!(!github_mcp_explicitly_listed(&review_config(Some(
            selectors(&["mcp__brave-search__brave_web_search"])
        ))));
        assert!(github_mcp_explicitly_listed(&review_config(Some(
            selectors(&["finish", "mcp__github__get_issue"])
        ))));
    }

    async fn mock_no_knowledge_tools() -> MockServer {
        let cp = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v2/internal/tasks/{}/knowledge/tools",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&cp)
            .await;
        cp
    }

    // ADR-0073: `run_sast` must NOT ride along in the "no allowlist" default surface the way every other
    // built-in does — deep's unset-`tools` default is otherwise literally every built-in in `tool_defs()`,
    // and `run_sast` is now unconditionally one of them. Without this exclusion, shipping this feature
    // would have silently turned SAST *on* for any deep tier that already had `sast.enabled=true` but no
    // `review.deep.tools` allowlist, contradicting the ADR's stated "silently disabled until the
    // ai-helm-values change lands" rollout story.
    #[tokio::test]
    async fn run_sast_is_excluded_from_the_unset_allowlist_default_even_when_sast_is_enabled() {
        let cp = mock_no_knowledge_tools().await;
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let review = review_config(None);
        let (offered, _) = resolve_offered_tools(&review, true, true, &client, Uuid::nil()).await;
        assert!(
            !offered.iter().any(|spec| spec.function.name == RUN_SAST),
            "run_sast must not be offered without an explicit allowlist entry"
        );
        // Sanity: the exclusion is `run_sast`-specific, not a blanket "no built-ins" bug — another
        // built-in the default surface DOES include stays present.
        assert!(offered.iter().any(|spec| spec.function.name == FINISH));
    }

    // The explicit opt-in path: `run_sast` in the allowlist + SAST enabled + a diff present offers it.
    #[tokio::test]
    async fn run_sast_is_offered_when_explicitly_allowlisted_with_sast_enabled_and_a_diff() {
        let cp = mock_no_knowledge_tools().await;
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let review = review_config(Some(vec![
            ReviewToolSelector::Builtin(ReviewTool::RunSast),
            ReviewToolSelector::Builtin(ReviewTool::Finish),
            ReviewToolSelector::Builtin(ReviewTool::Abort),
        ]));
        let (offered, _) = resolve_offered_tools(&review, true, true, &client, Uuid::nil()).await;
        assert!(
            offered.iter().any(|spec| spec.function.name == RUN_SAST),
            "run_sast is offered once explicitly allowlisted"
        );
    }

    // Even explicitly allowlisted, `run_sast` needs BOTH a diff to scope to AND SAST actually enabled —
    // otherwise the registry never registers it (see `resolve_offered_tools`'s doc comment) and a
    // dispatch attempt would be an "unknown tool" refusal instead of a clean "not offered".
    #[tokio::test]
    async fn run_sast_stays_excluded_without_a_diff_or_without_sast_enabled_even_if_allowlisted() {
        let cp = mock_no_knowledge_tools().await;
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let allow = Some(vec![
            ReviewToolSelector::Builtin(ReviewTool::RunSast),
            ReviewToolSelector::Builtin(ReviewTool::Finish),
        ]);

        let review = review_config(allow.clone());
        let (offered, _) = resolve_offered_tools(&review, false, true, &client, Uuid::nil()).await;
        assert!(
            !offered.iter().any(|spec| spec.function.name == RUN_SAST),
            "no diff"
        );

        let review = review_config(allow);
        let (offered, _) = resolve_offered_tools(&review, true, false, &client, Uuid::nil()).await;
        assert!(
            !offered.iter().any(|spec| spec.function.name == RUN_SAST),
            "sast disabled"
        );
    }
}
