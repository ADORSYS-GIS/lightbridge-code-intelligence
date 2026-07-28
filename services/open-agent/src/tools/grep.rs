//! `grep` — a bounded, sandbox-scoped literal/substring search over the working tree. Read-only
//! navigation; no regex engine (keeps it dependency-free and predictable). Walks only within the
//! workdir and never follows symlinks out of it.

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

pub const GREP: &str = "grep";
const MAX_HITS: usize = 100;
const MAX_FILE_BYTES: u64 = 512 * 1024;

#[derive(Deserialize)]
struct Args {
    query: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        GREP,
        "Search the sandbox working tree for a literal substring. Returns up to 100 matching \
         `path:line: text` hits. Confined to the workdir; symlinks out of it are not followed.",
        serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Literal substring to search for."}},"required":["query"]}),
    )
}

struct GrepTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(GrepTool { spec: spec() }), caps)
}

impl Tool for GrepTool {
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
            if args.query.is_empty() {
                return ToolOutcome::Continue("error: query must not be empty.".into());
            }
            let root = match resolve_root(cx).await {
                Ok(root) => root.to_path_buf(),
                Err(error) => return ToolOutcome::Continue(error.to_string()),
            };
            let query = args.query.clone();
            let hits = tokio::task::spawn_blocking(move || search(&root, &query))
                .await
                .unwrap_or_default();
            if hits.is_empty() {
                ToolOutcome::Continue(format!("No matches for {:?}.", args.query))
            } else {
                ToolOutcome::Continue(hits.join("\n"))
            }
        })
    }
}

/// Walk the workdir (via [`walk_files`], which never follows directory symlinks) and collect literal
/// matches. Bounded by [`MAX_HITS`] and a per-file byte cap so a huge/binary file can't blow the budget.
fn search(root: &Path, query: &str) -> Vec<String> {
    let mut hits = Vec::new();
    walk_files(root, |path, meta| {
        if hits.len() >= MAX_HITS {
            return ControlFlow::Break(());
        }
        if meta.len() > MAX_FILE_BYTES {
            return ControlFlow::Continue(());
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return ControlFlow::Continue(()); // binary / non-UTF8 → skip
        };
        let rel = path.strip_prefix(root).unwrap_or(path);
        for (index, line) in content.lines().enumerate() {
            if line.contains(query) {
                hits.push(format!("{}:{}: {}", rel.display(), index + 1, line.trim()));
                if hits.len() >= MAX_HITS {
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literal_matches_and_skips_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "needle").unwrap();
        let hits = search(dir.path(), "needle");
        assert_eq!(hits.len(), 1, "the .git tree is skipped: {hits:?}");
        assert!(hits[0].contains("a.rs:2:"));
    }
}
