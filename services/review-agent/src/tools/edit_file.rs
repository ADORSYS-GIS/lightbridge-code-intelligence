//! `edit_file` — whole-file replace of an EXISTING file (ADR-0104's fs-tool suite).
//!
//! ADR-0104 names `WRITE_FILE`/`EDIT_FILE` as separate tools without specifying how they differ (an
//! implementation-time decision, flagged in this story's PR). Resolved here as: `write_file`
//! (`write_file.rs`) is the create-or-overwrite primitive; `edit_file` requires the target to already
//! exist — "edit" implies something to edit — and otherwise does the same whole-file replace. Neither
//! does line-based patching; that's `run_command` + a diff-apply command, out of scope here (mirrors
//! `open` mode's existing `apply_patch.rs` precedent, which documents the same split).
//!
//! **Not registered for any review preset today** — see `tools.rs`'s `fs_write` opt-in flag.

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::fs_safety::resolve_read;
use super::parse;

pub const EDIT_FILE: &str = "edit_file";

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        EDIT_FILE,
        "Replace the full content of an EXISTING text file in the checked-out repository. Errors if \
         the file doesn't exist yet — use write_file to create a new one. Path is relative to the repo \
         root; absolute paths, `..` traversal, and symlinks that escape the checkout are rejected.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the repo root (no leading `/`, no `..`). Must already exist."},"content":{"type":"string","description":"The full new contents of the file."}},"required":["path","content"]}),
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
                Err(error) => return ToolOutcome::Continue(error.to_string()),
            };
            let root = match cx.workspace.root().await {
                Ok(root) => root,
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not materialize the repository checkout: {error}"
                    ));
                }
            };
            // `resolve_read`, not `resolve_write`: edit_file requires the file to already exist —
            // resolve_read's existence check + canonicalize-and-compare gives us that for free.
            let target = match resolve_read(root, &args.path) {
                Ok(target) => target,
                Err(error) => return ToolOutcome::Continue(error.to_string()),
            };
            ToolOutcome::Continue(edit(&target, &args.path, &args.content).await)
        })
    }
}

async fn edit(target: &std::path::Path, rel: &str, content: &str) -> String {
    match tokio::fs::write(target, content).await {
        Ok(()) => format!("wrote {} bytes to {rel}", content.len()),
        Err(error) => format!("error: could not write {rel:?}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old").unwrap();
        let target = super::super::fs_safety::resolve_read(dir.path(), "f.txt").unwrap();
        let msg = edit(&target, "f.txt", "new").await;
        assert!(msg.contains("wrote"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn edit_file_errors_when_the_target_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_read(dir.path(), "missing.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn edit_file_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_read(dir.path(), "../escape.txt").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "keep-me").unwrap();
        symlink(&secret, dir.path().join("alias.txt")).unwrap();
        let err = resolve_read(dir.path(), "alias.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escape"), "{err}");
    }
}
