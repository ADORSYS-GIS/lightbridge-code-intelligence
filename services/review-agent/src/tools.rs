//! Review-specific tool assembly and compatibility dispatcher.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_tools::{
    BoxFuture, DispatchRefusal, DispatchResult, RegistryError, RuntimeCaps, ToolCx, ToolRegistry,
    TurnFilter, Workspace, WorkspaceError,
};
pub use lci_agent_types::ToolOutcome;
use lci_agent_types::{ToolCallReq, ToolSpec};
use uuid::Uuid;

pub mod finish;
pub mod graph;
pub mod mcp;
pub mod read_file;
pub mod record;
pub mod reply;
pub mod sast;
pub mod vector;

pub use finish::{ABORT, FINISH, REPORT_PROGRESS};
pub use graph::{GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS};
pub use mcp::MCP_TOOL_PREFIX;
pub use read_file::READ_FILE;
pub use record::{ADD_REVIEW_COMMENT, RETRACT_FINDING};
pub use reply::ADD_COMMENT;
pub use sast::{RUN_SAST, SastToolConfig};
pub use vector::VECTOR_SEMANTIC_SEARCH;

pub(crate) const DEFAULT_LIMIT: i64 = 10;
pub(crate) const MAX_LIMIT: i64 = 100;

#[derive(Clone)]
pub(crate) struct ReviewServices {
    pub client: Arc<ControlPlaneClient>,
    pub embedder: Arc<EmbeddingsClient>,
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str::<T>(arguments).map_err(|error| {
        format!(
            "error: invalid arguments — {error}. Re-call with arguments matching the tool's schema."
        )
    })
}

pub(crate) fn render<T: serde::Serialize>(tool: &str, result: anyhow::Result<T>) -> String {
    match result.and_then(|value| Ok(serde_json::to_string_pretty(&value)?)) {
        Ok(json) if json.trim() == "[]" => EMPTY_RETRIEVAL_RESULT.to_string(),
        Ok(json) => json,
        Err(error) => format!("error: {tool} failed: {error:#}"),
    }
}

pub(crate) fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

pub const EMPTY_RETRIEVAL_RESULT: &str = "No results matched. An empty result means the index found \
    nothing for this query — it is NOT evidence that the symbol, code, or feature is absent or was \
    removed (it may be unindexed, renamed, or phrased differently). To check whether something exists, \
    open the relevant file with `read_file`. Do not record a finding from an empty retrieval alone \
    (ADR-0047).";

/// The complete built-in surface in the legacy stable order, plus `run_sast` (ADR-0073) appended last.
pub fn tool_defs() -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    specs.push(vector::spec());
    specs.extend(graph::specs());
    specs.push(read_file::spec());
    specs.extend(record::specs());
    specs.push(reply::spec());
    specs.push(finish::finish_spec());
    specs.extend(finish::aux_specs());
    specs.push(sast::spec());
    specs
}

pub fn known_tool_names() -> Vec<&'static str> {
    vec![
        VECTOR_SEMANTIC_SEARCH,
        GRAPH_FIND_SYMBOL,
        GRAPH_GET_CALLERS,
        READ_FILE,
        ADD_REVIEW_COMMENT,
        RETRACT_FINDING,
        ADD_COMMENT,
        FINISH,
        REPORT_PROGRESS,
        ABORT,
        RUN_SAST,
    ]
}

/// Assemble the exact concrete tools. Each module owns its own spec, replay class, and execution.
/// `sast` is `None` when SAST is off or there's no diff to scope a scan to (ADR-0073) — `run_sast` then
/// simply isn't registered, so a dispatch attempt is refused as an unknown tool rather than silently
/// scanning nothing.
pub fn tool_registry(
    client: Arc<ControlPlaneClient>,
    embedder: Arc<EmbeddingsClient>,
    discovered: impl IntoIterator<Item = ToolSpec>,
    caps: RuntimeCaps,
    sast: Option<SastToolConfig>,
) -> Result<ToolRegistry, RegistryError> {
    let services = ReviewServices { client, embedder };
    let mut registry = ToolRegistry::new();
    vector::register(&mut registry, &services, caps)?;
    graph::register(&mut registry, &services, caps)?;
    read_file::register(&mut registry, caps)?;
    record::register(&mut registry, &services, caps)?;
    reply::register(&mut registry, &services, caps)?;
    finish::register(&mut registry, &services, caps)?;
    for spec in discovered {
        mcp::register(&mut registry, &services, spec, caps)?;
    }
    if let Some(tool_config) = sast {
        self::sast::register(&mut registry, &services, tool_config, caps)?;
    }
    Ok(registry)
}

/// A [`Workspace`] that eagerly holds an already-materialized checkout root — the current Job host has
/// the working tree on disk before the agent starts, so `root()` resolves immediately. Public so the
/// host (and the golden test) can build the [`ToolCx`] the loop runs under; see
/// [`crate::flows::eager_workspace`].
#[derive(Clone)]
pub struct EagerWorkspace(PathBuf);

impl EagerWorkspace {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }
}

impl Workspace for EagerWorkspace {
    fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
        Box::pin(async { Ok(self.0.as_path()) })
    }
}

/// Compatibility wrapper used by the current Job loop. It dispatches through the same typed tools
/// that R1d will consume; there is no second name-matched implementation.
pub struct Tools {
    registry: ToolRegistry,
    workspace: EagerWorkspace,
    task_id: Uuid,
}

impl Tools {
    pub fn new(
        client: &ControlPlaneClient,
        embedder: &EmbeddingsClient,
        task_id: Uuid,
        checkout_root: &Path,
        discovered: impl IntoIterator<Item = ToolSpec>,
    ) -> Result<Self, RegistryError> {
        Ok(Self {
            registry: tool_registry(
                Arc::new(client.clone()),
                Arc::new(embedder.clone()),
                discovered,
                RuntimeCaps::default(),
                None,
            )?,
            workspace: EagerWorkspace(checkout_root.to_path_buf()),
            task_id,
        })
    }

    pub async fn dispatch(&self, call: &ToolCallReq) -> ToolOutcome {
        let cx = ToolCx {
            task_id: self.task_id,
            workspace: &self.workspace,
        };
        match self
            .registry
            .view(&TurnFilter::all())
            .dispatch(&cx, call)
            .await
        {
            DispatchResult::Completed(outcome) => outcome,
            DispatchResult::Refused(refusal) => render_refusal(refusal),
        }
    }

    /// The specs of every registered review tool. Public so a host that presents these tools over a
    /// different protocol than the native loop — the OpenCode ACP host's MCP surface (RFC-0009) —
    /// can advertise the exact same tuned schemas (e.g. `add_review_comment`'s P0/P1/P2 rubric)
    /// instead of re-declaring them and risking drift.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.registry.view(&TurnFilter::all()).specs().to_vec()
    }
}

fn render_refusal(refusal: DispatchRefusal) -> ToolOutcome {
    match refusal {
        DispatchRefusal::MissingCallId { tool_name } => ToolOutcome::Continue(format!(
            "error: tool {tool_name:?} requires a non-empty call id for deduplication. Re-call the tool."
        )),
        DispatchRefusal::NotOffered { tool_name } => ToolOutcome::Continue(format!(
            "error: unknown tool {tool_name:?}. Available tools: {VECTOR_SEMANTIC_SEARCH}, \
             {GRAPH_FIND_SYMBOL}, {GRAPH_GET_CALLERS}, {READ_FILE}, {ADD_REVIEW_COMMENT}, \
             {ADD_COMMENT}, {FINISH}, {REPORT_PROGRESS}, {ABORT}, {RUN_SAST}, plus any discovered \
             {MCP_TOOL_PREFIX}<server>__<tool>."
        )),
    }
}

/// Exact review-layer rendering of the current fast-tier refusal.
#[must_use]
pub fn fast_refusal(tool: &str) -> String {
    format!(
        "`{tool}` is not available in this fast review pass — review the diff directly, \
         record any findings with add_review_comment, then call finish."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_types::FunctionCallReq;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn call(id: &str, name: &str, arguments: &str) -> ToolCallReq {
        ToolCallReq {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: name.into(),
                arguments: arguments.into(),
            },
            extra_content: None,
        }
    }

    #[test]
    fn builtins_keep_the_legacy_order_and_full_schemas() {
        let specs = tool_defs();
        assert_eq!(
            specs.iter().map(ToolSpec::name).collect::<Vec<_>>(),
            known_tool_names()
        );
        assert!(
            specs
                .iter()
                .all(|spec| !spec.function.description.is_empty())
        );
        assert!(
            specs
                .iter()
                .all(|spec| spec.function.parameters.is_object())
        );
    }

    #[test]
    fn exact_fast_refusal_is_owned_by_the_review_layer() {
        assert_eq!(
            fast_refusal(READ_FILE),
            "`read_file` is not available in this fast review pass — review the diff directly, record any findings with add_review_comment, then call finish."
        );
    }

    #[tokio::test]
    async fn add_comment_requires_the_real_nonempty_call_id() {
        let cp = ControlPlaneClient::new("http://unused", "tok");
        let emb = EmbeddingsClient::new("http://unused", "key", "model");
        let tools = Tools::new(&cp, &emb, Uuid::nil(), Path::new("/tmp"), []).unwrap();
        let result = tools
            .dispatch(&call("", ADD_COMMENT, r#"{"body":"hi"}"#))
            .await;
        assert!(
            matches!(result, ToolOutcome::Continue(message) if message.contains("non-empty call id"))
        );
    }

    #[tokio::test]
    async fn every_concrete_module_dispatches_through_the_single_registry() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/graph/query", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&cp)
            .await;
        for endpoint in [
            "review/inline",
            "review/inline/retract",
            "review/comment",
            "review/summary",
        ] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{endpoint}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(204))
                .mount(&cp)
                .await;
        }
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/knowledge/call",
                Uuid::nil()
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text":"external fact"})),
            )
            .mount(&cp)
            .await;
        let embeddings = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"index":0,"embedding":[0.1,0.2]}]})),
            )
            .mount(&embeddings)
            .await;
        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::write(checkout.path().join("a.rs"), "one\ntwo\n")
            .await
            .unwrap();
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let embedder = EmbeddingsClient::new(&embeddings.uri(), "key", "model");
        let discovered = ToolSpec::function(
            "mcp__docs__lookup",
            "docs",
            serde_json::json!({"type":"object"}),
        );
        let tools = Tools::new(
            &client,
            &embedder,
            Uuid::nil(),
            checkout.path(),
            [discovered],
        )
        .unwrap();
        for (id, name, args) in [
            ("v", VECTOR_SEMANTIC_SEARCH, r#"{"query":"auth"}"#),
            ("g1", GRAPH_FIND_SYMBOL, r#"{"term":"main"}"#),
            ("g2", GRAPH_GET_CALLERS, r#"{"node_id":"n"}"#),
        ] {
            assert_eq!(
                tools.dispatch(&call(id, name, args)).await,
                ToolOutcome::Continue(EMPTY_RETRIEVAL_RESULT.into())
            );
        }
        assert!(
            matches!(tools.dispatch(&call("r",READ_FILE,r#"{"path":"a.rs","start_line":2}"#)).await,ToolOutcome::Continue(text) if text.contains("lines 2-2") && text.contains("two"))
        );
        assert_eq!(tools.dispatch(&call("a",ADD_REVIEW_COMMENT,r#"{"file":"a.rs","line":2,"title":"t","priority":"P2","category":"quality","body":"b","evidence":"line 2"}"#)).await,ToolOutcome::Continue("recorded finding at a.rs:2".into()));
        assert_eq!(
            tools
                .dispatch(&call(
                    "x",
                    RETRACT_FINDING,
                    r#"{"file":"a.rs","line":2,"reason":"wrong"}"#
                ))
                .await,
            ToolOutcome::Continue("retracted finding at a.rs:2 (wrong)".into())
        );
        assert_eq!(
            tools
                .dispatch(&call("c", ADD_COMMENT, r#"{"body":"hello"}"#))
                .await,
            ToolOutcome::Continue("comment recorded".into())
        );
        assert_eq!(
            tools
                .dispatch(&call("p", REPORT_PROGRESS, r#"{"note":"working"}"#))
                .await,
            ToolOutcome::Continue("acknowledged".into())
        );
        assert!(
            matches!(tools.dispatch(&call("m","mcp__docs__lookup",r#"{"query":"rust"}"#)).await,ToolOutcome::Continue(text) if text.contains("UNTRUSTED") && text.contains("external fact"))
        );
        assert_eq!(
            tools
                .dispatch(&call("f", FINISH, r#"{"summary":"done"}"#))
                .await,
            ToolOutcome::Finish
        );
        assert_eq!(
            tools
                .dispatch(&call("b", ABORT, r#"{"reason":"stop"}"#))
                .await,
            ToolOutcome::Abort("stop".into())
        );
        assert!(
            matches!(tools.dispatch(&call("u","delete_repo","{}")).await,ToolOutcome::Continue(message) if message.contains("unknown tool"))
        );
        for name in known_tool_names() {
            assert!(matches!(
                tools.dispatch(&call("bad", name, "not json")).await,
                ToolOutcome::Continue(_)
            ));
        }
    }
}
