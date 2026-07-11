//! `TurnPolicy` — the ordered, monotonic guards the engine runs before each turn, plus the *generic*
//! policies (companion doc §3.6). Review-flavored policies (fast-tier, coverage, scratchpad) live in
//! the review assembly; everything here reads only [`TurnState`] — turn index, budgets, token
//! estimate — and never diffs, changed files, or tiers.

use lci_agent_tools::{ReadKind, ToolKind, TurnFilter};
use lci_agent_types::ChatMessage;
use serde_json::{Value, json};

use crate::LoopLimits;

/// The signals a policy observes before a turn — all derived by the engine from the journaled
/// history, so `before_turn` is pure and replay-safe.
pub struct TurnState<'a> {
    pub turn: usize,
    pub limits: &'a LoopLimits,
    /// Cumulative investigation batches spent through the previous turn.
    pub batches: usize,
    /// Cumulative `read_file` calls spent through the previous turn.
    pub files_read: usize,
    /// Cumulative retrieval calls spent through the previous turn.
    pub searches: usize,
    /// Whether the run has entered wind-down (turn budget, batch budget, or context budget).
    pub in_winddown: bool,
    pub batches_spent: bool,
    pub tokens_spent: bool,
    pub files_spent: bool,
    pub searches_spent: bool,
}

/// What a turn produced, handed to [`TurnPolicy::after_turn`] for stateful bookkeeping.
pub struct TurnOutcome {
    pub tool_calls: usize,
    pub finished: bool,
}

/// A single action a policy asks the engine to take this turn. Merge semantics are fixed and simple:
/// `Narrow` only ever intersects the offered set (a later policy cannot re-widen); `Inject`ed
/// messages concatenate in policy-registration order; any `ForceFinish` ends the loop after the turn.
pub enum PolicyAction {
    /// Tighten the offered tool set (monotonic).
    Narrow(TurnFilter),
    /// Append a message to the conversation this turn (a convergence / budget nudge).
    Inject(ChatMessage),
    /// Record a policy telemetry event (name + detail) to the transcript.
    Emit { name: &'static str, detail: Value },
    /// End the loop after the current turn — the exhausted backstop.
    ForceFinish { reason: &'static str },
}

/// An ordered, `&mut self` guard run before (and after) each turn. Policies are state machines — the
/// ad-hoc `bool` flags of the pre-extraction loop (`winddown_announced`, …), now named and isolated.
pub trait TurnPolicy: Send {
    /// A stable name for tracing and transcripts.
    fn name(&self) -> &'static str;
    /// Actions to apply before the model call this turn.
    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction>;
    /// Bookkeeping after the turn's tools ran. Generic policies do not need it.
    fn after_turn(&mut self, _state: &TurnState<'_>, _outcome: &TurnOutcome) {}
}

/// The wind-down offered set (#137): drop the investigation tools (retrieval, `read_file`, graph) and
/// `report_progress`, keeping only the write tools + `finish`/`abort`. Expressed as kind filters so it
/// is independent of concrete tool names — the review assembly decides which write tools exist (e.g.
/// gating `add_review_comment` on a present diff), and this narrows whatever is registered.
fn winddown_filter() -> TurnFilter {
    TurnFilter::all()
        .without_kind(ToolKind::ReadOnly(ReadKind::Retrieval))
        .without_kind(ToolKind::ReadOnly(ReadKind::File))
        .without_kind(ToolKind::ReadOnly(ReadKind::Knowledge))
        .without_kind(ToolKind::Progress)
}

/// Wind-down convergence (#137): once the budget depletes, narrow to the write/finish set and tell the
/// model (once) to stop investigating and converge. The tool restriction is the real lever; the
/// message just explains why the tools changed.
#[derive(Default)]
pub struct WindDown {
    announced: bool,
}

impl WindDown {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TurnPolicy for WindDown {
    fn name(&self) -> &'static str {
        "wind_down"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if !state.in_winddown {
            return Vec::new();
        }
        let mut actions = vec![PolicyAction::Narrow(winddown_filter())];
        if !self.announced {
            self.announced = true;
            let winddown = state.limits.winddown_turn();
            let why = if state.tokens_spent && state.turn < winddown && !state.batches_spent {
                "Context budget nearly full".to_string()
            } else if state.batches_spent && state.turn < winddown {
                format!(
                    "Investigation batch budget spent ({}/{} batches)",
                    state.batches, state.limits.max_batches
                )
            } else {
                format!(
                    "Turn budget almost spent (turn {}/{})",
                    state.turn, state.limits.max_turns
                )
            };
            actions.push(PolicyAction::Emit {
                name: "wind_down",
                detail: json!({ "reason": why }),
            });
            actions.push(PolicyAction::Inject(ChatMessage::user(format!(
                "⏳ {why}. Stop investigating — record any remaining findings now with \
                 add_review_comment/add_comment, then call `finish` with your overall verdict. (The \
                 investigation tools are no longer available.)"
            ))));
        }
        actions
    }
}

/// Cumulative read budgets (ADR-0042): before full wind-down, drop just the exhausted read category
/// (`read_file` / retrieval) so the model can still record findings and finish while it stops the kind
/// of reading it has used up. Each category announces its exhaustion once.
#[derive(Default)]
pub struct ReadBudgets {
    files_announced: bool,
    searches_announced: bool,
}

impl ReadBudgets {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TurnPolicy for ReadBudgets {
    fn name(&self) -> &'static str {
        "read_budgets"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        // In wind-down the reduced set already excludes both read categories, and the wind-down
        // message supersedes these finer nudges — mirror the pre-extraction `else if` gate.
        if state.in_winddown {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if state.files_spent || state.searches_spent {
            let mut filter = TurnFilter::all();
            if state.files_spent {
                filter = filter.without_kind(ToolKind::ReadOnly(ReadKind::File));
            }
            if state.searches_spent {
                filter = filter.without_kind(ToolKind::ReadOnly(ReadKind::Retrieval));
            }
            actions.push(PolicyAction::Narrow(filter));
        }
        if state.files_spent && !self.files_announced {
            self.files_announced = true;
            actions.push(PolicyAction::Emit {
                name: "read_file_budget",
                detail: json!({ "files_read": state.files_read }),
            });
            actions.push(PolicyAction::Inject(ChatMessage::user(format!(
                "📄 You've read {} files (the read_file budget). Stop opening files — work \
                 from what you have, record findings, and head toward `finish`.",
                state.files_read
            ))));
        }
        if state.searches_spent && !self.searches_announced {
            self.searches_announced = true;
            actions.push(PolicyAction::Emit {
                name: "retrieval_budget",
                detail: json!({ "searches": state.searches }),
            });
            actions.push(PolicyAction::Inject(ChatMessage::user(format!(
                "🔎 You've run {} searches (the retrieval budget). Stop searching — record \
                 findings from what you've found and head toward `finish`.",
                state.searches
            ))));
        }
        actions
    }
}

/// Turn budget (#137): a soft, one-time nudge around the halfway mark. Kept light — the wind-down
/// tool restriction is the real lever. Skipped once wind-down is announced so the two never conflict.
#[derive(Default)]
pub struct TurnBudget {
    halfway_nudged: bool,
}

impl TurnBudget {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TurnPolicy for TurnBudget {
    fn name(&self) -> &'static str {
        "turn_budget"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if state.in_winddown {
            return Vec::new();
        }
        let halfway = state.limits.halfway();
        if !self.halfway_nudged && halfway > 0 && state.turn >= halfway {
            self.halfway_nudged = true;
            return vec![
                PolicyAction::Emit {
                    name: "halfway",
                    detail: json!({}),
                },
                PolicyAction::Inject(ChatMessage::user(
                    "You're past halfway on your turn budget — start converging: record what you've \
                     found and head toward `finish`.",
                )),
            ];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{PolicyAction, ReadBudgets, TurnBudget, TurnPolicy, TurnState, WindDown};
    use crate::LoopLimits;

    fn limits() -> LoopLimits {
        LoopLimits {
            max_turns: 20,
            max_batch_size: 8,
            max_batches: 6,
            max_files_read: 10,
            max_searches: 10,
            context_window: None,
        }
    }

    fn state<'a>(limits: &'a LoopLimits, turn: usize) -> TurnState<'a> {
        TurnState {
            turn,
            limits,
            batches: 0,
            files_read: 0,
            searches: 0,
            in_winddown: false,
            batches_spent: false,
            tokens_spent: false,
            files_spent: false,
            searches_spent: false,
        }
    }

    fn injected_texts(actions: &[PolicyAction]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                PolicyAction::Inject(m) => m.content.clone(),
                _ => None,
            })
            .collect()
    }

    fn emitted_names(actions: &[PolicyAction]) -> Vec<&'static str> {
        actions
            .iter()
            .filter_map(|a| match a {
                PolicyAction::Emit { name, .. } => Some(*name),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn wind_down_narrows_and_announces_exactly_once() {
        let limits = limits();
        let mut policy = WindDown::new();
        let mut s = state(&limits, limits.winddown_turn());
        s.in_winddown = true;

        let first = policy.before_turn(&s);
        assert!(matches!(first[0], PolicyAction::Narrow(_)));
        assert_eq!(emitted_names(&first), vec!["wind_down"]);
        assert!(injected_texts(&first)[0].contains("Stop investigating"));
        assert!(injected_texts(&first)[0].contains("Turn budget almost spent"));

        // Second wind-down turn still narrows, but the message/event fire only once.
        let second = policy.before_turn(&s);
        assert!(matches!(second[0], PolicyAction::Narrow(_)));
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn wind_down_reason_reflects_the_triggering_budget() {
        let limits = limits();
        // Context budget trips before the turn ceiling → "Context budget nearly full".
        let mut policy = WindDown::new();
        let mut s = state(&limits, 3);
        s.in_winddown = true;
        s.tokens_spent = true;
        assert!(injected_texts(&policy.before_turn(&s))[0].contains("Context budget nearly full"));

        // Batch budget trips before the ceiling → the batch reason with counts.
        let mut policy = WindDown::new();
        let mut s = state(&limits, 3);
        s.in_winddown = true;
        s.batches_spent = true;
        s.batches = 6;
        let text = injected_texts(&policy.before_turn(&s));
        assert!(text[0].contains("Investigation batch budget spent (6/6 batches)"));
    }

    #[test]
    fn read_budgets_drop_spent_categories_and_announce_once_each() {
        let limits = limits();
        let mut policy = ReadBudgets::new();
        let mut s = state(&limits, 2);
        s.files_spent = true;
        s.files_read = 10;

        let first = policy.before_turn(&s);
        assert!(matches!(first[0], PolicyAction::Narrow(_)));
        assert_eq!(emitted_names(&first), vec!["read_file_budget"]);
        assert!(injected_texts(&first)[0].contains("Stop opening files"));

        // Already announced files; now searches also spend → only the retrieval nudge is new.
        s.searches_spent = true;
        s.searches = 10;
        let second = policy.before_turn(&s);
        assert_eq!(emitted_names(&second), vec!["retrieval_budget"]);
    }

    #[test]
    fn read_budgets_are_silent_in_winddown() {
        let limits = limits();
        let mut policy = ReadBudgets::new();
        let mut s = state(&limits, 2);
        s.files_spent = true;
        s.in_winddown = true;
        assert!(policy.before_turn(&s).is_empty());
    }

    #[test]
    fn turn_budget_nudges_once_past_halfway() {
        let limits = limits();
        let mut policy = TurnBudget::new();
        assert!(policy.before_turn(&state(&limits, 9)).is_empty());
        let at_halfway = policy.before_turn(&state(&limits, 10));
        assert_eq!(emitted_names(&at_halfway), vec!["halfway"]);
        assert!(injected_texts(&at_halfway)[0].contains("past halfway"));
        // Fires only once.
        assert!(policy.before_turn(&state(&limits, 11)).is_empty());
    }
}
