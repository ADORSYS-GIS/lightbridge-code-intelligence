//! Runtime-independent tool contracts and guarded per-turn registry views.

mod filter;
mod registry;
mod tool;
mod workspace;

pub use filter::TurnFilter;
pub use registry::{DispatchRefusal, DispatchResult, RegistryError, ToolRegistry, TurnView};
pub use tool::{BoxFuture, ReadKind, ReplaySafety, RuntimeCaps, Tool, ToolKind};
pub use workspace::{ToolCx, Workspace, WorkspaceError};

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
    use uuid::Uuid;

    use super::*;

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
