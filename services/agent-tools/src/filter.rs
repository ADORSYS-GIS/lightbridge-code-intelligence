//! Turn-scoped restriction of the tool surface offered to a model.

use std::collections::BTreeSet;

use crate::{Tool, ToolKind};

/// A monotonic restriction of the tools offered on one turn.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnFilter {
    allowed_names: Option<BTreeSet<String>>,
    blocked_kinds: BTreeSet<ToolKind>,
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
        }
    }

    #[must_use]
    pub fn without_kind(mut self, kind: ToolKind) -> Self {
        self.blocked_kinds.insert(kind);
        self
    }

    /// Intersect another policy restriction. This operation can never widen the set.
    pub fn narrow(&mut self, other: &Self) {
        match (&mut self.allowed_names, &other.allowed_names) {
            (Some(current), Some(next)) => current.retain(|name| next.contains(name)),
            (None, Some(next)) => self.allowed_names = Some(next.clone()),
            _ => {}
        }
        self.blocked_kinds
            .extend(other.blocked_kinds.iter().copied());
    }

    pub(crate) fn offers(&self, tool: &dyn Tool) -> bool {
        !self.blocked_kinds.contains(&tool.kind())
            && self
                .allowed_names
                .as_ref()
                .is_none_or(|names| names.contains(tool.spec().name()))
    }
}
