//! The sandbox-scoped file walk shared by [`super::find_files`] and [`super::grep`] — the one place
//! that owns "never leave the workdir" for a directory traversal: it never follows symlinks (a
//! symlinked directory is treated as a leaf and not descended, so the walk cannot escape the workdir the
//! same way [`crate::workspace`] guards single-path resolution) and it skips `.git`.

use std::fs::Metadata;
use std::ops::ControlFlow;
use std::path::Path;

/// Depth-first walk of `root`, visiting each regular file's workdir-relative path and metadata.
/// `visit` returns [`ControlFlow::Break`] to stop the walk early (e.g. once a result cap is hit).
pub(crate) fn walk_files(root: &Path, mut visit: impl FnMut(&Path, &Metadata) -> ControlFlow<()>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata` does NOT follow the link, so a symlinked directory is treated as a
            // leaf and never descended — the walk cannot leave the workdir.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) != Some(".git") {
                    stack.push(path);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if visit(&path, &meta).is_break() {
                return;
            }
        }
    }
}
