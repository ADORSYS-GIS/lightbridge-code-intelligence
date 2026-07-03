//! The visual identity of the TUI: a central [`Theme`] palette every widget pulls from, so `ui.rs`
//! never hardcodes a `Color::`. The palette mirrors k9s's skin regions and opencode's `ThemePalette`
//! (background/surface, foreground/muted, accent/brand/secondary, borders, and the four semantic
//! status colors), plus a handful of derived style helpers.
//!
//! Discipline (grounded in k9s + opencode): **accent only for interactive/selected elements; status
//! in semantic colors; metadata dim/muted; ~80% of text in the default foreground; headers bold.**
//! Colorful where it carries meaning, calm everywhere else.

use ratatui::style::{Color, Modifier, Style};

/// Which built-in theme is active. Cyclable at runtime with `t`, selectable via `LCI_THEME` /
/// `config.toml`'s `theme =`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    /// Tokyo-Night-ish warm dark (default).
    Midnight,
    /// Transparent background, colors from the terminal's own 16 ANSI slots.
    Terminal,
    /// Nord — cool, muted arctic palette.
    Nord,
}

impl ThemeKind {
    /// All themes in cycle order (also the `t`-key order). Used by tests + external tooling.
    #[allow(dead_code)]
    pub const ALL: [ThemeKind; 3] = [ThemeKind::Midnight, ThemeKind::Terminal, ThemeKind::Nord];

    /// The next theme in the cycle (wraps).
    pub fn next(self) -> Self {
        match self {
            ThemeKind::Midnight => ThemeKind::Terminal,
            ThemeKind::Terminal => ThemeKind::Nord,
            ThemeKind::Nord => ThemeKind::Midnight,
        }
    }

    /// The lowercase name used in config/env and shown in the header.
    pub fn name(self) -> &'static str {
        match self {
            ThemeKind::Midnight => "midnight",
            ThemeKind::Terminal => "terminal",
            ThemeKind::Nord => "nord",
        }
    }

    /// Parse a name (case-insensitive) from `LCI_THEME` / config; unknown → default `midnight`.
    pub fn from_name(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "terminal" => ThemeKind::Terminal,
            "nord" => ThemeKind::Nord,
            _ => ThemeKind::Midnight,
        }
    }
}

/// Which button a confirm dialog draws (drives the accent color of the focused button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The affirmative choice (approve / confirm) — brand/success accent when focused.
    Primary,
    /// The destructive choice (deny) — error accent when focused.
    Danger,
    /// The dismissive choice (cancel) — neutral, muted when focused.
    Neutral,
}

/// The full palette + derived helpers. Cheap to `Copy`, so widgets take it by value.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub kind: ThemeKind,

    // --- surfaces ---
    pub background: Color,
    pub surface: Color,

    // --- text ---
    pub foreground: Color,
    pub muted: Color,

    // --- interactive / identity ---
    pub accent: Color,
    pub brand: Color,
    pub secondary: Color,

    // --- chrome ---
    pub border: Color,
    pub border_focus: Color,

    // --- semantic ---
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::from_kind(ThemeKind::Midnight)
    }
}

impl Theme {
    /// Build the palette for a given kind.
    pub fn from_kind(kind: ThemeKind) -> Self {
        match kind {
            // Tokyo-Night-ish warm dark.
            ThemeKind::Midnight => Theme {
                kind,
                background: Color::Rgb(0x1a, 0x1b, 0x26),
                surface: Color::Rgb(0x24, 0x28, 0x3b),
                foreground: Color::Rgb(0xc0, 0xca, 0xf5),
                muted: Color::Rgb(0x56, 0x5f, 0x89),
                accent: Color::Rgb(0xbb, 0x9a, 0xf7),
                brand: Color::Rgb(0xff, 0x9e, 0x64),
                secondary: Color::Rgb(0x7a, 0xa2, 0xf7),
                border: Color::Rgb(0x41, 0x48, 0x68),
                border_focus: Color::Rgb(0xbb, 0x9a, 0xf7),
                success: Color::Rgb(0x9e, 0xce, 0x6a),
                warning: Color::Rgb(0xe0, 0xaf, 0x68),
                error: Color::Rgb(0xf7, 0x76, 0x8e),
                info: Color::Rgb(0x7d, 0xcf, 0xff),
            },
            // Transparent surfaces, everything else from the terminal's own 16 ANSI colors — proves
            // the app works with no forced background.
            ThemeKind::Terminal => Theme {
                kind,
                background: Color::Reset,
                surface: Color::Reset,
                foreground: Color::Reset,
                muted: Color::DarkGray,
                accent: Color::Magenta,
                brand: Color::Yellow,
                secondary: Color::Blue,
                border: Color::DarkGray,
                border_focus: Color::Magenta,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                info: Color::Cyan,
            },
            // Nord — cool arctic palette.
            ThemeKind::Nord => Theme {
                kind,
                background: Color::Rgb(0x2e, 0x34, 0x40),
                surface: Color::Rgb(0x3b, 0x42, 0x52),
                foreground: Color::Rgb(0xe5, 0xe9, 0xf0),
                muted: Color::Rgb(0x61, 0x6e, 0x88),
                accent: Color::Rgb(0x88, 0xc0, 0xd0),
                brand: Color::Rgb(0x8f, 0xbc, 0xbb),
                secondary: Color::Rgb(0x81, 0xa1, 0xc1),
                border: Color::Rgb(0x4c, 0x56, 0x6a),
                border_focus: Color::Rgb(0x88, 0xc0, 0xd0),
                success: Color::Rgb(0xa3, 0xbe, 0x8c),
                warning: Color::Rgb(0xeb, 0xcb, 0x8b),
                error: Color::Rgb(0xbf, 0x61, 0x6a),
                info: Color::Rgb(0x81, 0xa1, 0xc1),
            },
        }
    }

    // --- derived style helpers -------------------------------------------------------------------

    /// A bold header/label style (table headers, section titles).
    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.foreground)
            .bg(self.surface)
            .add_modifier(Modifier::BOLD)
    }

    /// The selected-row cursor: accent background, contrasting foreground, bold.
    pub fn selected_row_style(&self) -> Style {
        Style::default()
            .fg(self.on_accent())
            .bg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Default body text.
    pub fn text(&self) -> Style {
        Style::default().fg(self.foreground)
    }

    /// Dim metadata (timestamps, counts, hints).
    pub fn muted_text(&self) -> Style {
        Style::default().fg(self.muted)
    }

    /// The panel/bar background fill.
    pub fn surface_style(&self) -> Style {
        Style::default().bg(self.surface).fg(self.foreground)
    }

    /// Semantic color for a repo/task status word (the status column).
    pub fn status_color(&self, status: &str) -> Color {
        match status {
            // repo
            "approved" => self.success,
            "pending" => self.warning,
            "disabled" => self.error,
            // task — in flight
            "running" | "posting_result" => self.info,
            "queued" | "waiting_for_index" | "received" => self.secondary,
            // task — terminal
            "succeeded" => self.success,
            "failed" | "timed_out" => self.error,
            "cancelled" => self.muted,
            _ => self.muted,
        }
    }

    /// A button style for the confirm dialog. Focused buttons get a solid semantic background with a
    /// contrasting foreground + bold; unfocused ones sit muted on the surface.
    pub fn button(&self, focused: bool, kind: ButtonKind) -> Style {
        if !focused {
            return Style::default().fg(self.muted).bg(self.surface);
        }
        let bg = match kind {
            ButtonKind::Primary => self.success,
            ButtonKind::Danger => self.error,
            ButtonKind::Neutral => self.secondary,
        };
        Style::default()
            .fg(self.on_accent())
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    }

    /// A readable foreground to place *on top of* an accent/semantic fill. Dark themes get a near-black
    /// ink; the terminal theme uses `Black` (works against the bright ANSI backgrounds).
    pub fn on_accent(&self) -> Color {
        match self.kind {
            ThemeKind::Terminal => Color::Black,
            // A dark ink drawn from the theme's own background family reads as "punched out".
            ThemeKind::Midnight => Color::Rgb(0x1a, 0x1b, 0x26),
            ThemeKind::Nord => Color::Rgb(0x2e, 0x34, 0x40),
        }
    }
}

/// A short, human display label for a task/repo status, so the STATUS column never truncates
/// mid-word (`waiting_for_index` → `indexing`, `posting_result` → `posting`). The raw status is
/// still what feeds [`Theme::status_color`] — this only changes what's *shown*. Unknown statuses
/// pass through unchanged.
pub fn status_label(status: &str) -> &str {
    match status {
        "waiting_for_index" => "indexing",
        "posting_result" => "posting",
        "succeeded" => "done",
        "timed_out" => "timed-out",
        // Already short and clear: running, queued, received, failed, cancelled, pending, approved,
        // disabled.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_kinds_round_trip_by_name() {
        for kind in ThemeKind::ALL {
            assert_eq!(ThemeKind::from_name(kind.name()), kind);
        }
    }

    #[test]
    fn unknown_theme_name_falls_back_to_midnight() {
        assert_eq!(ThemeKind::from_name("nonsense"), ThemeKind::Midnight);
        assert_eq!(ThemeKind::from_name("MIDNIGHT"), ThemeKind::Midnight);
    }

    #[test]
    fn cycle_visits_every_theme_and_wraps() {
        let mut seen = Vec::new();
        let mut k = ThemeKind::Midnight;
        for _ in 0..ThemeKind::ALL.len() {
            seen.push(k);
            k = k.next();
        }
        assert_eq!(k, ThemeKind::Midnight, "cycle wraps to the start");
        for kind in ThemeKind::ALL {
            assert!(seen.contains(&kind), "cycle visits {kind:?}");
        }
    }

    #[test]
    fn status_labels_are_short_and_never_mid_word() {
        // The long ones get shortened...
        assert_eq!(status_label("waiting_for_index"), "indexing");
        assert_eq!(status_label("posting_result"), "posting");
        assert_eq!(status_label("succeeded"), "done");
        assert_eq!(status_label("timed_out"), "timed-out");
        // ...the already-short ones pass through...
        assert_eq!(status_label("running"), "running");
        assert_eq!(status_label("failed"), "failed");
        assert_eq!(status_label("cancelled"), "cancelled");
        // ...and unknowns are untouched.
        assert_eq!(status_label("who-knows"), "who-knows");
        // Every label fits the 16-col Runs STATUS column with room to spare.
        for raw in [
            "running",
            "queued",
            "waiting_for_index",
            "posting_result",
            "received",
            "succeeded",
            "failed",
            "timed_out",
            "cancelled",
        ] {
            assert!(
                status_label(raw).chars().count() <= 12,
                "label for {raw} fits the column"
            );
        }
    }

    #[test]
    fn status_color_keyed_on_raw_status_unchanged() {
        // The label must NOT change what color a status renders — color still keys on the raw value.
        let t = Theme::from_kind(ThemeKind::Midnight);
        assert_eq!(t.status_color("waiting_for_index"), t.secondary);
        assert_eq!(t.status_color("posting_result"), t.info);
        assert_eq!(t.status_color("succeeded"), t.success);
        assert_eq!(t.status_color("timed_out"), t.error);
    }

    #[test]
    fn status_colors_are_semantic() {
        let t = Theme::from_kind(ThemeKind::Midnight);
        assert_eq!(t.status_color("approved"), t.success);
        assert_eq!(t.status_color("pending"), t.warning);
        assert_eq!(t.status_color("disabled"), t.error);
        assert_eq!(t.status_color("running"), t.info);
        assert_eq!(t.status_color("queued"), t.secondary);
        assert_eq!(t.status_color("failed"), t.error);
        assert_eq!(t.status_color("cancelled"), t.muted);
        assert_eq!(t.status_color("who-knows"), t.muted);
    }

    #[test]
    fn terminal_theme_has_transparent_surfaces() {
        let t = Theme::from_kind(ThemeKind::Terminal);
        assert_eq!(t.background, Color::Reset);
        assert_eq!(t.surface, Color::Reset);
    }

    #[test]
    fn focused_button_differs_from_unfocused() {
        let t = Theme::from_kind(ThemeKind::Midnight);
        let focused = t.button(true, ButtonKind::Primary);
        let unfocused = t.button(false, ButtonKind::Primary);
        assert_ne!(focused.bg, unfocused.bg);
        assert_eq!(focused.bg, Some(t.success));
    }
}
