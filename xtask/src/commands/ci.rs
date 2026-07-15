//! The `ci` subcommand: orchestrates the other subcommands into the full local Rust gate.

use crate::process::run;

use super::{dependency_hygiene, schema, test};

/// The full local Rust gate: schema check, format check, clippy, then tests.
pub(crate) fn ci() -> anyhow::Result<()> {
    schema::validate_schema()?;
    dependency_hygiene::dependency_hygiene()?;
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &["clippy", "--all-targets", "--", "-D", "warnings"],
    )?;
    test::test()
}
