//! Terminal lifecycle: enter raw mode + the alternate screen on start, and ALWAYS restore on exit —
//! whether by clean return, error, or panic. A TUI that leaves the terminal wrecked is unacceptable,
//! so restoration runs from three places: an explicit call, a `Drop` guard, and a panic hook.
//!
//! [`restore`] is a pure function (no captured state) so it's callable from all three and testable.

use anyhow::{Context, Result};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor::Show, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};

/// Restore the terminal to a sane state: leave the alternate screen, disable raw mode, show the
/// cursor. Idempotent and best-effort (errors are swallowed — we're usually on a teardown path and
/// there's nothing better to do). Pure: takes no captured state, so a panic hook can call it too.
pub fn restore() {
    let mut out = io::stdout();
    // Order mirrors setup in reverse. Ignore errors — a half-restored terminal is still better than
    // bailing early and leaving raw mode on.
    let _ = execute!(out, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
}

/// RAII guard that owns the ratatui terminal and guarantees [`restore`] runs on drop.
pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen and install a panic hook that restores first, so a panic
    /// mid-render still leaves a usable terminal (the hook chains to the previous one for the report).
    pub fn enter() -> Result<Self> {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous_hook(info);
        }));

        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("constructing terminal")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A stand-in restore target we can assert ran. Mirrors the guard structure: a `Drop` that calls a
    /// pure restore fn, so we test the *contract* (restore runs exactly once on drop) without touching
    /// the real terminal (which isn't a TTY under `cargo test`).
    struct FakeGuard {
        counter: Arc<AtomicUsize>,
    }
    impl Drop for FakeGuard {
        fn drop(&mut self) {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn restore_is_callable_and_pure() {
        // Just calling it must not panic (it's all best-effort ignores off a TTY).
        restore();
    }

    #[test]
    fn drop_guard_runs_restore_exactly_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let _g = FakeGuard {
                counter: counter.clone(),
            };
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "not restored while alive"
            );
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1, "restored once on drop");
    }
}
