//! Turn-scoped restriction of the tool surface offered to a model.

use std::collections::BTreeSet;

use crate::{Tool, ToolKind};

/// A restriction of the tools offered on one turn. `allowed_names`/`blocked_kinds` narrow
/// monotonically (a later `narrow()` can only shrink them); `forced_names` is the one deliberate
/// exception — see [`TurnFilter::force_names`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnFilter {
    allowed_names: Option<BTreeSet<String>>,
    blocked_kinds: BTreeSet<ToolKind>,
    forced_names: BTreeSet<String>,
}

impl TurnFilter {
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn only_names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_names: Some(names.into_iter().map(Into::into).collect()),
            blocked_kinds: BTreeSet::new(),
            forced_names: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn without_kind(mut self, kind: ToolKind) -> Self {
        self.blocked_kinds.insert(kind);
        self
    }

    /// Force specific tools onto this turn's offered set regardless of `allowed_names`/
    /// `blocked_kinds` — this policy's or any other's, on either side of a `narrow()` call. This is
    /// a deliberate one-shot escape hatch (e.g. `RefuteGate`'s post-bounce re-verification turn,
    /// #407): unlike the rest of `TurnFilter`, a forced name is never removed by narrowing, so a
    /// policy registered earlier (like `WindDown`) can't strip it back out. Callers are responsible
    /// for scoping how long the force applies (typically a single turn).
    #[must_use]
    pub fn force_names(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.forced_names.extend(names.into_iter().map(Into::into));
        self
    }

    /// Intersect another policy restriction's `allowed_names`/`blocked_kinds` (this can never widen
    /// those two), while unioning `forced_names` (which can only ever grow — see
    /// [`TurnFilter::force_names`]).
    pub fn narrow(&mut self, other: &Self) {
        match (&mut self.allowed_names, &other.allowed_names) {
            (Some(current), Some(next)) => current.retain(|name| next.contains(name)),
            (None, Some(next)) => self.allowed_names = Some(next.clone()),
            _ => {}
        }
        self.blocked_kinds
            .extend(other.blocked_kinds.iter().copied());
        self.forced_names.extend(other.forced_names.iter().cloned());
    }

    pub(crate) fn offers(&self, tool: &dyn Tool) -> bool {
        let name = tool.spec().name();
        self.forced_names.contains(name)
            || (!self.blocked_kinds.contains(&tool.kind())
                && self
                    .allowed_names
                    .as_ref()
                    .is_none_or(|names| names.contains(name)))
    }
}
