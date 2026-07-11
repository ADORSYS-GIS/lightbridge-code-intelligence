//! Durability seam for Lightbridge agent steps.
//!
//! [`StepRuntime`] is the single seam that separates *what* the agent loop does (deterministic glue
//! over journaled effects) from *how* those effects are made durable. The Job host uses
//! [`Passthrough`] — steps run inline exactly once, names are ignored. The Restate host (R2) will
//! implement the same trait over a `WorkflowContext`, so on replay a completed step returns its
//! journaled value without re-executing (ADR-0082).
//!
//! The trait is deliberately **not dyn-compatible**: `step` is generic over the journaled value and
//! takes an async closure, so `AgentLoop<R: StepRuntime>` is monomorphized per host and there is
//! never a `Box<dyn StepRuntime>` nor a runtime chosen at run time (companion doc §3.1).

use std::time::Duration;

use lci_agent_types::{StepError, StepName};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Runs the deterministic glue of an agent loop, making each named effect durable.
///
/// Native `async fn` in a trait (AFIT) is used intentionally: this is statically dispatched, so no
/// `async-trait` boxing is needed. The `async_fn_in_trait` lint (which warns that callers can't add
/// their own `Send` bound to the returned future) does not apply here — every consumer is generic
/// over a single concrete `R`, not over `dyn StepRuntime`.
#[allow(async_fn_in_trait)]
pub trait StepRuntime: Send + Sync {
    /// Run `f` as the named journaled step. On a durable host, a completed step returns its
    /// journaled value on replay without executing `f`; [`Passthrough`] always executes `f`.
    async fn step<T, F>(&self, name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send;

    /// A durable timer. [`Passthrough`] delegates to `tokio::time::sleep`; the Restate host uses a
    /// journaled `ctx.sleep`, so the delay survives a crash/replay.
    async fn sleep(&self, name: StepName, after: Duration) -> Result<(), StepError>;
}

/// The Job-host runtime: every step executes inline exactly once and step names are ignored.
///
/// This is the pre-durability behavior — a crash loses the run, exactly as a Kubernetes Job does
/// today. It exists so `AgentLoop` can be exercised (and shipped as the Job path) without the
/// Restate SDK anywhere in its dependency graph.
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
}

#[cfg(test)]
mod tests {
    use super::{Passthrough, StepRuntime};
    use lci_agent_types::{StepError, step_names};
    use std::time::Duration;

    #[tokio::test]
    async fn passthrough_runs_the_step_body_and_returns_its_value() {
        let value: u32 = Passthrough
            .step(step_names::llm_turn(0), async || Ok(41 + 1))
            .await
            .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn passthrough_propagates_step_errors_unchanged() {
        let err = Passthrough
            .step(step_names::tools(0), async || {
                Err::<(), _>(StepError::terminal("unknown tool"))
            })
            .await
            .unwrap_err();
        assert!(!err.is_transient());
        assert!(err.to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn passthrough_sleep_awaits_a_timer_and_completes() {
        // A short real timer keeps the test fast while proving `sleep` awaits and returns `Ok`.
        Passthrough
            .sleep(step_names::BOOTSTRAP.into(), Duration::from_millis(1))
            .await
            .unwrap();
    }
}
