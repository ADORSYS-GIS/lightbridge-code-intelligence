//! `edit_file` — the open agent's file **write** tool (ADR-0088's `apply_patch`/`edit_file`).
//!
//! Writes are confined to the sandbox workdir: every target goes through
//! [`crate::workspace::resolve_write`], which canonicalizes the path and **rejects `..` traversal AND
//! symlinks that point outside the workdir** — the classic bypass of a naive prefix check. The
//! read-only root filesystem in the sandbox Job spec is the deploy-side backstop if this check is ever
//! wrong. (Applying a unified diff is available to the agent through `run_command` — `git apply` —
//! under the same sandbox posture; `edit_file` is the direct create/overwrite primitive.)

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{parse, resolve_root};
use crate::workspace::resolve_write;

/// The write tool's model-facing name. ADR-0088 calls the write tool `apply_patch`/`edit_file`; we
/// expose the create/overwrite primitive under `edit_file`.
pub const EDIT_FILE: &str = "edit_file";

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        EDIT_FILE,
        "Create or overwrite a text file in the sandbox working tree with the given full content. \
         Path is relative to the workdir root; absolute paths, `..` traversal, and symlinks that \
         escape the workdir are rejected. Parent directories are created as needed. To apply a unified \
         diff instead, use run_command with `git apply`.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workdir root (no leading `/`, no `..`)."},"content":{"type":"string","description":"The full new contents of the file."}},"required":["path","content"]}),
    )
}

struct EditFileTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(EditFileTool { spec: spec() }), caps)
}

impl Tool for EditFileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        // Writing the full content is deterministic: a replay writes the same bytes to the same path.
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<Args>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => return ToolOutcome::Continue(error),
            };
            let root = match resolve_root(cx).await {
                Ok(root) => root,
                Err(error) => return ToolOutcome::Continue(error),
            };
            // The path-safety boundary: reject `..` + out-of-workdir symlink escapes (ADR-0088).
            let target = match resolve_write(root, &args.path) {
                Ok(target) => target,
                Err(error) => return ToolOutcome::Continue(error),
            };
            ToolOutcome::Continue(write(&target, &args.path, &args.content).await)
        })
    }
}

async fn write(target: &std::path::Path, rel: &str, content: &str) -> String {
    if let Some(parent) = target.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return format!("error: could not create parent directories for {rel:?}: {error}");
    }
    match tokio::fs::write(target, content).await {
        Ok(()) => format!("wrote {} bytes to {rel}", content.len()),
        Err(error) => format!("error: could not write {rel:?}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_tools::ToolCx;
    use uuid::Uuid;

    fn call(args: &str) -> ToolCallReq {
        ToolCallReq {
            id: "c1".into(),
            kind: "function".into(),
            function: lci_agent_types::FunctionCallReq {
                name: EDIT_FILE.into(),
                arguments: args.into(),
            },
            extra_content: None,
        }
    }

    #[tokio::test]
    async fn writes_a_new_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve_write(dir.path(), "src/new/mod.rs").unwrap();
        let msg = write(&target, "src/new/mod.rs", "pub fn x() {}").await;
        assert!(msg.contains("wrote"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new/mod.rs")).unwrap(),
            "pub fn x() {}"
        );
    }

    // Merge bar (ADR-0088): edit_file rejects `..` traversal at the tool boundary.
    #[tokio::test]
    async fn edit_file_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::workspace::SandboxWorkspace::new(dir.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let tool = EditFileTool { spec: spec() };
        let outcome = tool
            .call(&cx, &call(r#"{"path":"../escape.txt","content":"x"}"#))
            .await;
        assert!(
            matches!(&outcome, ToolOutcome::Continue(m) if m.contains("traversal")),
            "{outcome:?}"
        );
        // Nothing was written outside the workdir.
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    // Merge bar (ADR-0088): edit_file rejects a symlink whose parent escapes the workdir — the classic
    // prefix-check bypass. Without canonicalizing the parent, the write would land outside.
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_rejects_out_of_workdir_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("dir")).unwrap();
        let workspace = crate::workspace::SandboxWorkspace::new(dir.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let tool = EditFileTool { spec: spec() };
        let outcome = tool
            .call(&cx, &call(r#"{"path":"dir/evil.txt","content":"pwned"}"#))
            .await;
        assert!(
            matches!(&outcome, ToolOutcome::Continue(m) if m.contains("escape")),
            "{outcome:?}"
        );
        assert!(
            !outside.path().join("evil.txt").exists(),
            "the write must not have escaped the workdir via the symlink"
        );
    }
}
