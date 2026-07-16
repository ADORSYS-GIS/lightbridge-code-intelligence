//! Workspace automation (cargo-xtask pattern). Invoked via the justfile, e.g. `cargo xtask ci`.
//! Keeping CI logic here (rather than only in YAML) lets the same gate run locally — shift-left.
//!
//! Every shell-out goes through [`process::run`], which transparently wraps the command in
//! [`chronic`](https://joeyh.name/code/moreutils/) when it is on `PATH`: output is swallowed on
//! success and printed in full only on failure, so a green gate stays quiet. Without `chronic`
//! installed it falls back to running the command directly.

mod commands;
mod process;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Workspace automation (cargo-xtask pattern)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Task>,
}

#[derive(Subcommand)]
enum Task {
    /// Run the full local CI gate (fmt, lint, build, test, validate-schema, dependency-hygiene).
    Ci,
    /// Format the workspace (`cargo fmt` + Biome for JS/TS).
    Fmt,
    /// Lint the workspace (`cargo clippy` + Biome check).
    Lint,
    /// Build the workspace.
    Build,
    /// Run the Rust test suite (prefers `cargo-nextest`, falls back to `cargo test`).
    Test,
    /// Validate committed schema files via `cratestack-cli` (best-effort — skips if absent).
    ValidateSchema,
    /// Check dependency hygiene across the workspace.
    DependencyHygiene,
    /// Deep-tier review repeat-run severity/anchor variance tooling (issue #420).
    ReviewVariance {
        #[command(subcommand)]
        action: commands::review_variance::Action,
    },
    /// OpenCode↔native review shadow parity: diff two engines' findings (RFC-0009 slice 4 gate).
    Shadow {
        #[command(subcommand)]
        action: commands::shadow::Action,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Task::Ci) => commands::ci::ci(),
        Some(Task::Fmt) => commands::workspace::fmt(),
        Some(Task::Lint) => commands::workspace::lint(),
        Some(Task::Build) => commands::workspace::build(),
        Some(Task::Test) => commands::test::test(),
        Some(Task::ValidateSchema) => commands::schema::validate_schema(),
        Some(Task::DependencyHygiene) => commands::dependency_hygiene::dependency_hygiene(),
        Some(Task::ReviewVariance { action }) => commands::review_variance::run(action),
        Some(Task::Shadow { action }) => commands::shadow::run(action),
        None => {
            Cli::parse_from(["xtask", "--help"]);
            Ok(())
        }
    }
}
