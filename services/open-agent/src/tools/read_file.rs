//! `read_file` — read a UTF-8 text file from the sandbox workdir (read-only navigation, shared shape
//! with `review`). Path-safety goes through [`crate::workspace::resolve_read`].

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use super::parse;
use crate::workspace::resolve_read;

pub const READ_FILE: &str = "read_file";
const READ_FILE_CAP: usize = 64 * 1024;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        READ_FILE,
        "Read a UTF-8 text file from the sandbox working tree. Path is relative to the workdir root; \
         absolute paths, `..` traversal, and symlinks that escape the workdir are rejected. Returns up \
         to 64 KiB; pass `start_line`/`end_line` (1-based, inclusive) to read a slice.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workdir root (no leading `/`, no `..`)."},"start_line":{"type":"integer","description":"Optional 1-based first line to return (inclusive)."},"end_line":{"type":"integer","description":"Optional 1-based last line to return (inclusive)."}},"required":["path"]}),
    )
}

struct ReadFileTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(ReadFileTool { spec: spec() }), caps)
}

impl Tool for ReadFileTool {
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
                Err(error) => return ToolOutcome::Continue(error),
            };
            let root = match cx.workspace.root().await {
                Ok(root) => root,
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not materialize the sandbox workdir: {error}"
                    ));
                }
            };
            let path = match resolve_read(root, &args.path) {
                Ok(path) => path,
                Err(error) => return ToolOutcome::Continue(error),
            };
            ToolOutcome::Continue(read(&path, &args.path, args.start_line, args.end_line).await)
        })
    }
}

async fn read(
    path: &std::path::Path,
    rel: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> String {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return format!("error: could not open {rel:?} (file not found or unreadable).");
    };
    let mut bytes = Vec::new();
    if file
        .take((READ_FILE_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .is_err()
    {
        return format!("error: could not read {rel:?}.");
    }
    let over_cap = bytes.len() > READ_FILE_CAP;
    bytes.truncate(READ_FILE_CAP);
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
    };
    match (start_line, end_line) {
        (None, None) if over_cap => format!("{content}\n… [truncated at {READ_FILE_CAP} bytes]"),
        (None, None) => content,
        _ => {
            let start = start_line.unwrap_or(1).max(1);
            let end = end_line.unwrap_or(usize::MAX).max(start);
            let lines: Vec<_> = content.lines().collect();
            let total = lines.len();
            if start > total {
                return format!(
                    "error: start_line {start} is past the end of {rel:?} ({total} lines)."
                );
            }
            let last = end.min(total);
            let slice = lines[start - 1..last].join("\n");
            format!("{rel} lines {start}-{last} (of {total}):\n{slice}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_a_slice_within_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let path = resolve_read(dir.path(), "f.rs").unwrap();
        assert_eq!(
            read(&path, "f.rs", Some(2), Some(2)).await,
            "f.rs lines 2-2 (of 3):\ntwo"
        );
    }
}
