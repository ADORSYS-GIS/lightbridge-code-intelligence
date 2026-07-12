//! Configurable ignore-list (ADR-0086 §4). Replaces the hardcoded `matches!` dir set in
//! `agent-runner/src/indexer/mod.rs` with **gitignore-style globs**, operator-configurable, with the
//! historical junk dirs as built-in defaults.
//!
//! This is the *operator layer* only. It **composes with, does not replace** the repo's own
//! `.gitignore`: [`crate::walk`] drives the file walk with ripgrep's `ignore::WalkBuilder`
//! (`git_ignore(true)`), which honours the repo's nested `.gitignore` files natively, and applies
//! this list as an *additional* filter for junk that slipped past the repo's rules (ADR-0086 R3).
//! Every skip is logged so a too-broad operator glob is diagnosable, not silent.

use std::path::{Path, PathBuf};

use anyhow::Context;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The built-in junk-dir defaults (ADR-0086 §4). Gitignore syntax: a trailing `/` matches a
/// directory of that name at any depth. `.next/` and `venv/` are kept from the original hardcoded set
/// so this is behaviour-preserving for the paths it already skipped.
pub const DEFAULT_IGNORE_GLOBS: &[&str] = &[
    "target/",
    "node_modules/",
    ".git/",
    "dist/",
    "build/",
    "vendor/",
    ".venv/",
    "venv/",
    ".next/",
    "__pycache__/",
];

/// Env var holding operator-supplied extra globs, one per line or comma-separated.
pub const ENV_EXTRA_GLOBS: &str = "LCI_CODEGRAPH_IGNORE_GLOBS";

/// Configuration for the operator ignore layer. Uses a `bon` builder (≥3 fields, ADR-0083 idiom).
#[derive(Debug, Clone, bon::Builder)]
pub struct IgnoreConfig {
    /// Repo root the gitignore-style globs are anchored to.
    #[builder(into)]
    pub root: PathBuf,
    /// Operator-configurable extra globs (gitignore syntax), layered **on top of** the defaults.
    #[builder(default)]
    pub extra_globs: Vec<String>,
    /// Include the built-in [`DEFAULT_IGNORE_GLOBS`]. Defaults to `true`; an operator can disable the
    /// defaults and supply their own set entirely.
    #[builder(default = true)]
    pub include_defaults: bool,
}

/// A compiled operator ignore matcher.
#[derive(Debug)]
pub struct IgnoreList {
    matcher: Gitignore,
    root: PathBuf,
}

impl IgnoreList {
    /// Compile the operator layer from `config`. Fails only if a supplied glob is unparseable.
    pub fn build(config: &IgnoreConfig) -> anyhow::Result<Self> {
        let mut builder = GitignoreBuilder::new(&config.root);
        if config.include_defaults {
            for glob in DEFAULT_IGNORE_GLOBS {
                builder
                    .add_line(None, glob)
                    .with_context(|| format!("compiling default ignore glob {glob:?}"))?;
            }
        }
        for glob in &config.extra_globs {
            let glob = glob.trim();
            if glob.is_empty() || glob.starts_with('#') {
                continue;
            }
            builder
                .add_line(None, glob)
                .with_context(|| format!("compiling operator ignore glob {glob:?}"))?;
        }
        let matcher = builder
            .build()
            .context("building operator ignore matcher")?;
        Ok(Self {
            matcher,
            root: config.root.clone(),
        })
    }

    /// Build the operator layer for `root`, reading extra globs from [`ENV_EXTRA_GLOBS`] and keeping
    /// the built-in defaults.
    pub fn from_env(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let extra = std::env::var(ENV_EXTRA_GLOBS)
            .ok()
            .map(|raw| {
                raw.split(['\n', ','])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let config = IgnoreConfig::builder()
            .root(root)
            .extra_globs(extra)
            .build();
        Self::build(&config)
    }

    /// True when `path` is ignored by the operator layer. `is_dir` selects directory-only patterns
    /// (a trailing-slash glob only matches directories). `path` may be absolute (under root) or
    /// relative to root.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // `Gitignore::matched` wants a path relative to (or absolute under) the root; normalise so a
        // caller passing either form gets a correct answer.
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        self.matcher.matched(rel, is_dir).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn list(extra: &[&str]) -> IgnoreList {
        let config = IgnoreConfig::builder()
            .root("/repo")
            .extra_globs(extra.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
            .build();
        IgnoreList::build(&config).unwrap()
    }

    #[test]
    fn defaults_skip_the_historical_junk_dirs() {
        let l = list(&[]);
        for dir in [
            "target",
            "node_modules",
            ".git",
            "dist",
            "build",
            "vendor",
            ".venv",
            "__pycache__",
        ] {
            assert!(
                l.is_ignored(Path::new(dir), true),
                "{dir} should be ignored"
            );
        }
    }

    #[test]
    fn defaults_ignore_nested_dirs_at_any_depth() {
        let l = list(&[]);
        assert!(l.is_ignored(Path::new("crate/sub/node_modules"), true));
        assert!(l.is_ignored(Path::new("/repo/a/target"), true));
    }

    #[test]
    fn source_files_are_not_ignored() {
        let l = list(&[]);
        assert!(!l.is_ignored(Path::new("src/main.rs"), false));
        assert!(!l.is_ignored(Path::new("src"), true));
    }

    #[test]
    fn operator_globs_layer_on_top_of_defaults() {
        let l = list(&["*.min.js", "generated/"]);
        assert!(
            l.is_ignored(Path::new("web/app.min.js"), false),
            "operator file glob"
        );
        assert!(
            l.is_ignored(Path::new("generated"), true),
            "operator dir glob"
        );
        // Defaults still apply alongside the operator globs (composition, not replacement).
        assert!(l.is_ignored(Path::new("target"), true));
        assert!(!l.is_ignored(Path::new("web/app.js"), false));
    }

    #[test]
    fn defaults_can_be_disabled() {
        let config = IgnoreConfig::builder()
            .root("/repo")
            .extra_globs(vec!["only-this/".to_string()])
            .include_defaults(false)
            .build();
        let l = IgnoreList::build(&config).unwrap();
        assert!(l.is_ignored(Path::new("only-this"), true));
        assert!(
            !l.is_ignored(Path::new("target"), true),
            "defaults disabled"
        );
    }

    #[test]
    fn blank_and_comment_globs_are_ignored() {
        // Should not error and should not add spurious matches.
        let l = list(&["", "  ", "# a comment"]);
        assert!(!l.is_ignored(Path::new("src/main.rs"), false));
    }
}
