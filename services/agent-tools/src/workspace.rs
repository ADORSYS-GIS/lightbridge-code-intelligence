//! Checkout access and the per-call context every tool receives.

use std::path::Path;

use uuid::Uuid;

use crate::BoxFuture;

/// A checkout provider. Implementations may eagerly or lazily materialize the root.
pub trait Workspace: Send + Sync {
    fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>>;
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{reason}")]
pub struct WorkspaceError {
    reason: String,
}

impl WorkspaceError {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Context common to all tools. HTTP services remain owned by concrete review-tool implementations.
pub struct ToolCx<'a> {
    pub task_id: Uuid,
    pub workspace: &'a dyn Workspace,
}
