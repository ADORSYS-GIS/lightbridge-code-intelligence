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

use super::{parse, resolve_root};

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
            let root = match resolve_root(cx).await {
                Ok(root) => root.to_path_buf(),
                Err(error) => return ToolOutcome::Continue(error),
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
    use tokio::io::AsyncReadExt as _;

    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If this future is dropped (e.g. the whole tool call is cancelled) the child would otherwise
        // be orphaned — tokio's default is NOT to kill on drop. Reap it instead of leaking a process.
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return format!("error: could not run {command:?}: {error}"),
    };

    // Drain both pipes concurrently, keeping only the first `output_cap + 1024` bytes of each.
    // `cmd.output()` would buffer stdout/stderr *unbounded*, so a pathological process could OOM the
    // pod; capturing into a capped buffer bounds peak memory. Crucially, once the cap is reached we do
    // NOT stop reading — we keep draining the remainder of the pipe to `sink()` (discarding it) so a
    // verbose-but-finite command (e.g. `cargo build`) never blocks on a full ~64 KB OS pipe buffer.
    // Before this drain, such a command would block on the full pipe — wedging until the timeout
    // (`child.wait()` never completes → falsely reported as a timeout), or dying by SIGPIPE where
    // dropping the read end signals the writer — instead of finishing. A truly runaway/infinite writer
    // keeps the sink busy but never exits, so the outer timeout still fires and kills it (correct); a
    // finite writer drains and exits.
    let read_cap = (output_cap + 1024) as u64;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let drain_out = async {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = (&mut pipe).take(read_cap).read_to_end(&mut buf).await;
            // Discard anything past the cap so the child doesn't block on a full pipe.
            let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
        }
        buf
    };
    let drain_err = async {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = (&mut pipe).take(read_cap).read_to_end(&mut buf).await;
            // Discard anything past the cap so the child doesn't block on a full pipe.
            let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
        }
        buf
    };
    let combined = async { tokio::join!(child.wait(), drain_out, drain_err) };

    let (status, stdout_bytes, stderr_bytes) = match tokio::time::timeout(timeout, combined).await {
        Ok((Ok(status), out, err)) => (status, out, err),
        Ok((Err(error), _, _)) => return format!("error: could not run {command:?}: {error}"),
        Err(_) => {
            // Timed out. Explicitly kill + reap so the process is gone before we return, rather than
            // relying only on `kill_on_drop` firing when `child` drops at end of scope.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return format!(
                "error: {command:?} exceeded the per-command timeout of {}s and was aborted.",
                timeout.as_secs()
            );
        }
    };

    let code = status
        .code()
        .map_or_else(|| "signal".to_string(), |c| c.to_string());
    let stdout = truncate(&String::from_utf8_lossy(&stdout_bytes), output_cap);
    let stderr = truncate(&String::from_utf8_lossy(&stderr_bytes), output_cap);
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

    // A hung command must be actually TERMINATED on timeout, not just reported as one and left to run
    // as an orphan (the pre-fix `timeout(.., cmd.output())` dropped the future without killing the
    // child). Prove it behaviourally: the child would `touch` a marker after a sleep that outlasts the
    // timeout; if it were truly killed the marker never appears, if it leaked it creates the file.
    #[tokio::test]
    async fn kills_the_child_on_timeout_leaving_no_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("sleep 1; touch {}", marker.display());
        let out = run(
            dir.path(),
            "sh",
            &["-c".to_string(), script],
            Duration::from_millis(150),
            1024,
        )
        .await;
        assert!(out.contains("timeout"), "{out}");
        // Wait past the child's sleep. A killed child never reaches `touch`; a leaked orphan would.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "the child survived the timeout and ran `touch` — it was not killed"
        );
    }

    // The discriminating case for the drain fix: a command that writes FAR more than `read_cap` bytes
    // on stdout and then exits promptly. Without draining the excess to `sink()`, the child fills the
    // ~64 KB OS pipe buffer, blocks on write, `child.wait()` never returns, and the outer timeout
    // fires — falsely reporting a fast, benign command as a timeout. With the drain it exits normally
    // and we return success with TRUNCATED output. This test FAILS on the pre-fix (un-drained) version.
    #[tokio::test]
    async fn verbose_but_fast_command_returns_truncated_success_not_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let output_cap = 4096usize; // read_cap = 5120 bytes; the child writes ~1.2 MB, far exceeding it.
        // `yes` would run forever; instead emit a bounded-but-large stream then exit 0 immediately.
        let script = r#"awk 'BEGIN { for (i = 1; i <= 200000; i++) print "line" i }'"#;
        let out = run(
            dir.path(),
            "sh",
            &["-c".to_string(), script.to_string()],
            // A generous timeout: the command is fast, so if we still hit this it's the pipe deadlock,
            // not slowness. Pre-fix this test would report a timeout regardless of how large we set it.
            Duration::from_secs(20),
            output_cap,
        )
        .await;
        assert!(
            !out.contains("exceeded the per-command timeout"),
            "verbose-but-fast command was falsely reported as a timeout (pipe deadlock not drained): {out}"
        );
        assert!(out.contains("exit code: 0"), "expected a clean exit: {out}");
        assert!(
            out.contains("truncated at"),
            "expected truncated stdout: {out}"
        );
        // The captured stdout must be bounded near `output_cap`, not the child's full ~1.2 MB output.
        assert!(
            out.len() < output_cap * 3,
            "captured output was not bounded to roughly output_cap: {} bytes",
            out.len()
        );
    }
}
