//! The `test` subcommand.

use crate::process::run;

/// Prefer cargo-nextest; fall back to `cargo test` if it is not installed.
pub(crate) fn test() -> anyhow::Result<()> {
    run("cargo", &["nextest", "run"]).or_else(|_| run("cargo", &["test"]))
}
