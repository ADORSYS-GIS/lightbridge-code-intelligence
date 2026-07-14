//! Thin wrappers around plain cargo workspace commands.

use crate::process::run;

pub(crate) fn fmt() -> anyhow::Result<()> {
    run("cargo", &["fmt", "--all"])
}

pub(crate) fn lint() -> anyhow::Result<()> {
    run(
        "cargo",
        &["clippy", "--all-targets", "--", "-D", "warnings"],
    )
}

pub(crate) fn build() -> anyhow::Result<()> {
    run("cargo", &["build", "--workspace", "--all-targets"])
}
