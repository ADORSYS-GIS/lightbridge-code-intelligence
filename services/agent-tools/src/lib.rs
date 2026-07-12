//! Runtime-independent tool contracts and guarded per-turn registry views.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use uuid::Uuid;

/// The boxed future used only at the heterogeneous tool/workspace boundaries.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One executable tool. Dynamic dispatch is intentional: a registry is heterogeneous.
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn kind(&self) -> ToolKind;
    fn replay(&self) -> ReplaySafety;
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome>;
}

/// Classification used by budgets and per-turn offered-set policies.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ToolKind {
    ReadOnly(ReadKind),
    Write,
    Terminal,
    Progress,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReadKind {
    Retrieval,
    File,
    Knowledge,
}

/// The replay guarantee a host must honor before registering a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaySafety {
    ReadOnly,
    Idempotent,
    NeedsDedupKey,
}

/// Capabilities supplied by the runtime hosting a registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeCaps {
    /// Whether completed effects may be replayed by this host.
    pub replays_completed_steps: bool,
    pub per_call_dedup: bool,
}

/// A checkout provider. Implementations may eagerly or lazily materialize the root.
pub trait Workspace: Send + Sync {
    fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceError {
    reason: String,
}

impl WorkspaceError {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for WorkspaceError {}

/// Context common to all tools. HTTP services remain owned by concrete review-tool implementations.
pub struct ToolCx<'a> {
    pub task_id: Uuid,
    pub workspace: &'a dyn Workspace,
}

/// A refusal is typed so each assembly renders its own exact model-facing steer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchRefusal {
    NotOffered { tool_name: String },
    MissingCallId { tool_name: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchResult {
    Completed(ToolOutcome),
    Refused(DispatchRefusal),
}

/// A monotonic restriction of the tools offered on one turn.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnFilter {
    allowed_names: Option<BTreeSet<String>>,
    blocked_kinds: BTreeSet<ToolKind>,
}

impl TurnFilter {
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn only_names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_names: Some(names.into_iter().map(Into::into).collect()),
            blocked_kinds: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn without_kind(mut self, kind: ToolKind) -> Self {
        self.blocked_kinds.insert(kind);
        self
    }

    /// Intersect another policy restriction. This operation can never widen the set.
    pub fn narrow(&mut self, other: &Self) {
        match (&mut self.allowed_names, &other.allowed_names) {
            (Some(current), Some(next)) => current.retain(|name| next.contains(name)),
            (None, Some(next)) => self.allowed_names = Some(next.clone()),
            _ => {}
        }
        self.blocked_kinds
            .extend(other.blocked_kinds.iter().copied());
    }

    fn offers(&self, tool: &dyn Tool) -> bool {
        !self.blocked_kinds.contains(&tool.kind())
            && self
                .allowed_names
                .as_ref()
                .is_none_or(|names| names.contains(tool.spec().name()))
    }
}

/// Registration failures are startup errors, before a model can invoke a tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    DuplicateName(String),
    MissingDedupCapability(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(formatter, "tool {name:?} is already registered"),
            Self::MissingDedupCapability(name) => write!(
                formatter,
                "tool {name:?} needs a per-call dedup key, but the runtime cannot provide one"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Stable-order owner of the complete tool surface.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        tool: Arc<dyn Tool>,
        caps: RuntimeCaps,
    ) -> Result<(), RegistryError> {
        let name = tool.spec().name();
        if self
            .tools
            .iter()
            .any(|existing| existing.spec().name() == name)
        {
            return Err(RegistryError::DuplicateName(name.to_string()));
        }
        if tool.replay() == ReplaySafety::NeedsDedupKey
            && caps.replays_completed_steps
            && !caps.per_call_dedup
        {
            return Err(RegistryError::MissingDedupCapability(name.to_string()));
        }
        self.tools.push(tool);
        Ok(())
    }

    #[must_use]
    pub fn view(&self, filter: &TurnFilter) -> TurnView<'_> {
        let offered: Vec<&dyn Tool> = self
            .tools
            .iter()
            .map(Arc::as_ref)
            .filter(|tool| filter.offers(*tool))
            .collect();
        let specs = offered.iter().map(|tool| tool.spec().clone()).collect();
        TurnView { offered, specs }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Classification lookup used by the generic loop to partition read batches from ordered
    /// effect calls. The tool implementation remains hidden behind the registry.
    #[must_use]
    pub fn kind(&self, name: &str) -> Option<ToolKind> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name() == name)
            .map(|tool| tool.kind())
    }

    #[must_use]
    pub fn replay(&self, name: &str) -> Option<ReplaySafety> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name() == name)
            .map(|tool| tool.replay())
    }
}

/// One turn's offered specs and guarded dispatcher.
pub struct TurnView<'r> {
    offered: Vec<&'r dyn Tool>,
    specs: Vec<ToolSpec>,
}

impl TurnView<'_> {
    #[must_use]
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub async fn dispatch(&self, cx: &ToolCx<'_>, call: &ToolCallReq) -> DispatchResult {
        match self
            .offered
            .iter()
            .find(|tool| tool.spec().name() == call.function.name)
        {
            Some(tool)
                if tool.replay() == ReplaySafety::NeedsDedupKey && call.id.trim().is_empty() =>
            {
                DispatchResult::Refused(DispatchRefusal::MissingCallId {
                    tool_name: call.function.name.clone(),
                })
            }
            Some(tool) => DispatchResult::Completed(tool.call(cx, call).await),
            None => DispatchResult::Refused(DispatchRefusal::NotOffered {
                tool_name: call.function.name.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_types::FunctionCallReq;

    struct FixedTool {
        spec: ToolSpec,
        kind: ToolKind,
        replay: ReplaySafety,
    }

    impl FixedTool {
        fn new(name: &str, kind: ToolKind, replay: ReplaySafety) -> Self {
            Self {
                spec: ToolSpec::function(name, "test", serde_json::json!({"type": "object"})),
                kind,
                replay,
            }
        }
    }

    impl Tool for FixedTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn kind(&self) -> ToolKind {
            self.kind
        }

        fn replay(&self) -> ReplaySafety {
            self.replay
        }

        fn call<'a>(
            &'a self,
            _cx: &'a ToolCx<'a>,
            call: &'a ToolCallReq,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(
                async move { ToolOutcome::Continue(format!("ran {}", call.function.arguments)) },
            )
        }
    }

    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
    }

    fn call(name: &str) -> ToolCallReq {
        ToolCallReq {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: name.into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        }
    }

    #[tokio::test]
    async fn registry_filters_in_registration_order_and_guards_dispatch() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                Arc::new(FixedTool::new(
                    "read",
                    ToolKind::ReadOnly(ReadKind::File),
                    ReplaySafety::ReadOnly,
                )),
                RuntimeCaps::default(),
            )
            .unwrap();
        registry
            .register(
                Arc::new(FixedTool::new(
                    "finish",
                    ToolKind::Terminal,
                    ReplaySafety::Idempotent,
                )),
                RuntimeCaps::default(),
            )
            .unwrap();
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());

        let mut filter = TurnFilter::all();
        filter.narrow(&TurnFilter::only_names(["read", "finish"]));
        filter.narrow(&TurnFilter::all().without_kind(ToolKind::ReadOnly(ReadKind::File)));
        let view = registry.view(&filter);
        assert_eq!(view.specs()[0].name(), "finish");

        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &Root,
        };
        assert_eq!(
            view.dispatch(&cx, &call("finish")).await,
            DispatchResult::Completed(ToolOutcome::Continue("ran {}".into()))
        );
        assert_eq!(
            view.dispatch(&cx, &call("read")).await,
            DispatchResult::Refused(DispatchRefusal::NotOffered {
                tool_name: "read".into()
            })
        );
        assert_eq!(cx.workspace.root().await.unwrap(), Path::new("/tmp"));
    }

    #[test]
    fn registry_rejects_duplicates_and_unsupported_dedup_tools() {
        let mut registry = ToolRegistry::new();
        let first = Arc::new(FixedTool::new(
            "comment",
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
        ));
        assert_eq!(
            registry.register(
                first.clone(),
                RuntimeCaps {
                    replays_completed_steps: true,
                    per_call_dedup: false,
                }
            ),
            Err(RegistryError::MissingDedupCapability("comment".into()))
        );
        registry
            .register(
                first,
                RuntimeCaps {
                    replays_completed_steps: true,
                    per_call_dedup: true,
                },
            )
            .unwrap();
        assert_eq!(
            registry.register(
                Arc::new(FixedTool::new(
                    "comment",
                    ToolKind::Write,
                    ReplaySafety::Idempotent,
                )),
                RuntimeCaps::default(),
            ),
            Err(RegistryError::DuplicateName("comment".into()))
        );
        assert!(
            RegistryError::DuplicateName("x".into())
                .to_string()
                .contains("already")
        );
        assert!(
            WorkspaceError::new("missing")
                .to_string()
                .contains("missing")
        );
    }

    #[tokio::test]
    async fn needs_dedup_uses_the_actual_call_id_and_rejects_missing_ids() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                Arc::new(FixedTool::new(
                    "comment",
                    ToolKind::Write,
                    ReplaySafety::NeedsDedupKey,
                )),
                RuntimeCaps::default(),
            )
            .unwrap();
        let view = registry.view(&TurnFilter::all());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &Root,
        };
        let mut missing = call("comment");
        missing.id.clear();
        assert_eq!(
            view.dispatch(&cx, &missing).await,
            DispatchResult::Refused(DispatchRefusal::MissingCallId {
                tool_name: "comment".into()
            })
        );
        let actual = call("comment");
        assert_eq!(
            view.dispatch(&cx, &actual).await,
            DispatchResult::Completed(ToolOutcome::Continue("ran {}".into()))
        );
    }
}
