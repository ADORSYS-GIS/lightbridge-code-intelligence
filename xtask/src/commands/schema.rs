//! The `validate-schema` subcommand.

use crate::process::{on_path, run};

const SCHEMA: &str = "services/control-plane/schema/control-plane.cstack";

/// Lint the cratestack schema against the documented 0.4.x grammar so the schema-first source of
/// truth cannot silently drift from `src/types.rs` (codegen stays deferred, ADR-0005). Best-effort:
/// skips with a hint when `cratestack-cli` is absent, so CI never hard-requires a young external crate.
pub(crate) fn validate_schema() -> anyhow::Result<()> {
    if on_path("cratestack-cli") {
        run("cratestack-cli", &["validate", SCHEMA])
    } else {
        eprintln!("cratestack-cli not installed — skipping schema validation.");
        eprintln!("Install to enforce: cargo install cratestack-cli --version 0.4.9");
        Ok(())
    }
}
