//! Stable-order owner of the complete tool surface, and the guarded per-turn dispatcher.

use std::sync::Arc;

use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};

use crate::{ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, TurnFilter};

/// Registration failures are startup errors, before a model can invoke a tool.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RegistryError {
    #[error("tool {0:?} is already registered")]
    DuplicateName(String),
    #[error("tool {0:?} needs a per-call dedup key, but the runtime cannot provide one")]
    MissingDedupCapability(String),
}

/// Stable-order owner of the complete tool surface.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        tool: Arc<dyn Tool>,
        caps: RuntimeCaps,
    ) -> Result<(), RegistryError> {
        let name = tool.spec().name();
        if self.find(name).is_some() {
            return Err(RegistryError::DuplicateName(name.to_string()));
        }
        if tool.replay() == ReplaySafety::NeedsDedupKey
            && caps.replays_completed_steps
            && !caps.per_call_dedup
        {
            return Err(RegistryError::MissingDedupCapability(name.to_string()));
        }
        self.tools.push(tool);
        Ok(())
    }

    #[must_use]
    pub fn view(&self, filter: &TurnFilter) -> TurnView<'_> {
        let offered: Vec<&dyn Tool> = self
            .tools
            .iter()
            .map(Arc::as_ref)
            .filter(|tool| filter.offers(*tool))
            .collect();
        let specs = offered.iter().map(|tool| tool.spec().clone()).collect();
        TurnView { offered, specs }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Classification lookup used by the generic loop to partition read batches from ordered
    /// effect calls. The tool implementation remains hidden behind the registry.
    #[must_use]
    pub fn kind(&self, name: &str) -> Option<ToolKind> {
        self.find(name).map(Tool::kind)
    }

    #[must_use]
    pub fn replay(&self, name: &str) -> Option<ReplaySafety> {
        self.find(name).map(Tool::replay)
    }

    /// Shared by-name lookup backing registration checks and the classification getters.
    fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .map(Arc::as_ref)
            .find(|tool| tool.spec().name() == name)
    }
}

/// A refusal is typed so each assembly renders its own exact model-facing steer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchRefusal {
    NotOffered { tool_name: String },
    MissingCallId { tool_name: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchResult {
    Completed(ToolOutcome),
    Refused(DispatchRefusal),
}

/// One turn's offered specs and guarded dispatcher.
pub struct TurnView<'r> {
    offered: Vec<&'r dyn Tool>,
    specs: Vec<ToolSpec>,
}

impl TurnView<'_> {
    #[must_use]
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub async fn dispatch(&self, cx: &ToolCx<'_>, call: &ToolCallReq) -> DispatchResult {
        match self
            .offered
            .iter()
            .find(|tool| tool.spec().name() == call.function.name)
        {
            Some(tool)
                if tool.replay() == ReplaySafety::NeedsDedupKey && call.id.trim().is_empty() =>
            {
                DispatchResult::Refused(DispatchRefusal::MissingCallId {
                    tool_name: call.function.name.clone(),
                })
            }
            Some(tool) => DispatchResult::Completed(tool.call(cx, call).await),
            None => DispatchResult::Refused(DispatchRefusal::NotOffered {
                tool_name: call.function.name.clone(),
            }),
        }
    }
}
