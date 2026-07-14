//! The small value types the Model (`App`/`DetailState`) is built from and the Update layer speaks
//! in: which screen is active, the repo status filter, transient toasts, and the confirm-dialog
//! prompt + the action it will fire once accepted.

use crate::theme::ButtonKind;
use std::time::Instant;

/// The operator views. `Detail` is a "page" opened from a selected Runs row (Enter / l / →).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Repositories,
    Runs,
    Detail,
}

/// The status filter cycled with `f` on the Repositories view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoFilter {
    All,
    Pending,
    Approved,
    Disabled,
}

impl RepoFilter {
    /// The `?status=` query value (`None` for `All`).
    pub fn as_query(self) -> Option<&'static str> {
        match self {
            RepoFilter::All => None,
            RepoFilter::Pending => Some("pending"),
            RepoFilter::Approved => Some("approved"),
            RepoFilter::Disabled => Some("disabled"),
        }
    }

    /// A short label for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            RepoFilter::All => "all",
            RepoFilter::Pending => "pending",
            RepoFilter::Approved => "approved",
            RepoFilter::Disabled => "disabled",
        }
    }

    /// Cycle to the next filter (wraps).
    pub fn next(self) -> Self {
        match self {
            RepoFilter::All => RepoFilter::Pending,
            RepoFilter::Pending => RepoFilter::Approved,
            RepoFilter::Approved => RepoFilter::Disabled,
            RepoFilter::Disabled => RepoFilter::All,
        }
    }
}

/// A transient status message shown at the bottom for a few seconds.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub shown_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// A pending confirmation prompt (`a`/`d`/`c` ask before acting). Carries which of the two buttons
/// currently has focus so the renderer can highlight it and Enter picks the right choice.
#[derive(Debug, Clone)]
pub struct Confirm {
    /// The target described in a full sentence (e.g. "Approve vymalo/lci?").
    pub prompt: String,
    /// A one-line consequence note shown under the prompt (may be empty).
    pub detail: String,
    /// The verb + kind for the affirmative button (e.g. "Approve", Primary).
    pub verb: String,
    pub verb_kind: ButtonKind,
    pub action: PendingAction,
    /// Which button is focused. `true` = the affirmative button, `false` = Cancel.
    pub confirm_focused: bool,
}

impl Confirm {
    /// Move focus to the other button (Left/Right/Tab all toggle between exactly two).
    pub fn toggle_focus(&mut self) {
        self.confirm_focused = !self.confirm_focused;
    }
}

/// The action a confirmation will trigger once accepted.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Approve(i64),
    Deny(i64),
    Cancel(uuid::Uuid),
}
