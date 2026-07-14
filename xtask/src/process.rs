//! Shared process-execution helpers used by every subcommand.

use std::path::Path;
use std::process::Command;

/// Run `cmd args`, wrapped in `chronic` when available (quiet on success, full output on failure).
pub(crate) fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = if on_path("chronic") {
        Command::new("chronic").arg(cmd).args(args).status()?
    } else {
        Command::new(cmd).args(args).status()?
    };
    if !status.success() {
        anyhow::bail!("`{cmd} {}` failed: {status}", args.join(" "));
    }
    Ok(())
}

/// Whether an executable named `bin` exists on `PATH` (a dependency-free `which`).
pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file() || Path::new(&candidate).with_extension("exe").is_file()
        })
    })
}
