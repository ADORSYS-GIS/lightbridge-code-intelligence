//! Shared path-safety boundary for the fs-tool suite (ADR-0104): [`resolve_read`] and [`resolve_write`]
//! canonicalize a tool's `path` argument and reject anything that escapes the checkout root, including
//! symlink escapes a naive `path.starts_with(root)` prefix check would miss.
//!
//! Ported from `services/open-agent/src/workspace.rs` (`open` mode's sandbox path-safety module,
//! battle-tested with symlink-escape tests) rather than reimplemented — `read_file.rs`'s own inline
//! `resolve()` only ever needed the lexical (absolute/`..`) check because it always operates on an
//! existing file; [`resolve_write`]'s "walk up to the deepest existing ancestor and canonicalize that"
//! technique is the one that correctly handles a not-yet-existing write target while still catching a
//! symlinked ancestor that escapes the checkout. No shared crate exists between `review-agent` and
//! `open-agent` today (creating one is out of scope for this story), so this is a deliberate port, not a
//! shared dependency — keep the two in sync by hand if either evolves.

use std::path::{Component, Path, PathBuf};

/// Why a `path` argument was rejected, either lexically (before touching the filesystem) or after
/// canonicalizing it against the checkout root.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PathError {
    #[error("error: {0:?} must be a path relative to the repo root (no leading `/`).")]
    Absolute(String),
    #[error("error: {0:?} must not contain `..` (path traversal rejected).")]
    Traversal(String),
    #[error("error: {0:?} is not a valid file path.")]
    NotAFilePath(String),
    #[error("error: could not materialize the repository checkout.")]
    WorkdirInaccessible,
    #[error("error: could not open {0:?} (file not found or unreadable).")]
    NotFound(String),
    #[error("error: {0:?} resolves outside the repository (symlink escape rejected).")]
    ReadEscape(String),
    #[error("error: {0:?} resolves outside the repository (symlink/traversal escape rejected).")]
    WriteEscape(String),
}

/// Lexically reject the two escapes that need no filesystem lookup: an absolute path (`/etc/passwd`)
/// and any `..` component (path traversal). Returns the cleaned, root-relative path.
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

/// Resolve a path for **reading or listing**: the target must exist, and its fully-canonicalized real
/// path must lie within the canonical checkout root. Rejects `..`, absolute paths, and symlink escapes.
pub(crate) fn resolve_read(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let cleaned = lexical_clean(rel)?;
    let canonical_root = std::fs::canonicalize(root).map_err(|_| PathError::WorkdirInaccessible)?;
    let canonical = std::fs::canonicalize(canonical_root.join(&cleaned))
        .map_err(|_| PathError::NotFound(rel.to_string()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(PathError::ReadEscape(rel.to_string()));
    }
    Ok(canonical)
}

/// Resolve a path for **writing**: the target may not exist yet, so we canonicalize the deepest
/// *existing* ancestor and require it to stay within the canonical checkout root. This is what defeats
/// the symlink bypass: a parent directory that is a symlink to `/etc` canonicalizes to a path outside
/// the root (a naive `target.starts_with(root)` on the un-canonicalized join would wrongly pass), and a
/// final component that is itself an outside-pointing symlink canonicalizes to its target and is caught
/// the same way. Returns the concrete path to write (rooted at the canonical checkout root).
pub(crate) fn resolve_write(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    let cleaned = lexical_clean(rel)?;
    let canonical_root = std::fs::canonicalize(root).map_err(|_| PathError::WorkdirInaccessible)?;
    let target = canonical_root.join(&cleaned);

    // Walk up to the deepest ancestor that exists on disk and canonicalize it. `canonicalize` follows
    // every symlink in the path, so a symlinked ancestor that escapes the root surfaces here.
    let mut probe: &Path = target.as_path();
    let real_existing = loop {
        match std::fs::canonicalize(probe) {
            Ok(resolved) => break resolved,
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                // We started from `canonical_root.join(...)`, so the root itself always canonicalizes;
                // reaching here would mean the checkout vanished mid-call.
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
        assert!(resolve_write(root.path(), "/etc/passwd").is_err());
        assert!(resolve_read(root.path(), "../secret").is_err());
    }

    #[test]
    fn plain_relative_write_target_resolves_within_the_root() {
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
        symlink(&secret, root.path().join("alias.txt")).unwrap();
        let err = resolve_write(root.path(), "alias.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escape"), "unexpected: {err}");
    }

    #[test]
    fn read_requires_existence_within_the_root() {
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
