//! The `test` subcommand.

use crate::process::{on_path, run};

/// Prefer cargo-nextest; fall back to `cargo test` only when nextest itself is not installed.
///
/// The fallback must be gated on nextest's *availability*, not on `run`'s exit status: a genuine
/// test failure under nextest also returns `Err`, and re-running the whole suite under
/// `cargo test` in that case would silently mask the failure as a "handled missing tool" case.
pub(crate) fn test() -> anyhow::Result<()> {
    run_test(
        on_path("cargo-nextest"),
        || run("cargo", &["nextest", "run"]),
        || run("cargo", &["test"]),
    )
}

fn run_test(
    nextest_available: bool,
    run_nextest: impl FnOnce() -> anyhow::Result<()>,
    run_cargo_test: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if nextest_available {
        run_nextest()
    } else {
        run_cargo_test()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn genuine_nextest_failure_propagates_without_falling_back() {
        let cargo_test_called = Cell::new(false);

        let result = run_test(
            true,
            || anyhow::bail!("`cargo nextest run` failed: 2 tests failed"),
            || {
                cargo_test_called.set(true);
                Ok(())
            },
        );

        assert!(
            result.is_err(),
            "a genuine nextest failure must propagate as an error"
        );
        assert!(
            !cargo_test_called.get(),
            "a genuine nextest failure must not trigger the cargo-test fallback"
        );
    }

    #[test]
    fn missing_nextest_falls_back_to_cargo_test() {
        let cargo_test_called = Cell::new(false);

        let result = run_test(
            false,
            || anyhow::bail!("should not be called when nextest is unavailable"),
            || {
                cargo_test_called.set(true);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert!(
            cargo_test_called.get(),
            "missing nextest must fall back to cargo test"
        );
    }
}
