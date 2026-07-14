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
