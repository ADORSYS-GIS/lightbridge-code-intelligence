//! Loop outcome and transcript recording seam.

use lci_agent_types::{ToolCallReq, ToolOutcome};
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
