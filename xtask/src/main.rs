//! Workspace automation (cargo-xtask pattern). Invoked via the justfile, e.g. `cargo xtask ci`.
//! Keeping CI logic here (rather than only in YAML) lets the same gate run locally — shift-left.
//!
//! Every shell-out goes through [`process::run`], which transparently wraps the command in
//! [`chronic`](https://joeyh.name/code/moreutils/) when it is on `PATH`: output is swallowed on
//! success and printed in full only on failure, so a green gate stays quiet. Without `chronic`
//! installed it falls back to running the command directly.

mod commands;
mod process;

fn main() -> anyhow::Result<()> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "help".to_string());
    match task.as_str() {
        "ci" => commands::ci::ci(),
        "fmt" => commands::workspace::fmt(),
        "lint" => commands::workspace::lint(),
        "build" => commands::workspace::build(),
        "test" => commands::test::test(),
        "validate-schema" => commands::schema::validate_schema(),
        "dependency-hygiene" => commands::dependency_hygiene::dependency_hygiene(),
        _ => {
            eprintln!(
                "usage: cargo xtask <ci|fmt|lint|build|test|validate-schema|dependency-hygiene>"
            );
            Ok(())
        }
    }
}
