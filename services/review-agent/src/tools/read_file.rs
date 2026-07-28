use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use super::parse;

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
        "Read a UTF-8 text file from the checked-out repository (the working tree under review). Path is relative to the repo root; absolute paths and `..` traversal are rejected. Returns up to 64 KiB; pass `start_line`/`end_line` (1-based, inclusive) to read a slice. Use this to look at the actual source when the search/graph tools come up empty.",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the repo root (no leading `/`, no `..`)."},"start_line":{"type":"integer","description":"Optional 1-based first line to return (inclusive)."},"end_line":{"type":"integer","description":"Optional 1-based last line to return (inclusive)."}},"required":["path"]}),
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
            ToolOutcome::Continue(read(root, &args.path, args.start_line, args.end_line).await)
        })
    }
}

async fn read(
    root: &Path,
    rel: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> String {
    let resolved = match resolve(root, rel) {
        Ok(path) => path,
        Err(error) => return error.to_string(),
    };
    let canonical_root = match tokio::fs::canonicalize(root).await {
        Ok(path) => path,
        Err(_) => return format!("error: could not open {rel:?} (file not found or unreadable)."),
    };
    let canonical = match tokio::fs::canonicalize(&resolved).await {
        Ok(path) => path,
        Err(_) => return format!("error: could not open {rel:?} (file not found or unreadable)."),
    };
    if !canonical.starts_with(&canonical_root) {
        return format!(
            "error: {rel:?} resolves outside the repository (symlink escape rejected)."
        );
    }
    let Ok(file) = tokio::fs::File::open(&canonical).await else {
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
            if over_cap {
                format!(
                    "{rel} lines {start}-{last} of a file truncated at {READ_FILE_CAP} bytes (read past the cap to see more is not possible):\n{slice}"
                )
            } else {
                format!("{rel} lines {start}-{last} (of {total}):\n{slice}")
            }
        }
    }
}

/// Why a `path` argument to `read_file` was rejected before ever touching the filesystem.
#[derive(Debug, thiserror::Error)]
enum ResolveError {
    #[error("error: {0:?} must be a path relative to the repo root (no leading `/`).")]
    Absolute(String),
    #[error("error: {0:?} must not contain `..` (path traversal).")]
    Traversal(String),
    #[error("error: {0:?} is not a file path.")]
    NotAFilePath(String),
}

fn resolve(root: &Path, rel: &str) -> Result<PathBuf, ResolveError> {
    let mut cleaned = PathBuf::new();
    for component in Path::new(rel).components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                return Err(ResolveError::Absolute(rel.to_string()));
            }
            Component::ParentDir => {
                return Err(ResolveError::Traversal(rel.to_string()));
            }
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
        }
    }
    if cleaned.as_os_str().is_empty() {
        return Err(ResolveError::NotAFilePath(rel.to_string()));
    }
    Ok(root.join(cleaned))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_is_rejected() {
        assert!(
            resolve(Path::new("/tmp/root"), "../secret")
                .unwrap_err()
                .to_string()
                .contains("traversal")
        );
        assert!(resolve(Path::new("/tmp/root"), "/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn bounded_utf8_reads_and_slices_cover_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("small"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        assert_eq!(
            read(dir.path(), "small", Some(2), Some(2)).await,
            "small lines 2-2 (of 3):\ntwo"
        );
        assert!(
            read(dir.path(), "small", Some(99), None)
                .await
                .contains("past the end")
        );
        tokio::fs::write(dir.path().join("large"), vec![b'x'; READ_FILE_CAP + 8])
            .await
            .unwrap();
        assert!(
            read(dir.path(), "large", None, None)
                .await
                .contains("truncated")
        );
        assert!(
            read(dir.path(), "large", Some(1), Some(1))
                .await
                .contains("truncated at")
        );
        tokio::fs::write(dir.path().join("utf8"), [b'a', 0xf0, 0x9f])
            .await
            .unwrap();
        assert_eq!(read(dir.path(), "utf8", None, None).await, "a");
        assert!(
            read(dir.path(), "missing", None, None)
                .await
                .starts_with("error:")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = "external-secret-must-not-be-disclosed";
        let secret_path = outside.path().join("secret.txt");
        tokio::fs::write(&secret_path, secret).await.unwrap();
        symlink(&secret_path, repository.path().join("escape.txt")).unwrap();

        let result = read(repository.path(), "escape.txt", None, None).await;
        assert_eq!(
            result,
            "error: \"escape.txt\" resolves outside the repository (symlink escape rejected)."
        );
        assert!(!result.contains(secret));

        tokio::fs::write(repository.path().join("source.txt"), "safe in-repo content")
            .await
            .unwrap();
        symlink("source.txt", repository.path().join("alias.txt")).unwrap();
        assert_eq!(
            read(repository.path(), "alias.txt", None, None).await,
            "safe in-repo content"
        );
    }
}
