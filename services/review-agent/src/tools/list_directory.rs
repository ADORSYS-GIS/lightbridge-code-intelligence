//! `list_directory` — non-recursive directory listing (ADR-0104's fs-tool suite).
//!
//! Not a glob/search tool — ADR-0104 names `GLOB`/`SEARCH_FILES` as a separate, optional tool "if a
//! mode needs it"; this is the simpler "what's in this one directory" primitive. **Not registered for
//! any review preset today** — see `tools.rs`'s `fs_write` opt-in flag.

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::fs_safety::resolve_read;
use super::parse;

pub const LIST_DIRECTORY: &str = "list_directory";
/// Cap on returned entries — mirrors `read_file`'s `READ_FILE_CAP` never-truncate-silently convention:
/// excess is disclosed, not dropped.
const LIST_DIRECTORY_CAP: usize = 1000;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    path: Option<String>,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        LIST_DIRECTORY,
        "List the immediate entries (files and subdirectories) of a directory in the checked-out \
         repository. Non-recursive. Path is relative to the repo root and defaults to the repo root \
         itself; absolute paths, `..` traversal, and symlinks that escape the checkout are rejected.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path relative to the repo root (no leading `/`, no `..`). Defaults to the repo root."}}}),
    )
}

struct ListDirectoryTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(ListDirectoryTool { spec: spec() }), caps)
}

impl Tool for ListDirectoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly(ReadKind::File)
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::ReadOnly
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
            let rel = args.path.as_deref().unwrap_or(".");
            ToolOutcome::Continue(list(root, rel).await)
        })
    }
}

async fn list(root: &std::path::Path, rel: &str) -> String {
    // "." is the repo root itself — resolve_read requires an existing target, and the root always
    // exists, but its own lexical_clean rejects an empty cleaned path; special-case it here.
    let target = if rel == "." {
        match tokio::fs::canonicalize(root).await {
            Ok(path) => path,
            Err(_) => return "error: could not materialize the repository checkout.".to_string(),
        }
    } else {
        match resolve_read(root, rel) {
            Ok(path) => path,
            Err(error) => return error.to_string(),
        }
    };
    let mut entries = match tokio::fs::read_dir(&target).await {
        Ok(entries) => entries,
        Err(error) => return format!("error: could not list {rel:?}: {error}"),
    };
    let mut names: Vec<(String, bool)> = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
                names.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
            }
            Ok(None) => break,
            Err(error) => return format!("error: could not list {rel:?}: {error}"),
        }
    }
    names.sort();
    let total = names.len();
    let over_cap = total > LIST_DIRECTORY_CAP;
    names.truncate(LIST_DIRECTORY_CAP);
    let rendered = names
        .into_iter()
        .map(|(name, is_dir)| if is_dir { format!("{name}/") } else { name })
        .collect::<Vec<_>>()
        .join("\n");
    if over_cap {
        format!(
            "{rel} ({total} entries, showing first {LIST_DIRECTORY_CAP}):\n{rendered}\n… [{} more not shown]",
            total - LIST_DIRECTORY_CAP
        )
    } else if total == 0 {
        format!("{rel} is empty.")
    } else {
        format!("{rel} ({total} entries):\n{rendered}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_immediate_entries_sorted_with_dir_suffix() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "").await.unwrap();
        tokio::fs::create_dir(dir.path().join("a_dir"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("c.rs"), "").await.unwrap();
        let result = list(dir.path(), ".").await;
        assert_eq!(result, ". (3 entries):\na_dir/\nb.rs\nc.rs");
    }

    #[tokio::test]
    async fn lists_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
        tokio::fs::write(dir.path().join("src/main.rs"), "")
            .await
            .unwrap();
        let result = list(dir.path(), "src").await;
        assert_eq!(result, "src (1 entries):\nmain.rs");
    }

    #[tokio::test]
    async fn empty_directory_is_disclosed_not_silent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list(dir.path(), ".").await, ". is empty.");
    }

    #[tokio::test]
    async fn list_directory_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path(), "../etc").await.starts_with("error:"));
    }

    #[tokio::test]
    async fn missing_directory_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path(), "nope").await.starts_with("error:"));
    }
}
