//! `find_files` — list working-tree paths whose name contains a substring. Read-only navigation,
//! sandbox-scoped, never following directory symlinks out of the workdir.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::walk::walk_files;
use super::{parse, resolve_root};

pub const FIND_FILES: &str = "find_files";
const MAX_RESULTS: usize = 200;

#[derive(Deserialize)]
struct Args {
    name_contains: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        FIND_FILES,
        "List files in the sandbox working tree whose relative path contains the given substring \
         (up to 200). Confined to the workdir; directory symlinks out of it are not followed.",
        serde_json::json!({"type":"object","properties":{"name_contains":{"type":"string","description":"Substring to match against each file's workdir-relative path."}},"required":["name_contains"]}),
    )
}

struct FindFilesTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(FindFilesTool { spec: spec() }), caps)
}

impl Tool for FindFilesTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly(ReadKind::Retrieval)
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
            let root = match resolve_root(cx).await {
                Ok(root) => root.to_path_buf(),
                Err(error) => return ToolOutcome::Continue(error.to_string()),
            };
            let needle = args.name_contains.clone();
            let found = tokio::task::spawn_blocking(move || walk(&root, &needle))
                .await
                .unwrap_or_default();
            if found.is_empty() {
                ToolOutcome::Continue(format!("No files match {:?}.", args.name_contains))
            } else {
                ToolOutcome::Continue(found.join("\n"))
            }
        })
    }
}

fn walk(root: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    walk_files(root, |path, _meta| {
        if found.len() >= MAX_RESULTS {
            return ControlFlow::Break(());
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        if rel.contains(needle) {
            found.push(rel);
        }
        ControlFlow::Continue(())
    });
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_relative_path_substring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let found = walk(dir.path(), ".rs");
        assert_eq!(found, vec!["src/main.rs".to_string()]);
    }
}
