//! `run_command` — the sandboxed build/test/tooling runner (ADR-0088). **This is the tool that runs
//! untrusted code** (the repo's own build scripts + the agent's generated code); the sandbox *is* its
//! safety — it executes inside the same pod under the same non-root / seccomp / egress-restricted
//! posture (enforced deploy-side by the hardened Job spec, `control-plane/src/integrations/k8s.rs`).
//!
//! Bounding (ADR-0088 "bounded"): each invocation has a per-command wall-clock timeout; the overall run
//! is additionally capped by the loop's turn budget and the pod's `activeDeadlineSeconds`. Output is
//! truncated so a chatty build can't blow the context budget. The program is executed directly (no
//! shell), cwd pinned to the workdir root, so argument handling is predictable.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use tokio::process::Command;

use super::parse;

pub const RUN_COMMAND: &str = "run_command";

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        RUN_COMMAND,
        "Run a build/test/tooling command inside the sandbox working tree (e.g. cargo test, npm run \
         build, git add/commit, git apply). Executed directly (no shell), cwd is the workdir root, \
         bounded by a per-command timeout. Returns the exit code plus truncated stdout/stderr.",
        serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"The program to run (e.g. \"cargo\", \"git\")."},"args":{"type":"array","items":{"type":"string"},"description":"Arguments passed to the program, one per array element."}},"required":["command"]}),
    )
}

struct RunCommandTool {
    spec: ToolSpec,
    timeout: Duration,
    output_cap: usize,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    timeout: Duration,
    output_cap: usize,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(
        Arc::new(RunCommandTool {
            spec: spec(),
            timeout,
            output_cap,
        }),
        caps,
    )
}

impl Tool for RunCommandTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        // A side-effecting execution — dispatched serially (not batched with read-only calls) and
        // journaled as its own step, like a write.
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        // Sandbox-local + egress-denied, so a re-run is contained; under a replaying host a completed
        // command step is journaled and not re-executed.
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<Args>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => return ToolOutcome::Continue(error),
            };
            if args.command.trim().is_empty() {
                return ToolOutcome::Continue("error: command must not be empty.".into());
            }
            let root = match cx.workspace.root().await {
                Ok(root) => root.to_path_buf(),
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not materialize the sandbox workdir: {error}"
                    ));
                }
            };
            ToolOutcome::Continue(
                run(
                    &root,
                    &args.command,
                    &args.args,
                    self.timeout,
                    self.output_cap,
                )
                .await,
            )
        })
    }
}

async fn run(
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
    timeout: Duration,
    output_cap: usize,
) -> String {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return format!("error: could not run {command:?}: {error}"),
        Err(_) => {
            return format!(
                "error: {command:?} exceeded the per-command timeout of {}s and was aborted.",
                timeout.as_secs()
            );
        }
    };
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |c| c.to_string());
    let stdout = truncate(&String::from_utf8_lossy(&output.stdout), output_cap);
    let stderr = truncate(&String::from_utf8_lossy(&output.stderr), output_cap);
    format!("exit code: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
}

fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated at {cap} bytes]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_exit_code_and_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            dir.path(),
            "echo",
            &["hello".to_string()],
            Duration::from_secs(5),
            1024,
        )
        .await;
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), "false", &[], Duration::from_secs(5), 1024).await;
        assert!(out.contains("exit code: 1"), "{out}");
    }

    #[tokio::test]
    async fn enforces_the_per_command_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            dir.path(),
            "sleep",
            &["5".to_string()],
            Duration::from_millis(50),
            1024,
        )
        .await;
        assert!(out.contains("timeout"), "{out}");
    }
}
