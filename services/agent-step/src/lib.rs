//! Durability seam for Lightbridge agent steps.

#![allow(async_fn_in_trait)] // Native AFIT is the accepted static host boundary (ADR-0082).

use std::future::{Future, pending};
use std::time::Duration;

pub use lci_agent_types::StepError;
use lci_agent_types::StepName;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Stable identifier returned for a future externally-resolvable awaitable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwaitableId(StepName);

impl AwaitableId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Static durability boundary. A host supplies exactly one runtime implementation.
///
/// This trait intentionally is not dyn-compatible: journal implementations need the concrete
/// serializable return type of every step.
pub trait StepRuntime: Send + Sync {
    async fn step<T, F>(&self, name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send;

    async fn sleep(&self, name: StepName, after: Duration) -> Result<(), StepError>;

    async fn awaitable<T>(
        &self,
        name: StepName,
    ) -> Result<
        (
            AwaitableId,
            impl Future<Output = Result<T, StepError>> + Send,
        ),
        StepError,
    >
    where
        T: Serialize + DeserializeOwned + Send + 'static;
}

/// The current Kubernetes Job runtime: effects execute once in the calling task.
#[derive(Clone, Copy, Debug, Default)]
pub struct Passthrough;

impl StepRuntime for Passthrough {
    async fn step<T, F>(&self, _name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send,
    {
        f().await
    }

    async fn sleep(&self, _name: StepName, after: Duration) -> Result<(), StepError> {
        tokio::time::sleep(after).await;
        Ok(())
    }

    async fn awaitable<T>(
        &self,
        name: StepName,
    ) -> Result<
        (
            AwaitableId,
            impl Future<Output = Result<T, StepError>> + Send,
        ),
        StepError,
    >
    where
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        // Resolution routing is deliberately reserved for ADR-0081. Returning a stable id and a
        // pending in-process future keeps the R1 seam honest without inventing an external resolver.
        Ok((AwaitableId(name), pending()))
    }
}

/// Map a workspace failure without creating an upward dependency from `agent-step` to tools.
#[must_use]
pub fn workspace_error(reason: impl Into<String>) -> StepError {
    StepError::terminal(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_types::step_names;
    use std::time::Instant;

    #[tokio::test]
    async fn passthrough_executes_the_named_step_once() {
        let mut calls = 0;
        let value = Passthrough
            .step(step_names::llm_turn(2), async || {
                calls += 1;
                Ok::<_, StepError>(42_u8)
            })
            .await
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn passthrough_propagates_typed_failures() {
        let error = Passthrough
            .step(step_names::tools(0), async || {
                Err::<(), _>(StepError::terminal("bad arguments"))
            })
            .await
            .unwrap_err();
        assert!(!error.is_transient());
        assert!(error.to_string().contains("bad arguments"));
    }

    #[tokio::test]
    async fn passthrough_sleep_waits_for_the_requested_duration() {
        let started = Instant::now();
        Passthrough
            .sleep(StepName::from("retry"), Duration::from_millis(1))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn awaitable_keeps_the_stable_step_name() {
        let (id, future) = Passthrough
            .awaitable::<String>(StepName::from("human-input"))
            .await
            .unwrap();
        assert_eq!(id.as_str(), "human-input");
        assert!(
            tokio::time::timeout(Duration::from_millis(1), future)
                .await
                .is_err()
        );
    }

    #[test]
    fn workspace_failures_are_terminal_at_the_loop_boundary() {
        assert!(
            workspace_error("checkout missing")
                .to_string()
                .contains("checkout missing")
        );
    }
}
