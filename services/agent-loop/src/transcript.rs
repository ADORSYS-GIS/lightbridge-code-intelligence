//! Loop outcome and transcript recording seam.

use lci_agent_types::{ToolCallReq, ToolOutcome, TurnTelemetry};
use serde::{Deserialize, Serialize};

use crate::chat::ChatMessage;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoopOutcome {
    Finished,
    Exhausted,
    Aborted { reason: String },
}

/// Generic transcript events. Control-plane transport rows remain owned by `agent-clients`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    Assistant {
        turn: usize,
        message: ChatMessage,
        /// This turn's model telemetry (tokens/reasoning), `None` when the model client didn't
        /// report any. Carried alongside `message` (never inside it — `ChatMessage` is what gets
        /// echoed back to the model) so a durable-replay host can journal/restore it with the turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry: Option<TurnTelemetry>,
    },
    Tool {
        turn: usize,
        call: ToolCallReq,
        outcome: ToolOutcome,
    },
    Policy {
        turn: usize,
        name: &'static str,
        detail: serde_json::Value,
    },
}

pub trait TranscriptSink: Send {
    fn record(&mut self, entry: TranscriptEvent);
}
