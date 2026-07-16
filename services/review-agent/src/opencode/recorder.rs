//! Recorder → `TurnOutcome` adapter: reconstruct one review turn's worth of tool calls from the
//! OpenCode recorder JSONL (ADR-0095) so the reused quality gates see exactly the tools that ran.
//!
//! The recorder runs in-process and is the completeness authority — it sees subagent-internal tool
//! calls the ACP client never gets, so an `explore` subagent's `read_file`s still count toward
//! coverage. Tool ids are normalized back to their canonical native names by longest-suffix match
//! (native names are mixed: bare `read_file` vs already-`lightbridge_`-prefixed
//! `lightbridge_vector_semantic_search`), so the reused gates compare against the strings their
//! constants expect regardless of the exact server prefix OpenCode applies.

use lci_agent_loop::{ChatMessage, ToolCallResult, TurnOutcome};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome};
use serde::Deserialize;
use serde_json::Value;

use crate::tools::{
    ABORT, ADD_COMMENT, ADD_REVIEW_COMMENT, FINISH, GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS,
    READ_FILE, REPORT_PROGRESS, RETRACT_FINDING, RUN_SAST, VECTOR_SEMANTIC_SEARCH,
};

/// Every review tool the gates key on, by its native canonical name. OpenCode exposes each mediated
/// tool under the `lightbridge` MCP server, so an observed tool id carries a server prefix (e.g.
/// `lightbridge_read_file`; and `lightbridge_lightbridge_vector_semantic_search` for a name that is
/// *already* `lightbridge_`-prefixed natively). [`normalize_tool_name`] maps an observed id back to
/// the canonical name here by longest-suffix match, so the reused gates see exactly the strings their
/// constants compare against — independent of the exact separator OpenCode uses.
pub const KNOWN_REVIEW_TOOLS: &[&str] = &[
    READ_FILE,
    ADD_REVIEW_COMMENT,
    RETRACT_FINDING,
    FINISH,
    ABORT,
    REPORT_PROGRESS,
    ADD_COMMENT,
    RUN_SAST,
    GRAPH_FIND_SYMBOL,
    GRAPH_GET_CALLERS,
    VECTOR_SEMANTIC_SEARCH,
];

/// Map an OpenCode tool id back to the canonical native review tool name, or `None` for a tool the
/// gates don't track (a built-in, or an unknown). Matches the longest known name that is a suffix of
/// `raw` at a non-identifier boundary — so `lightbridge_read_file` → `read_file`,
/// `lightbridge_lightbridge_vector_semantic_search` → `lightbridge_vector_semantic_search`, while
/// `spread_file` does NOT spuriously match `read_file` (the char before the suffix, `p`, is part of a
/// longer identifier).
#[must_use]
pub fn normalize_tool_name(raw: &str) -> Option<&'static str> {
    KNOWN_REVIEW_TOOLS
        .iter()
        .copied()
        .filter(|name| {
            let Some(prefix) = raw.strip_suffix(*name) else {
                return false;
            };
            // Whole string, or the suffix begins at a non-identifier boundary (`_`, `.`, `/`, …).
            prefix.is_empty() || !prefix.chars().next_back().is_some_and(char::is_alphanumeric)
        })
        // Longest match wins: `add_review_comment` over a shorter coincidental tail.
        .max_by_key(|name| name.len())
}

/// One recorder JSONL event (ADR-0095). Only the fields the adapter needs are declared; the recorder
/// also stamps `ts`/`sessionID`, ignored here. Deliberately lenient (`Option` everywhere) so a
/// half-written or unexpected line degrades to "no data" rather than failing the parse.
#[derive(Debug, Deserialize)]
pub struct RecorderEvent {
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(rename = "callID", default)]
    pub call_id: Option<String>,
    /// The tool's input object (recorder writes it as `args`).
    #[serde(default)]
    pub args: Option<Value>,
    /// The tool's full result object (`{content,isError}` for MCP tools; `{title,output,…}` for
    /// built-ins). Recorded verbatim by the plugin.
    #[serde(default)]
    pub result: Option<Value>,
}

/// Parse recorder JSONL, skipping blank or unparseable lines (the recorder must never take the loop
/// down, and the supervisor mirrors that leniency).
#[must_use]
pub fn parse_recorder(jsonl: &str) -> Vec<RecorderEvent> {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RecorderEvent>(line).ok())
        .collect()
}

/// Extract the human-visible result text a native `ToolOutcome::Continue` would carry, from either
/// tool-result shape. The MCP shape (`{content:[{type,text}],isError}`) is what the mediated review
/// tools return — its text is the dispatch message the gates match on (e.g. the RefuteGate keys off
/// `add_review_comment` returning `"recorded finding at …"`).
fn result_text(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let joined = content
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return joined;
        }
    }
    for key in ["output", "title"] {
        if let Some(text) = result.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    result.to_string()
}

/// One in-flight tool call being reassembled from its `tool.before` + `tool.after` events.
struct Pending {
    call_id: String,
    /// Canonical native name (already normalized), or `None` for an untracked tool.
    name: Option<&'static str>,
    raw_name: String,
    args: Value,
    result: Option<Value>,
}

/// Reconstruct one review [`TurnOutcome`] from the recorder events of a single OpenCode
/// `session/prompt` cycle. All of the cycle's tool calls collapse into one outcome's `results` (the
/// gates accumulate engagement across results order-independently, exactly as within a native turn);
/// `finish_requested` / `abort_reason` are set from a `finish` / `abort` tool call in the cycle.
#[must_use]
pub fn cycle_turn_outcome(events: &[RecorderEvent]) -> TurnOutcome {
    let mut pending: Vec<Pending> = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "tool.before" => {
                let Some(call_id) = event.call_id.clone() else {
                    continue;
                };
                let raw_name = event.tool.clone().unwrap_or_default();
                pending.push(Pending {
                    call_id,
                    name: normalize_tool_name(&raw_name),
                    raw_name,
                    args: event.args.clone().unwrap_or(Value::Null),
                    result: event.result.clone(),
                });
            }
            "tool.after" => {
                // Attach to the most recent same-id call still awaiting its result.
                if let Some(slot) = event.call_id.as_ref().and_then(|id| {
                    pending
                        .iter_mut()
                        .rev()
                        .find(|p| &p.call_id == id && p.result.is_none())
                }) {
                    slot.result = event.result.clone();
                } else if let Some(id) = event.call_id.clone() {
                    // An `after` with no matching `before` (cycle boundary cut the pair): keep it so
                    // its result still counts.
                    let raw_name = event.tool.clone().unwrap_or_default();
                    pending.push(Pending {
                        call_id: id,
                        name: normalize_tool_name(&raw_name),
                        raw_name,
                        args: event.args.clone().unwrap_or(Value::Null),
                        result: event.result.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    let mut finish_requested = false;
    let mut abort_reason = None;
    let mut results = Vec::with_capacity(pending.len());
    for call in pending {
        let name = call
            .name
            .map_or_else(|| call.raw_name.clone(), str::to_string);
        let arguments = if call.args.is_null() {
            "{}".to_string()
        } else {
            call.args.to_string()
        };
        let outcome = match call.name {
            Some(FINISH) => {
                finish_requested = true;
                ToolOutcome::Finish
            }
            Some(ABORT) => {
                let reason = call
                    .args
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| call.result.as_ref().map(result_text))
                    .unwrap_or_default();
                abort_reason.get_or_insert(reason.clone());
                ToolOutcome::Abort(reason)
            }
            _ => ToolOutcome::Continue(call.result.as_ref().map(result_text).unwrap_or_default()),
        };
        results.push(ToolCallResult {
            call: ToolCallReq {
                id: call.call_id,
                kind: "function".to_string(),
                function: FunctionCallReq { name, arguments },
                extra_content: None,
            },
            kind: None,
            outcome,
        });
    }

    TurnOutcome {
        // The gates ignore `assistant`; the recorder carries reasoning separately (ADR-0060).
        assistant: ChatMessage::user(""),
        results,
        finish_requested,
        abort_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{after, before};
    use super::*;

    #[test]
    fn normalizes_prefixed_and_self_prefixed_tool_ids() {
        // A bare native name gets the server prefix.
        assert_eq!(
            normalize_tool_name("lightbridge_read_file"),
            Some(READ_FILE)
        );
        assert_eq!(normalize_tool_name("lightbridge_finish"), Some(FINISH));
        // A name already `lightbridge_`-prefixed natively → server double-prefix; strip one, match.
        assert_eq!(
            normalize_tool_name("lightbridge_lightbridge_vector_semantic_search"),
            Some(VECTOR_SEMANTIC_SEARCH)
        );
        // An exact, unprefixed id still matches (defensive — separator-independent).
        assert_eq!(
            normalize_tool_name("add_review_comment"),
            Some(ADD_REVIEW_COMMENT)
        );
        // A longer identifier that merely *ends in* a known name must NOT match (boundary check).
        assert_eq!(normalize_tool_name("spread_file"), None);
        // An untracked built-in.
        assert_eq!(normalize_tool_name("grep"), None);
    }

    #[test]
    fn reconstructs_a_cycle_outcome_with_finish_and_finding_text() {
        let events = vec![
            before(
                "lightbridge_read_file",
                "c1",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "c1", "source of a.rs"),
            before(
                "lightbridge_add_review_comment",
                "c2",
                serde_json::json!({"file": "a.rs", "line": 2, "priority": "P1"}),
            ),
            after(
                "lightbridge_add_review_comment",
                "c2",
                "recorded finding at a.rs:2",
            ),
            before(
                "lightbridge_finish",
                "c3",
                serde_json::json!({"summary": "done"}),
            ),
            after(
                "lightbridge_finish",
                "c3",
                "Review finished; the host will finalize.",
            ),
        ];
        let outcome = cycle_turn_outcome(&events);
        assert!(outcome.finish_requested);
        assert!(outcome.abort_reason.is_none());
        assert_eq!(outcome.results.len(), 3);
        // read_file keeps its path arg for coverage accounting.
        assert_eq!(outcome.results[0].call.function.name, READ_FILE);
        assert!(outcome.results[0].call.function.arguments.contains("a.rs"));
        // add_review_comment's outcome text is the dispatch message the refute gate keys on.
        assert_eq!(outcome.results[1].call.function.name, ADD_REVIEW_COMMENT);
        assert!(matches!(
            &outcome.results[1].outcome,
            ToolOutcome::Continue(text) if text.starts_with("recorded finding")
        ));
        assert!(matches!(outcome.results[2].outcome, ToolOutcome::Finish));
    }

    #[test]
    fn reconstructs_abort_reason_from_args() {
        let events = vec![
            before(
                "lightbridge_abort",
                "c1",
                serde_json::json!({"reason": "cannot review"}),
            ),
            after("lightbridge_abort", "c1", "Review aborted: cannot review"),
        ];
        let outcome = cycle_turn_outcome(&events);
        assert_eq!(outcome.abort_reason.as_deref(), Some("cannot review"));
        assert!(!outcome.finish_requested);
    }
}
