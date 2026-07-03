//! Terminal lifecycle: enter raw mode + the alternate screen on start, and ALWAYS restore on exit —
//! whether by clean return, error, or panic. A TUI that leaves the terminal wrecked is unacceptable,
//! so restoration runs from three places: an explicit call, a `Drop` guard, and a panic hook.
//!
//! [`restore`] is a pure function (no captured state) so it's callable from all three and testable.

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor::Show, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};

/// Restore the terminal to a sane state: **disable mouse capture**, leave the alternate screen,
/// disable raw mode, show the cursor. Idempotent and best-effort (errors are swallowed — we're
/// usually on a teardown path and there's nothing better to do). Pure: takes no captured state, so a
/// panic hook and the color-eyre error hook can both call it.
///
/// Disabling mouse capture here (not just where it was enabled) matters: if we leave it on, the host
/// terminal keeps swallowing scroll/selection after we exit — a wrecked terminal by another name.
pub fn restore() {
    let _ = write_restore_sequences(&mut io::stdout());
    let _ = disable_raw_mode();
}

/// Emit the restore escape sequences to `out` — **disable mouse capture**, leave the alternate
/// screen, show the cursor — in setup-reverse order. Split out from [`restore`] so a test can assert
/// the byte sequence (mouse-capture disable in particular) without touching the real terminal.
fn write_restore_sequences<W: std::io::Write>(out: &mut W) -> std::io::Result<()> {
    execute!(out, DisableMouseCapture, LeaveAlternateScreen, Show)
}

/// RAII guard that owns the ratatui terminal and guarantees [`restore`] runs on drop.
pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen + mouse capture, and install a panic hook that restores
    /// first, so a panic mid-render still leaves a usable terminal. The hook chains to whatever hook
    /// was installed before it — in `main` that's **color-eyre's** panic hook, so the pretty report
    /// still prints, just onto a restored terminal instead of a raw-mode mess.
    pub fn enter() -> Result<Self> {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous_hook(info);
        }));

        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = io::stdout();
        // Enter the alternate screen AND enable mouse capture (scroll drives the transcript/table).
        // Capture is toggled at runtime with `m` (see the event loop) so the operator can fall back to
        // the terminal's native text selection; restore() always disables it on teardown.
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .context("entering alternate screen + enabling mouse capture")?;
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
    fn restore_sequences_disable_mouse_capture() {
        // The teardown MUST turn mouse capture back off (leaving it on wrecks the host terminal's
        // scroll/selection after exit). Assert the disable sequence is present in the emitted bytes.
        let mut buf: Vec<u8> = Vec::new();
        write_restore_sequences(&mut buf).expect("write to a Vec never fails");
        let out = String::from_utf8_lossy(&buf);
        // crossterm's DisableMouseCapture emits the `?1000/?1002/?1003/?1006` "l" (reset) sequences.
        assert!(
            out.contains("\u{1b}[?1000l") || out.contains("?1000l"),
            "restore must emit the mouse-capture disable sequence, got: {out:?}"
        );
        // And it still leaves the alternate screen + shows the cursor.
        assert!(out.contains("?1049l"), "leaves the alternate screen");
        assert!(out.contains("?25h"), "shows the cursor");
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
