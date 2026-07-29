//! `write_file` — create-or-overwrite a whole file (ADR-0104's fs-tool suite).
//!
//! **Not registered for any review preset today** — see `tools.rs`'s `fs_write` opt-in flag. This tool
//! exists so a future consumer (`open` mode, once it migrates onto this shared fs-tool family; a
//! preset that deliberately wants write access) can register it without a new implementation, per
//! ADR-0104. Write access is confined to the checkout via [`super::fs_safety::resolve_write`] —
//! canonicalizes the path and rejects `..` traversal and symlink escapes.

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::fs_safety::resolve_write;
use super::parse;

pub const WRITE_FILE: &str = "write_file";

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        WRITE_FILE,
        "Create or overwrite a text file in the checked-out repository with the given full content. \
         Path is relative to the repo root; absolute paths, `..` traversal, and symlinks that escape \
         the checkout are rejected. Parent directories are created as needed.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the repo root (no leading `/`, no `..`)."},"content":{"type":"string","description":"The full new contents of the file."}},"required":["path","content"]}),
    )
}

struct WriteFileTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(WriteFileTool { spec: spec() }), caps)
}

impl Tool for WriteFileTool {
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
            let target = match resolve_write(root, &args.path) {
                Ok(target) => target,
                Err(error) => return ToolOutcome::Continue(error.to_string()),
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

    #[tokio::test]
    async fn overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old").unwrap();
        let target = resolve_write(dir.path(), "f.txt").unwrap();
        write(&target, "f.txt", "new").await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_write(dir.path(), "../escape.txt").is_err());
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_out_of_workdir_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("dir")).unwrap();
        let err = resolve_write(dir.path(), "dir/evil.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escape"), "{err}");
        assert!(!outside.path().join("evil.txt").exists());
    }
}
