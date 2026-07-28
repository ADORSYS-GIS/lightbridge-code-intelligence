//! Failure classification shared by the model, runtime, and tool-effect seams.

use std::fmt;
use std::time::Duration;

/// Failure classification shared by the model, runtime, and tool-effect seams.
///
/// The classification is deliberately made at the source of an effect. Runtimes may retry
/// [`StepError::Transient`] failures, while [`StepError::Terminal`] failures are deterministic.
#[derive(Debug)]
pub enum StepError {
    Transient {
        source: anyhow::Error,
        retry_after: Option<Duration>,
    },
    Terminal {
        reason: String,
    },
}

impl StepError {
    #[must_use]
    pub fn transient(source: impl Into<anyhow::Error>, retry_after: Option<Duration>) -> Self {
        Self::Transient {
            source: source.into(),
            retry_after,
        }
    }

    #[must_use]
    pub fn terminal(reason: impl Into<String>) -> Self {
        Self::Terminal {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Transient { retry_after, .. } => *retry_after,
            Self::Terminal { .. } => None,
        }
    }

    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

// NOTE (issue #503, thiserror/anyhow layering pass): this type is deliberately kept as a
// hand-written `Display`/`Error` impl instead of `#[derive(thiserror::Error)]`. This was not
// skipped out of laziness — it was evaluated and rejected for a concrete, provable reason:
//
// `Self::Transient { source, .. }` stores `source: anyhow::Error`, and today's `source()`
// delegates to `source.source()` — i.e. it skips the anyhow-wrapped top frame and returns
// *its* cause (one hop further down the chain than the field itself).
//
// thiserror's derive can only ever produce `Some(self.source.as_dyn_error())`, which — because
// `anyhow::Error: Deref<Target = dyn Error + Send + Sync>` and does not itself implement
// `std::error::Error` — resolves through autoderef to the anyhow-wrapped top frame itself. That
// is `Some(&self.source)`'s effective target, one hop *shallower* than today's behavior. This was
// confirmed by reading thiserror 2.0.19's codegen (`thiserror-impl/src/expand.rs`, the
// `self.#source #asref.as_dyn_error()` arm) and anyhow 1.0.102 (`anyhow::Error` has no inherent
// or trait `source()`; only `Deref` to the top-level error object).
//
// Worse, this can't be papered over with "derive for Display only, hand-write source()": the
// `#[derive(thiserror::Error)]` macro unconditionally emits a full `impl std::error::Error for
// StepError { .. }` block (with `source()` present-or-`None` per variant) whenever it runs at
// all — there is no way to derive Display without also getting thiserror's Error::source(), and
// a second, hand-written `impl std::error::Error for StepError` alongside the derive is a
// conflicting-impl compile error (E0119). Renaming the field away from `source` doesn't help
// either: thiserror would then see no source field anywhere on the enum and emit `source() ->
// None` unconditionally for every variant, which silently drops the chain instead of preserving
// it.
//
// So: keeping this hand-rolled is the only way to keep `StepError`'s externally observable
// `source()` chaining byte-identical, per this PR's zero-behavior-change mandate. If the
// one-hop discrepancy above is itself a bug, that's a separate, deliberately *not* fixed here —
// flagging for a follow-up ticket rather than silently changing behavior in a typing refactor.
impl fmt::Display for StepError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient { source, .. } => {
                write!(formatter, "transient step failure: {source:#}")
            }
            Self::Terminal { reason } => write!(formatter, "terminal step failure: {reason}"),
        }
    }
}

impl std::error::Error for StepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transient { source, .. } => source.source(),
            Self::Terminal { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StepError;
    use std::time::Duration;

    #[test]
    fn step_errors_preserve_retry_class_and_server_hint() {
        let retry_after = Duration::from_secs(7);
        let transient = StepError::transient(anyhow::anyhow!("gateway down"), Some(retry_after));
        assert!(transient.is_transient());
        assert_eq!(transient.retry_after(), Some(retry_after));
        assert!(transient.to_string().contains("gateway down"));
        let _ = std::error::Error::source(&transient);

        let terminal = StepError::terminal("malformed arguments");
        assert!(!terminal.is_transient());
        assert_eq!(terminal.retry_after(), None);
        assert!(terminal.to_string().contains("malformed arguments"));
        assert!(std::error::Error::source(&terminal).is_none());
    }
}
