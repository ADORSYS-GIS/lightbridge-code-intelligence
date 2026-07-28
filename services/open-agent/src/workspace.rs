//! The sandbox workdir and its **path-safety boundary** (ADR-0088).
//!
//! Every `open` write and read is confined to the sandbox `emptyDir` checkout root. This module owns
//! the one function that enforces it — [`resolve_write`] (and its read sibling [`resolve_read`]) —
//! canonicalizing each target and rejecting anything that escapes the workdir, including:
//!
//! - **`..` traversal** and absolute paths — rejected lexically before touching the filesystem.
//! - **Symlinks that point outside the workdir** — the classic bypass of a naive
//!   `path.starts_with(root)` prefix check. A middle component that is a symlink to `/etc`, or a final
//!   component that is itself an outside-pointing symlink, both resolve (via `canonicalize`) to a path
//!   outside the canonical root and are rejected.
//!
//! The read-only root filesystem in the sandbox Job spec is the deploy-side backstop if this check is
//! ever wrong; this function is the in-process first line.

use std::path::{Component, Path, PathBuf};

use lci_agent_tools::{BoxFuture, Workspace, WorkspaceError};

/// The sandbox checkout root — the pod's writable work `emptyDir`, mounted at the checkout root. The
/// open agent's *entire* writable + readable surface is this directory. The current `run-once` host
/// materializes the checkout before the loop starts, so `root()` resolves eagerly.
#[derive(Clone)]
pub struct SandboxWorkspace(PathBuf);

impl SandboxWorkspace {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }
}

impl Workspace for SandboxWorkspace {
    fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
        Box::pin(async { Ok(self.0.as_path()) })
    }
}

/// Why a sandbox-relative path was rejected, either lexically (before touching the filesystem) or
/// after canonicalizing it against the workdir.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("error: {0:?} must be a path relative to the sandbox workdir (no leading `/`).")]
    Absolute(String),
    #[error("error: {0:?} must not contain `..` (path traversal rejected).")]
    Traversal(String),
    #[error("error: {0:?} is not a valid file path.")]
    NotAFilePath(String),
    #[error("error: sandbox workdir is not accessible.")]
    WorkdirInaccessible,
    #[error("error: could not open {0:?} (file not found or unreadable).")]
    NotFound(String),
    #[error("error: {0:?} resolves outside the sandbox workdir (symlink escape rejected).")]
    ReadEscape(String),
    #[error(
        "error: {0:?} resolves outside the sandbox workdir (symlink/traversal escape rejected)."
    )]
    WriteEscape(String),
}

/// Lexically reject the two escapes that need no filesystem lookup: an absolute path (`/etc/passwd`)
/// and any `..` component (path traversal). Returns the cleaned, workdir-relative path.
fn lexical_clean(rel: &str) -> Result<PathBuf, PathError> {
    let mut cleaned = PathBuf::new();
    for component in Path::new(rel).components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                return Err(PathError::Absolute(rel.to_string()));
            }
            Component::ParentDir => {
                return Err(PathError::Traversal(rel.to_string()));
            }
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
        }
    }
    if cleaned.as_os_str().is_empty() {
        return Err(PathError::NotAFilePath(rel.to_string()));
    }
    Ok(cleaned)
}

/// Resolve a path for **reading**: the file must exist, and its fully-canonicalized real path must lie
/// within the canonical workdir. Rejects `..`, absolute paths, and symlink escapes.
pub fn resolve_read(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let cleaned = lexical_clean(rel)?;
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| PathError::WorkdirInaccessible)?;
    let canonical = std::fs::canonicalize(canonical_root.join(&cleaned))
        .map_err(|_| PathError::NotFound(rel.to_string()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(PathError::ReadEscape(rel.to_string()));
    }
    Ok(canonical)
}

/// Resolve a path for **writing**: the target may not exist yet, so we canonicalize the deepest
/// *existing* ancestor and require it to stay within the canonical workdir. This is what defeats the
/// symlink bypass: a parent directory that is a symlink to `/etc` canonicalizes to a path outside the
/// workdir (a naive `target.starts_with(root)` on the un-canonicalized join would wrongly pass), and a
/// final component that is itself an outside-pointing symlink canonicalizes to its target and is caught
/// the same way. Returns the concrete path to write (rooted at the canonical workdir).
pub fn resolve_write(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let cleaned = lexical_clean(rel)?;
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| PathError::WorkdirInaccessible)?;
    let target = canonical_root.join(&cleaned);

    // Walk up to the deepest ancestor that exists on disk and canonicalize it. `canonicalize` follows
    // every symlink in the path, so a symlinked ancestor that escapes the workdir surfaces here.
    let mut probe: &Path = target.as_path();
    let real_existing = loop {
        match std::fs::canonicalize(probe) {
            Ok(resolved) => break resolved,
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                // We started from `canonical_root.join(...)`, so the root itself always canonicalizes;
                // reaching here would mean the workdir vanished mid-call.
                None => return Err(PathError::WorkdirInaccessible),
            },
        }
    };
    if !real_existing.starts_with(&canonical_root) {
        return Err(PathError::WriteEscape(rel.to_string()));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_absolute_paths_are_rejected_lexically() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            resolve_write(root.path(), "../escape")
                .unwrap_err()
                .to_string()
                .contains("traversal")
        );
        assert!(
            resolve_write(root.path(), "a/../../escape")
                .unwrap_err()
                .to_string()
                .contains("traversal")
        );
        assert!(resolve_write(root.path(), "/etc/passwd").is_err());
        assert!(resolve_read(root.path(), "../secret").is_err());
    }

    #[test]
    fn plain_relative_write_target_resolves_within_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let resolved = resolve_write(root.path(), "src/new/file.rs").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        assert!(resolved.starts_with(&canonical_root));
        assert!(resolved.ends_with("src/new/file.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_a_symlinked_parent_that_escapes_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // `dir` inside the workdir is a symlink to a directory OUTSIDE it. Writing `dir/evil` would
        // escape via the symlink — the naive prefix check on the un-canonicalized join would pass.
        symlink(outside.path(), root.path().join("dir")).unwrap();
        let err = resolve_write(root.path(), "dir/evil").unwrap_err().to_string();
        assert!(err.contains("escape"), "unexpected: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_a_final_symlink_that_escapes_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "keep-me").unwrap();
        // A file inside the workdir that is itself a symlink pointing outside: writing through it would
        // clobber the outside file.
        symlink(&secret, root.path().join("alias.txt")).unwrap();
        let err = resolve_write(root.path(), "alias.txt").unwrap_err().to_string();
        assert!(err.contains("escape"), "unexpected: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn in_workdir_symlink_is_allowed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        // A symlink that stays INSIDE the workdir is fine — it resolves within the canonical root.
        symlink(root.path().join("real"), root.path().join("link")).unwrap();
        assert!(resolve_write(root.path(), "link/file.rs").is_ok());
    }

    #[test]
    fn read_requires_existence_within_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("here.txt"), "hi").unwrap();
        assert!(resolve_read(root.path(), "here.txt").is_ok());
        assert!(
            resolve_read(root.path(), "missing.txt")
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }
}
