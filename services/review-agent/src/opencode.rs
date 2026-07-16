//! OpenCode review host — the parity-critical adapter (RFC-0009 / ADR-0094/0095 review cutover, slice 3).
//!
//! The native review path drives `AgentLoop`, which owns both the model call and tool dispatch and
//! interleaves the review policies per turn ([`crate::flows::run_review`]). When review runs on
//! OpenCode instead, **OpenCode owns its own loop**: one `session/prompt` runs its entire internal
//! agent cycle (many model round-trips, many tool calls) and returns once. The supervisor only
//! *observes* and *re-drives*. That splits the native policies into two classes:
//!
//! - **Loop mechanics** (context-trim, wind-down, read/turn budgets — ADR-0042 batching): handed to
//!   OpenCode's own maintained loop. The supervisor cannot narrow OpenCode's tools mid-internal-loop.
//! - **Review-quality gates** ([`CoverageGate`], [`RefuteGate`]): kept as Rust and run *here*, reusing
//!   the exact tuned `TurnPolicy` code — no TypeScript reimplementation of the coverage denominator,
//!   citation crediting, or the ADR-0091 absence-claim directive.
//!
//! This module is the pure, host-independent core: it (1) reconstructs a review [`TurnOutcome`] from
//! the OpenCode **recorder JSONL** (ADR-0095) — the in-process completeness authority that sees every
//! tool call, *including subagent-internal ones the ACP client is never shown* (so coverage counts an
//! `explore` subagent's `read_file`s) — and (2) drives the reused gates over each cycle, emitting the
//! same `RejectFinish` bounces (→ re-prompt OpenCode) and coverage disclosure the native loop would.
//! The transport (spawning OpenCode, tailing the recorder, sending `session/prompt`) is the host's job.

use lci_agent_loop::{
    ChatMessage, LoopStats, Nudge, PolicyAction, ToolCallResult, TurnOutcome, TurnPolicy, TurnState,
};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use serde_json::Value;

use crate::policies::{CoverageGate, CoverageState, RefuteGate};
use crate::tools::{
    ABORT, ADD_COMMENT, ADD_REVIEW_COMMENT, FINISH, GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS, READ_FILE,
    REPORT_PROGRESS, RETRACT_FINDING, RUN_SAST, VECTOR_SEMANTIC_SEARCH,
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
        let name = call.name.map_or_else(|| call.raw_name.clone(), str::to_string);
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
            _ => ToolOutcome::Continue(
                call.result.as_ref().map(result_text).unwrap_or_default(),
            ),
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

/// A minimal `TurnState` for driving the finish-time gates. The reused gates read only `turn`
/// (CoverageGate's wind-down guard) and `max_turns` off it; everything else is empty/default, mirroring
/// the policy unit tests' own scaffold.
fn turn_state(turn: usize, max_turns: usize) -> TurnState<'static> {
    static MESSAGES: [ChatMessage; 0] = [];
    static TOOLS: [ToolSpec; 0] = [];
    static STATS: LoopStats = LoopStats {
        files_read: 0,
        searches: 0,
        batches: 0,
        successful_writes: 0,
        findings_recorded: 0,
    };
    TurnState {
        turn,
        max_turns,
        messages: &MESSAGES,
        base_tools: &TOOLS,
        stats: &STATS,
        converging: false,
    }
}

/// What the supervisor should do after one observed OpenCode cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The cycle didn't request finish (or aborted): keep driving / finalize as the cycle dictates.
    Proceed,
    /// A quality gate rejected the finish. Re-prompt OpenCode with this text (the combined nudge),
    /// then observe the next cycle — do NOT finalize.
    RejectFinish(String),
    /// Every gate accepted the finish: finalize. `disclosure`, when `Some`, is the coverage note to
    /// append to the posted summary (ADR-0069 / #306).
    Accept { disclosure: Option<String> },
}

/// The reused review quality gates, driven over OpenCode `session/prompt` cycles instead of native
/// turns. Owns the same [`CoverageGate`] + [`RefuteGate`] the native flow composes; their internal
/// counters (coverage bounces, the refute one-shot) advance exactly as native because each cycle's
/// outcome is fed through the identical `after_turn_actions`.
pub struct ReviewGates {
    coverage: CoverageGate,
    coverage_state: CoverageState,
    refute: RefuteGate,
    max_turns: usize,
    fast: bool,
    cycle: usize,
}

impl ReviewGates {
    /// Compose the gates for one review, mirroring [`crate::flows::run_review`]'s construction (same
    /// coverage denominator, bounce cap, and fast-tier disabling).
    #[must_use]
    pub fn new(
        diff_files: Vec<String>,
        max_coverage_bounces: usize,
        max_turns: usize,
        fast: bool,
    ) -> Self {
        let (coverage, coverage_state) =
            CoverageGate::new(diff_files, max_coverage_bounces, max_turns, fast);
        Self {
            coverage,
            coverage_state,
            refute: RefuteGate::new(fast),
            max_turns,
            fast,
            cycle: 0,
        }
    }

    /// Observe one completed OpenCode cycle. Always feeds the outcome to both gates (so engagement /
    /// findings accumulate on every cycle, not just finish cycles), then — only when the cycle
    /// requested finish — returns whether a gate bounced it (coverage first, then refute, matching
    /// native registration order) or accepted it with any coverage disclosure to append.
    pub fn observe_cycle(&mut self, outcome: &TurnOutcome) -> GateDecision {
        let turn = self.cycle;
        let state = turn_state(turn, self.max_turns);
        let coverage_actions = self.coverage.after_turn_actions(&state, outcome);
        let state = turn_state(turn, self.max_turns);
        let refute_actions = self.refute.after_turn_actions(&state, outcome);
        self.cycle += 1;

        // Aborts aren't gated (the run ends with the model's reason); a non-finish cycle just
        // accumulated engagement, nothing to decide.
        if outcome.abort_reason.is_some() || !outcome.finish_requested {
            return GateDecision::Proceed;
        }

        let nudges: Vec<String> = coverage_actions
            .iter()
            .chain(refute_actions.iter())
            .filter_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .collect();
        if !nudges.is_empty() {
            return GateDecision::RejectFinish(nudges.join("\n\n"));
        }

        // Accepted — flush any coverage disclosure via the same finish path the native loop runs.
        let state = turn_state(turn, self.max_turns);
        let _ = self.coverage.finish_actions(&state, outcome);
        GateDecision::Accept {
            disclosure: self.coverage_state.amended_summary(),
        }
    }

    /// The run ended without an accepted finish (OpenCode stopped, or a budget/turn ceiling tripped).
    /// Runs the coverage exhaustion path so a truncated run still discloses what it never examined
    /// rather than reading as a clean pass. Returns the disclosure note, if any.
    pub fn exhausted(&mut self) -> Option<String> {
        let state = turn_state(self.cycle, self.max_turns);
        let _ = self.coverage.exhausted_actions(&state);
        self.coverage_state.amended_summary()
    }

    /// Whether these gates are running in FAST-tier (no bouncing, no disclosure) — exposed so a host
    /// can assert the tier it configured.
    #[must_use]
    pub fn is_fast(&self) -> bool {
        self.fast
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn before(tool: &str, call_id: &str, args: Value) -> RecorderEvent {
        RecorderEvent {
            kind: "tool.before".into(),
            tool: Some(tool.into()),
            call_id: Some(call_id.into()),
            args: Some(args),
            result: None,
        }
    }

    /// An MCP-shaped `tool.after` (`{content:[{type,text}],isError}`), the shape the mediated review
    /// tools actually return at runtime (recorder ADR-0095 note).
    fn after(tool: &str, call_id: &str, text: &str) -> RecorderEvent {
        RecorderEvent {
            kind: "tool.after".into(),
            tool: Some(tool.into()),
            call_id: Some(call_id.into()),
            args: None,
            result: Some(serde_json::json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })),
        }
    }

    #[test]
    fn normalizes_prefixed_and_self_prefixed_tool_ids() {
        // A bare native name gets the server prefix.
        assert_eq!(normalize_tool_name("lightbridge_read_file"), Some(READ_FILE));
        assert_eq!(normalize_tool_name("lightbridge_finish"), Some(FINISH));
        // A name already `lightbridge_`-prefixed natively → server double-prefix; strip one, match.
        assert_eq!(
            normalize_tool_name("lightbridge_lightbridge_vector_semantic_search"),
            Some(VECTOR_SEMANTIC_SEARCH)
        );
        // An exact, unprefixed id still matches (defensive — separator-independent).
        assert_eq!(normalize_tool_name("add_review_comment"), Some(ADD_REVIEW_COMMENT));
        // A longer identifier that merely *ends in* a known name must NOT match (boundary check).
        assert_eq!(normalize_tool_name("spread_file"), None);
        // An untracked built-in.
        assert_eq!(normalize_tool_name("grep"), None);
    }

    #[test]
    fn reconstructs_a_cycle_outcome_with_finish_and_finding_text() {
        let events = vec![
            before("lightbridge_read_file", "c1", serde_json::json!({"path": "a.rs"})),
            after("lightbridge_read_file", "c1", "source of a.rs"),
            before(
                "lightbridge_add_review_comment",
                "c2",
                serde_json::json!({"file": "a.rs", "line": 2, "priority": "P1"}),
            ),
            after("lightbridge_add_review_comment", "c2", "recorded finding at a.rs:2"),
            before("lightbridge_finish", "c3", serde_json::json!({"summary": "done"})),
            after("lightbridge_finish", "c3", "Review finished; the host will finalize."),
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
            before("lightbridge_abort", "c1", serde_json::json!({"reason": "cannot review"})),
            after("lightbridge_abort", "c1", "Review aborted: cannot review"),
        ];
        let outcome = cycle_turn_outcome(&events);
        assert_eq!(outcome.abort_reason.as_deref(), Some("cannot review"));
        assert!(!outcome.finish_requested);
    }

    /// Coverage parity: a diff with one source file, a first cycle that finishes without touching it,
    /// must be bounced; a second cycle that reads it is accepted with no disclosure (fully covered).
    #[test]
    fn coverage_bounces_a_premature_finish_then_accepts() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, false);

        let premature = cycle_turn_outcome(&[
            before("lightbridge_finish", "c1", serde_json::json!({"summary": "lgtm"})),
            after("lightbridge_finish", "c1", "finalize"),
        ]);
        match gates.observe_cycle(&premature) {
            GateDecision::RejectFinish(nudge) => assert!(nudge.contains("a.rs")),
            other => panic!("expected a coverage RejectFinish, got {other:?}"),
        }

        let covered = cycle_turn_outcome(&[
            before("lightbridge_read_file", "c2", serde_json::json!({"path": "a.rs"})),
            after("lightbridge_read_file", "c2", "source"),
            before("lightbridge_finish", "c3", serde_json::json!({"summary": "done"})),
            after("lightbridge_finish", "c3", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&covered),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// Refute parity: a P1 finding + finish in one cycle is bounced once; a follow-up cycle that
    /// retracts it and finishes is accepted.
    #[test]
    fn refute_bounces_an_outstanding_p1_once() {
        // No diff files → coverage never bounces, isolating the refute gate.
        let mut gates = ReviewGates::new(vec![], 3, 40, false);

        let record_and_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_add_review_comment",
                "c1",
                serde_json::json!({"file": "a.rs", "line": 2, "priority": "P1", "title": "bug"}),
            ),
            after("lightbridge_add_review_comment", "c1", "recorded finding at a.rs:2"),
            before("lightbridge_finish", "c2", serde_json::json!({"summary": "one bug"})),
            after("lightbridge_finish", "c2", "finalize"),
        ]);
        match gates.observe_cycle(&record_and_finish) {
            GateDecision::RejectFinish(nudge) => assert!(nudge.contains("re-verify") || nudge.contains("DISPROVE")),
            other => panic!("expected a refute RejectFinish, got {other:?}"),
        }

        let retract_and_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_retract_finding",
                "c3",
                serde_json::json!({"file": "a.rs", "line": 2}),
            ),
            after("lightbridge_retract_finding", "c3", "retracted finding at a.rs:2"),
            before("lightbridge_finish", "c4", serde_json::json!({"summary": "no findings"})),
            after("lightbridge_finish", "c4", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&retract_and_finish),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// The real disclosure path (ADR-0069 / #306): once the coverage bounce budget is spent, the next
    /// finish is ACCEPTED and its summary is amended with a note naming the source file the run never
    /// examined — so a run that gave up on covering a file doesn't read as a clean pass.
    #[test]
    fn accepts_after_bounce_budget_but_discloses_unexamined_source() {
        // One bounce allowed.
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 1, 40, false);

        let first_finish = cycle_turn_outcome(&[
            before("lightbridge_finish", "c1", serde_json::json!({"summary": "lgtm"})),
            after("lightbridge_finish", "c1", "finalize"),
        ]);
        assert!(matches!(
            gates.observe_cycle(&first_finish),
            GateDecision::RejectFinish(_)
        ));

        // Second finish, a.rs still unexamined: the bounce budget is spent, so it's accepted — but the
        // summary carries the coverage note.
        let second_finish = cycle_turn_outcome(&[
            before("lightbridge_finish", "c2", serde_json::json!({"summary": "still lgtm"})),
            after("lightbridge_finish", "c2", "finalize"),
        ]);
        match gates.observe_cycle(&second_finish) {
            GateDecision::Accept { disclosure: Some(note) } => {
                assert!(note.contains("examined 0 of 1"), "{note}");
                assert!(note.contains("a.rs"), "{note}");
            }
            other => panic!("expected Accept with a disclosure, got {other:?}"),
        }
    }

    /// Native parity for the no-summary case: a run that stops with no finish at all has no summary to
    /// amend, so exhaustion produces no disclosure (the caller posts its own truncation note).
    #[test]
    fn exhaustion_without_a_finish_produces_no_disclosure() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, false);
        let idle = cycle_turn_outcome(&[before(
            "lightbridge_report_progress",
            "c1",
            serde_json::json!({"note": "looking"}),
        )]);
        assert_eq!(gates.observe_cycle(&idle), GateDecision::Proceed);
        assert_eq!(gates.exhausted(), None);
    }

    /// Fast tier never bounces and never discloses (parity with `run_review`'s fast path).
    #[test]
    fn fast_tier_accepts_immediately() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, true);
        assert!(gates.is_fast());
        let finish = cycle_turn_outcome(&[before(
            "lightbridge_finish",
            "c1",
            serde_json::json!({"summary": "quick"}),
        )]);
        assert_eq!(
            gates.observe_cycle(&finish),
            GateDecision::Accept { disclosure: None }
        );
    }
}
