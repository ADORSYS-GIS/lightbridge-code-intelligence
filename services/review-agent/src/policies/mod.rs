//! Review-flavoured policies kept above the generic runtime loop.
//!
//! Each submodule owns one independent [`TurnPolicy`](lci_agent_loop::TurnPolicy): [`coverage`] bounces a
//! premature `finish` until the changed files were actually engaged (or discloses what wasn't),
//! [`scratchpad`] breaks a same-line `add_review_comment` loop, [`refute`] makes the model re-verify its
//! own P0/P1 findings before finishing, [`sast_anchor`] rejects a SAST triage verdict anchored to a line
//! opengrep never flagged, and [`finding_nudge`] steers the model toward `finish` once it has recorded
//! something. Every policy here is preset-uniform (ADR-0103): none branches on which named preset is
//! running — only the numeric budgets the host passes in via [`crate::flows::ReviewRunParams`] (turn/
//! read/batch ceilings) differ. This file only holds the helpers shared across them and the flat
//! re-exports the host (`crate::flows`) consumes.

use lci_agent_tools::DispatchRefusal;
use lci_agent_types::ToolOutcome;

use crate::tools::fast_refusal;

mod coverage;
mod finding_nudge;
mod refute;
mod sast_anchor;
mod scratchpad;

#[cfg(test)]
mod test_support;

pub use coverage::{CoverageGate, CoverageState};
pub use finding_nudge::FindingFinishNudge;
pub use refute::RefuteGate;
pub use sast_anchor::{SastAnchorGate, SastLead, SastLeadSink};
pub use scratchpad::ScratchpadLoopGuard;

/// Pull a string field out of a tool call's raw JSON `arguments`. `None` on malformed JSON, a missing
/// key, or a non-string value — every policy here treats that as "couldn't identify the target" rather
/// than an error, since a bad call already gets its own error surfaced to the model by the dispatcher.
fn arg_field(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// Pull an integer field out of a tool call's raw JSON `arguments`, same contract as [`arg_field`].
fn arg_int_field(arguments: &str, key: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_i64()
}

/// Normalize a model-supplied repo path so the same file can't dodge coverage/loop tracking — or the
/// `run_sast` tool's changed-file scoping check (`crate::tools::sast`) — by varying its spelling
/// (backslashes, a leading `./`, or a leading `/`).
pub(crate) fn normalize_repo_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Exact assembly-owned rendering for strict fast-tier dispatch.
#[must_use]
pub fn render_fast_refusal(refusal: DispatchRefusal) -> ToolOutcome {
    match refusal {
        DispatchRefusal::NotOffered { tool_name } => {
            ToolOutcome::Continue(fast_refusal(&tool_name))
        }
        DispatchRefusal::MissingCallId { tool_name } => ToolOutcome::Continue(format!(
            "error: tool {tool_name:?} requires a non-empty call id for deduplication. Re-call the tool."
        )),
    }
}
