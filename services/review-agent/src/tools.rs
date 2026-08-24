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

pub mod edit_file;
pub mod finish;
mod fs_safety;
pub mod graph;
pub mod list_directory;
pub mod mcp;
pub mod read_file;
pub mod record;
pub mod reply;
pub mod sast;
pub mod vector;
pub mod write_file;

pub use edit_file::EDIT_FILE;
pub use finish::{ABORT, FINISH, REPORT_PROGRESS};
pub use graph::{GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS, GRAPH_SEMANTIC_SEARCH};
pub use list_directory::LIST_DIRECTORY;
pub use mcp::MCP_TOOL_PREFIX;
pub use read_file::READ_FILE;
pub use record::{ADD_REVIEW_COMMENT, RETRACT_FINDING};
pub use reply::ADD_COMMENT;
pub use sast::{RUN_SAST, SastToolConfig};
pub use vector::VECTOR_SEMANTIC_SEARCH;
pub use write_file::WRITE_FILE;

pub(crate) const DEFAULT_LIMIT: i64 = 10;
pub(crate) const MAX_LIMIT: i64 = 100;

#[derive(Clone)]
pub(crate) struct ReviewServices {
    pub client: Arc<ControlPlaneClient>,
    pub embedder: Arc<EmbeddingsClient>,
    /// Repo `severity.min` (ADR-0030): the minimum priority (`"P0"`/`"P1"`/`"P2"`) a finding must meet
    /// to actually be recorded. `None` = no filter (record everything, today's behavior).
    pub min_priority: Option<String>,
}

/// Why a tool call's raw JSON arguments failed to parse into the tool's typed `Args`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("error: invalid arguments — {0}. Re-call with arguments matching the tool's schema.")]
    InvalidArguments(#[from] serde_json::Error),
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, ParseError> {
    Ok(serde_json::from_str::<T>(arguments)?)
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
        GRAPH_SEMANTIC_SEARCH,
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
///
/// `fs_write` (ADR-0104, story #497) gates `write_file`/`edit_file`/`list_directory` the same way —
/// **deliberately not** via `tool_defs()`/`known_tool_names()`/the `ReviewTool` allowlist enum. Those
/// drive the per-preset allowlist (ADR-0062/ADR-0103), now enforced on the live OpenCode-hosted review
/// path via `Tools::with_offer`'s `TurnFilter` (the supervisor resolves the allowlist and `lci-review-mcp`
/// applies it to both `tools/list` and `tools/call`). `fs_write` stays a SEPARATE `bool` so the fs-tool
/// trio is never reachable through the (now-enforced) allowlist either. Since review must NEVER get
/// write access, gating these three tools behind an explicit `bool` this function's every caller passes
/// `false` for is safe regardless of a preset's `tools:` config — no review preset can reach them. `open`
/// mode migrating onto this shared fs-tool family (ADR-0104's "More Information") is a future consumer
/// that would pass `true`.
#[allow(clippy::too_many_arguments)]
pub fn tool_registry(
    client: Arc<ControlPlaneClient>,
    embedder: Arc<EmbeddingsClient>,
    discovered: impl IntoIterator<Item = ToolSpec>,
    caps: RuntimeCaps,
    sast: Option<SastToolConfig>,
    min_priority: Option<String>,
    fs_write: bool,
) -> Result<ToolRegistry, RegistryError> {
    let services = ReviewServices {
        client,
        embedder,
        min_priority,
    };
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
    if fs_write {
        write_file::register(&mut registry, caps)?;
        edit_file::register(&mut registry, caps)?;
        list_directory::register(&mut registry, caps)?;
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
    /// The offered surface narrowed to a per-run allowlist (`review.<preset>.tools`, ADR-0062/
    /// ADR-0103). `TurnFilter::all()` = every registered tool. Used by `lcireview-mcp`'s OpenCode
    /// host so `tools/list` AND `tools/call` both honor the operator's preset config; the native loop
    /// never sets this (it narrows at the per-turn `Conversation` layer instead), so its
    /// `new`/`with_sast` constructors are untouched.
    offered: TurnFilter,
}

impl Tools {
    pub fn new(
        client: &ControlPlaneClient,
        embedder: &EmbeddingsClient,
        task_id: Uuid,
        checkout_root: &Path,
        discovered: impl IntoIterator<Item = ToolSpec>,
    ) -> Result<Self, RegistryError> {
        Self::with_sast(
            client,
            embedder,
            task_id,
            checkout_root,
            discovered,
            None,  // sast
            None,  // min_priority
            false, // fs_write
        )
    }

    /// Like [`Self::new`], but also registers the `run_sast` tool (ADR-0073) when `sast` is `Some`,
    /// applies the repo's `severity.min` (ADR-0030) when `min_priority` is `Some`, and registers the
    /// `write_file`/`edit_file`/`list_directory` fs-tool trio (ADR-0104, story #497) when `fs_write` is
    /// `true`. Used by the OpenCode review path's stdio MCP server (`lci-review-mcp`): `run_sast` runs
    /// in that separate process exactly as it does in the native loop, reusing `lci-agent-sast`
    /// verbatim. `sast: None` / `fs_write: false` leave those tools unregistered — the opt-in surface
    /// rule is enforced by the caller, so an un-offered tool never reaches `tools/list` or dispatch. No
    /// production caller passes `fs_write: true` today (see `tool_registry`'s doc comment).
    #[allow(clippy::too_many_arguments)]
    pub fn with_sast(
        client: &ControlPlaneClient,
        embedder: &EmbeddingsClient,
        task_id: Uuid,
        checkout_root: &Path,
        discovered: impl IntoIterator<Item = ToolSpec>,
        sast: Option<SastToolConfig>,
        min_priority: Option<String>,
        fs_write: bool,
    ) -> Result<Self, RegistryError> {
        Self::with_sast_and_filter(
            client,
            embedder,
            task_id,
            checkout_root,
            discovered,
            sast,
            min_priority,
            fs_write,
            TurnFilter::all(),
        )
    }

    /// The shared constructor behind [`Self::with_sast`] and [`Self::with_offer`]: builds the full
    /// registered registry (via [`tool_registry`]), then narrows what `specs()`/`dispatch()` expose
    /// to `offered`. The native path keeps [`Self::with_sast`] (a full-surface filter); the OpenCode
    /// MCP path passes a per-preset [`TurnFilter::only_names`].
    #[allow(clippy::too_many_arguments)]
    fn with_sast_and_filter(
        client: &ControlPlaneClient,
        embedder: &EmbeddingsClient,
        task_id: Uuid,
        checkout_root: &Path,
        discovered: impl IntoIterator<Item = ToolSpec>,
        sast: Option<SastToolConfig>,
        min_priority: Option<String>,
        fs_write: bool,
        offered: TurnFilter,
    ) -> Result<Self, RegistryError> {
        Ok(Self {
            registry: tool_registry(
                Arc::new(client.clone()),
                Arc::new(embedder.clone()),
                discovered,
                RuntimeCaps::default(),
                sast,
                min_priority,
                fs_write,
            )?,
            workspace: EagerWorkspace(checkout_root.to_path_buf()),
            task_id,
            offered,
        })
    }

    /// Like [`Self::with_sast`], but additionally restricts the offered surface to `allowed_names`
    /// (canonical registered tool names, e.g. `read_file` / `lightbridge_vector_semantic_search`).
    /// Used by `lci-review-mcp`: the supervisor resolves the preset's `review.tools` allowlist and the
    /// ADR-0066 `mcp__` selectors into exactly this set, set as `LCI_MCP_OFFERED_TOOLS`, and this
    /// constructor applies it as a [`TurnFilter::only_names`] so both `specs()` (what `tools/list`
    /// advertises) and `dispatch()` (what `tools/call` will execute) honor it. `None`/empty = the full
    /// registered surface, preserving today's behavior when a preset's allowlist is unset (the
    /// ADR-0062/ADR-0103 default). The allowed names are compared against REGISTERED names, so a bare
    /// canonical like `lightbridge_vector_semantic_search` offered to OpenCode (where MCP prefixing
    /// renders it `lightbridge_vector_semantic_search`) resolves regardless of the exact token; a name
    /// that matches no registered tool is simply never offered (an unknown tool, refused at dispatch).
    #[allow(clippy::too_many_arguments)]
    pub fn with_offer(
        client: &ControlPlaneClient,
        embedder: &EmbeddingsClient,
        task_id: Uuid,
        checkout_root: &Path,
        discovered: impl IntoIterator<Item = ToolSpec>,
        sast: Option<SastToolConfig>,
        min_priority: Option<String>,
        allowed_names: Option<&[String]>,
    ) -> Result<Self, RegistryError> {
        let offered = allowed_names
            .filter(|names| !names.is_empty())
            .map_or_else(TurnFilter::all, |names| {
                TurnFilter::only_names(names.iter().cloned())
            });
        Self::with_sast_and_filter(
            client,
            embedder,
            task_id,
            checkout_root,
            discovered,
            sast,
            min_priority,
            false, // fs_write — review never gets write access (ADR-0104); see `tool_registry`
            offered,
        )
    }

    pub async fn dispatch(&self, call: &ToolCallReq) -> ToolOutcome {
        let cx = ToolCx {
            task_id: self.task_id,
            workspace: &self.workspace,
        };
        match self.registry.view(&self.offered).dispatch(&cx, call).await {
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
        self.registry.view(&self.offered).specs().to_vec()
    }
}

fn render_refusal(refusal: DispatchRefusal) -> ToolOutcome {
    match refusal {
        DispatchRefusal::MissingCallId { tool_name } => ToolOutcome::Continue(format!(
            "error: tool {tool_name:?} requires a non-empty call id for deduplication. Re-call the tool."
        )),
        DispatchRefusal::NotOffered { tool_name } => ToolOutcome::Continue(format!(
            "error: unknown tool {tool_name:?}. Available tools: {VECTOR_SEMANTIC_SEARCH}, \
             {GRAPH_FIND_SYMBOL}, {GRAPH_GET_CALLERS}, {GRAPH_SEMANTIC_SEARCH}, {READ_FILE}, \
             {ADD_REVIEW_COMMENT}, {ADD_COMMENT}, {FINISH}, {REPORT_PROGRESS}, {ABORT}, {RUN_SAST}, \
             plus any discovered {MCP_TOOL_PREFIX}<server>__<tool>."
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

    // ADR-0104 / story #497 safety proof: `Tools::new` (used by every production review call site) sets
    // `fs_write: false`, so `write_file`/`edit_file`/`list_directory` are unregistered — a call to any
    // of them is refused as an unknown tool, not dispatched. Review must never gain write access.
    #[tokio::test]
    async fn fs_write_tools_are_unreachable_via_the_default_review_registry() {
        let client = ControlPlaneClient::new("http://unused", "tok");
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let tools = Tools::new(&client, &embedder, Uuid::nil(), Path::new("/tmp"), []).unwrap();
        for (name, args) in [
            (WRITE_FILE, r#"{"path":"x","content":"y"}"#),
            (EDIT_FILE, r#"{"path":"x","content":"y"}"#),
            (LIST_DIRECTORY, r#"{}"#),
        ] {
            let outcome = tools.dispatch(&call("t", name, args)).await;
            assert!(
                matches!(&outcome, ToolOutcome::Continue(m) if m.contains("unknown tool")),
                "{name} must be unreachable by default, got {outcome:?}"
            );
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

    // The opt-in path (a future consumer, e.g. `open` mode): with `fs_write: true`, the trio dispatches
    // through the same single registry every other tool does — proves the tools themselves work, not
    // just that they're correctly withheld by default.
    #[tokio::test]
    async fn fs_write_tools_dispatch_when_explicitly_enabled() {
        let cp = ControlPlaneClient::new("http://unused", "tok");
        let emb = EmbeddingsClient::new("http://unused", "key", "model");
        let checkout = tempfile::tempdir().unwrap();
        let registry = tool_registry(
            Arc::new(cp),
            Arc::new(emb),
            [],
            RuntimeCaps::default(),
            None,
            None,
            true,
        )
        .unwrap();
        let workspace = EagerWorkspace(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let outcome = registry
            .view(&TurnFilter::all())
            .dispatch(
                &cx,
                &call("w", WRITE_FILE, r#"{"path":"new.txt","content":"hello"}"#),
            )
            .await;
        let DispatchResult::Completed(ToolOutcome::Continue(message)) = outcome else {
            panic!("expected a completed write, got {outcome:?}");
        };
        assert!(message.contains("wrote"), "{message}");
        assert_eq!(
            std::fs::read_to_string(checkout.path().join("new.txt")).unwrap(),
            "hello"
        );

        let outcome = registry
            .view(&TurnFilter::all())
            .dispatch(&cx, &call("l", LIST_DIRECTORY, r#"{}"#))
            .await;
        assert!(matches!(
            outcome,
            DispatchResult::Completed(ToolOutcome::Continue(m)) if m.contains("new.txt")
        ));
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
            (
                "g3",
                GRAPH_SEMANTIC_SEARCH,
                r#"{"query":"auth retry logic"}"#,
            ),
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

    /// A control-plane mock that stubs every internal endpoint the review tools hit, so an
    /// `add_review_comment`/`finish` that IS offered can dispatch to a clean result rather than an
    /// `http://unused` connection error (which would still be refused correctly, but with a noisy
    /// error line and a less clear assertion failure on the "stays offered" path).
    async fn mock_offered_surface_cp() -> (MockServer, ControlPlaneClient) {
        let cp = MockServer::start().await;
        for endpoint in [
            "review/inline",
            "review/inline/retract",
            "review/comment",
            "review/summary",
            "review/finalize",
        ] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{endpoint}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(204))
                .mount(&cp)
                .await;
        }
        let uri = cp.uri();
        (cp, ControlPlaneClient::new(uri, "tok"))
    }

    // ADR-0062/ADR-0103 (#497/#537): `Tools::with_offer` narrows BOTH `specs()` (what `tools/list`
    // advertises) and `dispatch()` (what `tools/call` executes) to the allowlist. An unlisted
    // retrieval tool is refused as not-offered even though it IS registered.
    #[tokio::test]
    async fn with_offer_gates_specs_and_dispatch_to_the_allowlist() {
        let (_cp, client) = mock_offered_surface_cp().await;
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let tools = Tools::with_offer(
            &client,
            &embedder,
            Uuid::nil(),
            Path::new("/tmp"),
            [],
            None,
            None,
            Some(&[
                ADD_REVIEW_COMMENT.to_string(),
                FINISH.to_string(),
                ABORT.to_string(),
            ]),
        )
        .unwrap();

        let names: Vec<String> = tools.specs().iter().map(|s| s.name().to_string()).collect();
        assert_eq!(
            names,
            vec![ADD_REVIEW_COMMENT, FINISH, ABORT],
            "tools/list must advertise exactly the allowlist"
        );

        // A retrieval tool that is NOT listed is refused, not dispatched.
        let outcome = tools
            .dispatch(&call("r", READ_FILE, r#"{"path":"a.rs","start_line":1}"#))
            .await;
        assert!(
            matches!(outcome, ToolOutcome::Continue(ref message) if message.contains("unknown tool")),
            "an unlisted but REGISTERED tool must be refused-not-dispatched: {outcome:?}"
        );
        let outcome = tools
            .dispatch(&call("v", VECTOR_SEMANTIC_SEARCH, r#"{"query":"auth"}"#))
            .await;
        assert!(
            matches!(outcome, ToolOutcome::Continue(ref message) if message.contains("unknown tool"))
        );
    }

    #[tokio::test]
    async fn with_offer_full_surface_when_the_allowlist_is_unset() {
        let (_cp, client) = mock_offered_surface_cp().await;
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let tools = Tools::with_offer(
            &client,
            &embedder,
            Uuid::nil(),
            Path::new("/tmp"),
            [],
            None,
            None,
            None,
        )
        .unwrap();
        // Unset = the full registered surface (today's behavior when a preset's `tools` is unset) —
        // every built-in is advertised and dispatchable.
        let names: Vec<String> = tools.specs().iter().map(|s| s.name().to_string()).collect();
        // `run_sast` is unregistered here (sast: None — see `tool_registry`), so the full-surface
        // expectation is every OTHER built-in.
        for builtin in known_tool_names().into_iter().filter(|n| *n != RUN_SAST) {
            assert!(
                names.contains(&builtin.to_string()),
                "missing {builtin}: {names:?}"
            );
        }
        assert_eq!(
            tools
                .dispatch(&call("f", FINISH, r#"{"summary":"done"}"#))
                .await,
            ToolOutcome::Finish
        );
    }

    #[tokio::test]
    async fn with_offer_empty_list_is_the_full_surface_too() {
        // An empty env value (a supervisor that serialized nothing) must not collapse to "no tools
        // at all" — that would strand every review. Treat it like the unset case.
        let (_cp, client) = mock_offered_surface_cp().await;
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let tools = Tools::with_offer(
            &client,
            &embedder,
            Uuid::nil(),
            Path::new("/tmp"),
            [],
            None,
            None,
            Some(&[]),
        )
        .unwrap();
        assert!(
            !tools.specs().is_empty(),
            "an EMPTY allowlist must keep the full surface, not empty it"
        );
    }
}
