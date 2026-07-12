//! The open flow: compose the shared `AgentLoop` with the write+execute registry, the open budget
//! policies, and the open [`LoopLimits`], then run it over the injected runtime + model. Mirrors
//! `review`'s `run_review` — same generic loop, different registry + policies (the mode *is* the
//! toolset). Generic over the runtime + model so the `Passthrough` Job host and an in-process scripted
//! model share exactly this assembly; the same seam inherits `CheckpointRuntime` replay when a
//! replaying host is wired (ADR-0087).

use lci_agent_loop::{
    AgentLoop, Conversation, LoopLimits, LoopOutcome, ModelClient, TranscriptSink,
};
use lci_agent_step::StepRuntime;
use lci_agent_tools::{ToolCx, ToolRegistry};

use crate::policies::{OpenBudgets, build_policies};

pub use crate::workspace::SandboxWorkspace;

/// Run one open task: compose the policies + limits, drive the engine loop, and return the
/// [`LoopOutcome`] the host maps to a result. `Finished` = the agent called `propose_pr` (a PR was
/// proposed via mediated egress); `Aborted` = it called `abort`; `Exhausted` = the turn budget ran out.
/// Only a true transport/loop failure is `Err`.
pub async fn run_open<R, M>(
    runtime: R,
    model: M,
    sink: Box<dyn TranscriptSink>,
    cx: &ToolCx<'_>,
    registry: ToolRegistry,
    conversation: Conversation,
    budgets: OpenBudgets,
) -> anyhow::Result<LoopOutcome>
where
    R: StepRuntime,
    M: ModelClient,
{
    let policies = build_policies(&budgets);
    let mut agent = AgentLoop::new(
        runtime,
        model,
        registry,
        policies,
        sink,
        LoopLimits {
            max_turns: budgets.max_turns,
            max_batch_size: budgets.max_batch_size,
            circuit_breaker_threshold: budgets.circuit_breaker_threshold,
            no_tool_nudge:
                "Use the tools to investigate and edit, run the build/tests, commit your \
                change, then call `propose_pr` (or `abort`). Do not reply only in prose."
                    .into(),
        },
    );
    let outcome = agent
        .run(conversation, cx)
        .await
        .map_err(|error| anyhow::anyhow!("open agent loop failed: {error}"))?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;
    use std::sync::Arc;

    use lci_agent_clients::ControlPlaneClient;
    use lci_agent_loop::{ChatMessage, RequestOptions};
    use lci_agent_step::Passthrough;
    use lci_agent_testkit::{CapturingSink, ScriptedModel};
    use lci_agent_tools::RuntimeCaps;
    use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::{
        DEFAULT_COMMAND_OUTPUT_CAP, DEFAULT_COMMAND_TIMEOUT, EDIT_FILE, PROPOSE_PR, RUN_COMMAND,
        tool_registry,
    };

    fn call(id: &str, name: &str, args: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: args.into(),
                },
                extra_content: None,
            }],
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A repo with one committed file, so `HEAD~1..HEAD` is well-defined once the agent commits on top.
    fn seed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(
            dir.path(),
            &["config", "user.email", "agent@lightbridge.dev"],
        );
        git(dir.path(), &["config", "user.name", "open-agent"]);
        std::fs::write(dir.path().join("README.md"), "base\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "initial"]);
        dir
    }

    // Merge bar (ADR-0088): the open loop drives to `propose_pr` under a ScriptedModel, in-process. The
    // agent edits a file, commits a local branch, and proposes — the terminal `propose_pr` hits the
    // mediated internal API exactly once and the loop finishes.
    #[tokio::test]
    async fn open_loop_edits_commits_and_proposes_a_pr() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/propose-pr", Uuid::nil())))
            .respond_with(ResponseTemplate::new(202))
            .mount(&cp)
            .await;

        let repo = seed_repo();
        let client = Arc::new(ControlPlaneClient::new(cp.uri(), "tok"));
        let registry = tool_registry(
            client,
            DEFAULT_COMMAND_TIMEOUT,
            DEFAULT_COMMAND_OUTPUT_CAP,
            RuntimeCaps::default(),
        )
        .unwrap();
        let workspace = SandboxWorkspace::new(repo.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let conversation = Conversation::new(
            vec![
                ChatMessage::system("be an open agent"),
                ChatMessage::user("do the ticket"),
            ],
            RequestOptions {
                model: "m".to_string(),
                ..RequestOptions::default()
            },
        );
        let script = [
            call(
                "e",
                EDIT_FILE,
                r#"{"path":"src/feature.rs","content":"pub fn feature() {}\n"}"#,
            ),
            call("a", RUN_COMMAND, r#"{"command":"git","args":["add","-A"]}"#),
            call(
                "c",
                RUN_COMMAND,
                r#"{"command":"git","args":["commit","-q","-m","add feature"]}"#,
            ),
            call(
                "p",
                PROPOSE_PR,
                r#"{"title":"Add feature","body":"AI Usage Declaration: authored by the open agent. Source of truth: #357. Verification: cargo test passed.","branch":"open/357"}"#,
            ),
        ];

        let outcome = run_open(
            Passthrough,
            ScriptedModel::new(script),
            Box::new(CapturingSink::default()),
            &cx,
            registry,
            conversation,
            OpenBudgets::default(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, LoopOutcome::Finished);
        // The edit landed inside the sandbox workdir.
        assert!(repo.path().join("src/feature.rs").exists());
        // The terminal step called the mediated PR-open endpoint exactly once (no forge call, no push).
        let hits = cp
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/propose-pr"))
            .count();
        assert_eq!(
            hits, 1,
            "propose_pr must mediate exactly one PR-open intent"
        );
    }

    // The clean give-up path: `abort` proposes nothing and never touches the control plane.
    #[tokio::test]
    async fn open_loop_abort_proposes_nothing() {
        let cp = MockServer::start().await;
        let repo = seed_repo();
        let client = Arc::new(ControlPlaneClient::new(cp.uri(), "tok"));
        let registry = tool_registry(
            client,
            DEFAULT_COMMAND_TIMEOUT,
            DEFAULT_COMMAND_OUTPUT_CAP,
            RuntimeCaps::default(),
        )
        .unwrap();
        let workspace = SandboxWorkspace::new(repo.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let conversation = Conversation::new(
            vec![
                ChatMessage::system("be an open agent"),
                ChatMessage::user("do it"),
            ],
            RequestOptions {
                model: "m".to_string(),
                ..RequestOptions::default()
            },
        );
        let outcome = run_open(
            Passthrough,
            ScriptedModel::new([call(
                "b",
                crate::tools::ABORT,
                r#"{"reason":"underspecified"}"#,
            )]),
            Box::new(CapturingSink::default()),
            &cx,
            registry,
            conversation,
            OpenBudgets::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            LoopOutcome::Aborted {
                reason: "underspecified".into()
            }
        );
        assert!(cp.received_requests().await.unwrap().is_empty());
    }
}
