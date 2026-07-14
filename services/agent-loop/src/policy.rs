//! Built-in `TurnPolicy` implementations: context trimming, wind-down, and read/turn budgets.

use lci_agent_tools::{ReadKind, ToolKind, TurnFilter};

use crate::budget::{convergence_filter, estimate_tokens, trim_tool_history, winddown_turn};
use crate::turn::{Nudge, PolicyAction, TurnPolicy, TurnState};

pub struct ContextWindowTrim {
    context_window: Option<usize>,
    announced: bool,
}

impl ContextWindowTrim {
    #[must_use]
    pub fn new(context_window: Option<usize>) -> Self {
        Self {
            context_window,
            announced: false,
        }
    }
}

impl TurnPolicy for ContextWindowTrim {
    fn name(&self) -> &'static str {
        "context_trim"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        let Some(window) = self.context_window else {
            return Vec::new();
        };
        let target = (window as f64 * 0.75) as usize;
        let estimate = estimate_tokens(state.messages, state.base_tools);
        if estimate < target {
            return Vec::new();
        }
        let mut preview = state.messages.to_vec();
        trim_tool_history(&mut preview, state.base_tools, target);
        let remains_over = estimate_tokens(&preview, state.base_tools) >= target;
        let convergence = match (self.announced, remains_over) {
            (true, over) => over.then(|| (convergence_filter(), None, None)),
            (false, true) => {
                self.announced = true;
                Some((
                    convergence_filter(),
                    Some(Nudge("⏳ Context budget nearly full. Stop investigating — record any remaining findings now with add_review_comment/add_comment, then call `finish` with your overall verdict. (The investigation tools are no longer available.)".into())),
                    Some(serde_json::json!({"reason": "Context budget nearly full"})),
                ))
            }
            (false, false) => None,
        };
        vec![PolicyAction::TrimHistory {
            target_tokens: target,
            convergence,
        }]
    }
}

pub struct WindDown {
    max_turns: usize,
    max_batches: usize,
    announced: bool,
    disabled: bool,
    assembly_filter: TurnFilter,
}

impl WindDown {
    #[must_use]
    pub fn new(max_turns: usize, max_batches: usize) -> Self {
        Self {
            max_turns,
            max_batches,
            announced: false,
            disabled: false,
            assembly_filter: TurnFilter::all(),
        }
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Apply an assembly-owned restriction whenever convergence is active (for example, the
    /// review diff-absent gate for inline-only effects).
    #[must_use]
    pub fn with_filter(mut self, filter: TurnFilter) -> Self {
        self.assembly_filter = filter;
        self
    }
}

impl TurnPolicy for WindDown {
    fn name(&self) -> &'static str {
        "wind_down"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if self.disabled || state.converging {
            return Vec::new();
        }
        let boundary = winddown_turn(self.max_turns);
        let batches_spent = state.stats.batches >= self.max_batches;
        if state.turn < boundary && !batches_spent {
            return Vec::new();
        }
        let mut narrowed = convergence_filter();
        narrowed.narrow(&self.assembly_filter);
        let mut actions = Vec::new();
        if !self.announced {
            self.announced = true;
            let reason = if batches_spent && state.turn < boundary {
                format!(
                    "Investigation batch budget spent ({}/{} batches)",
                    state.stats.batches, self.max_batches
                )
            } else {
                format!(
                    "Turn budget almost spent (turn {}/{})",
                    state.turn, self.max_turns
                )
            };
            actions.push(PolicyAction::Record {
                name: None,
                detail: serde_json::json!({"reason": reason}),
            });
            actions.push(PolicyAction::Converge {
                filter: narrowed,
                nudge: Nudge(format!(
                    "⏳ {reason}. Stop investigating — record any remaining findings now with add_review_comment/add_comment, then call `finish` with your overall verdict. (The investigation tools are no longer available.)"
                )),
            });
        } else {
            actions.push(PolicyAction::Narrow(narrowed));
        }
        actions
    }
}

pub struct ReadBudgets {
    max_files: usize,
    max_searches: usize,
    files_announced: bool,
    searches_announced: bool,
    disabled: bool,
}

impl ReadBudgets {
    #[must_use]
    pub fn new(max_files: usize, max_searches: usize) -> Self {
        Self {
            max_files,
            max_searches,
            files_announced: false,
            searches_announced: false,
            disabled: false,
        }
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl TurnPolicy for ReadBudgets {
    fn name(&self) -> &'static str {
        "read_budgets"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if self.disabled || state.converging {
            return Vec::new();
        }
        let files_spent = state.stats.files_read >= self.max_files;
        let searches_spent = state.stats.searches >= self.max_searches;
        let mut actions = Vec::new();
        if files_spent {
            actions.push(PolicyAction::Narrow(
                TurnFilter::all().without_kind(ToolKind::ReadOnly(ReadKind::File)),
            ));
            if !self.files_announced {
                self.files_announced = true;
                actions.push(PolicyAction::Record {
                    name: Some("read_file_budget"),
                    detail: serde_json::json!({"files_read": state.stats.files_read}),
                });
                actions.push(PolicyAction::Inject(Nudge(format!(
                    "📄 You've read {} files (the read_file budget). Stop opening files — work from what you have, record findings, and head toward `finish`.",
                    state.stats.files_read
                ))));
            }
        }
        if searches_spent {
            actions.push(PolicyAction::Narrow(
                TurnFilter::all().without_kind(ToolKind::ReadOnly(ReadKind::Retrieval)),
            ));
            if !self.searches_announced {
                self.searches_announced = true;
                actions.push(PolicyAction::Record {
                    name: Some("retrieval_budget"),
                    detail: serde_json::json!({"searches": state.stats.searches}),
                });
                actions.push(PolicyAction::Inject(Nudge(format!(
                    "🔎 You've run {} searches (the retrieval budget). Stop searching — record findings from what you've found and head toward `finish`.",
                    state.stats.searches
                ))));
            }
        }
        actions
    }
}

pub struct TurnBudget {
    halfway: usize,
    announced: bool,
    disabled: bool,
}

impl TurnBudget {
    #[must_use]
    pub fn new(max_turns: usize) -> Self {
        Self {
            halfway: max_turns / 2,
            announced: false,
            disabled: false,
        }
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl TurnPolicy for TurnBudget {
    fn name(&self) -> &'static str {
        "halfway"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if self.disabled
            || self.announced
            || state.converging
            || self.halfway == 0
            || state.turn < self.halfway
        {
            return Vec::new();
        }
        self.announced = true;
        vec![
            PolicyAction::Record {
                name: None,
                detail: serde_json::json!({}),
            },
            PolicyAction::Inject(Nudge("You're past halfway on your turn budget — start converging: record what you've found and head toward `finish`.".into())),
        ]
    }
}
