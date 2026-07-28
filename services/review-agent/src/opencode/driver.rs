//! Pure drive-loop control for the OpenCode review host.
//!
//! [`ReviewDriver`] wraps [`ReviewGates`] with the supervisor's re-prompt budget: given each observed
//! cycle outcome, it decides whether to re-prompt (a gate bounce, or a keep-going nudge) or finalize.
//! Keeping this pure keeps the async transport shell (spawn OpenCode / `session/prompt` / recorder
//! tail) a thin I/O layer over a testable core.

use lci_agent_loop::TurnOutcome;

use super::gates::{GateDecision, ReviewGates};

/// The keep-reviewing nudge sent when a cycle ends without the model calling `finish` or `abort` —
/// the OpenCode-cycle analogue of the native loop re-prompting a turn that produced no terminal
/// signal (`LoopLimits::no_tool_nudge`).
const CONTINUE_NUDGE: &str = "You have not finished the review. Continue investigating the changed \
files and record findings with add_review_comment, then call finish with your verdict — or abort if \
you genuinely cannot review this change. Do not stop until you call finish or abort.";

/// How a driven review run resolved — maps 1:1 onto the host's `ReviewOutcome` (Finished / Exhausted
/// / Aborted) so `finalize_review_outcome` stays untouched at the cutover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewResolution {
    /// The model finished and every quality gate accepted it. `disclosure` is the coverage note to
    /// append to the posted summary, if any.
    Finished { disclosure: Option<String> },
    /// The re-prompt budget ran out before an accepted finish (OpenCode kept stopping short). Still
    /// finalizes so buffered findings post; `disclosure` carries any coverage note.
    Exhausted { disclosure: Option<String> },
    /// The model called `abort` — incomplete and unverified; the host clears findings and posts the
    /// reason.
    Aborted(String),
}

/// What the transport host should do after one observed OpenCode cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveAction {
    /// Send this text as the next `session/prompt` (a gate bounce, or the keep-reviewing nudge), then
    /// observe the next cycle.
    Prompt(String),
    /// The run resolved; finalize with this outcome.
    Finalize(ReviewResolution),
}

/// Pure drive-loop control for the OpenCode review host.
pub struct ReviewDriver {
    gates: ReviewGates,
    /// Supervisor re-prompt budget: how many OpenCode cycles the host will drive before declaring
    /// exhaustion. Distinct from — and much smaller than — the native turn budget the gates carry for
    /// the coverage wind-down threshold (one cycle is a whole `session/prompt`, i.e. many model turns).
    max_cycles: usize,
    cycles: usize,
}

impl ReviewDriver {
    #[must_use]
    pub fn new(gates: ReviewGates, max_cycles: usize) -> Self {
        Self {
            gates,
            max_cycles: max_cycles.max(1),
            cycles: 0,
        }
    }

    /// Consume one observed cycle and decide the next action. Abort short-circuits before any gate (an
    /// aborted run is unverified). A gate bounce re-prompts with the nudge; an accepted finish
    /// finalizes; a cycle that stopped without a terminal signal re-prompts to keep going until the
    /// re-prompt budget is spent, then finalizes as exhausted.
    pub fn on_cycle(&mut self, outcome: &TurnOutcome) -> DriveAction {
        self.cycles += 1;
        if let Some(reason) = &outcome.abort_reason {
            return DriveAction::Finalize(ReviewResolution::Aborted(reason.clone()));
        }
        match self.gates.observe_cycle(outcome) {
            // A gate-accepted finish always wins — never turn a valid finish into exhaustion.
            GateDecision::Accept { disclosure } => {
                DriveAction::Finalize(ReviewResolution::Finished { disclosure })
            }
            // Otherwise `max_cycles` is the HARD ceiling on re-prompts, enforced for BOTH a gate
            // bounce and a keep-going nudge (gemini #446): the gates are internally bounded today
            // (coverage `max_bounces` + the refute one-shot), but the driver's own budget must be the
            // authoritative backstop so a future unbounded gate — or a `max_cycles` set below the
            // bounce budget — can never spin the loop / burn unbounded tokens.
            _ if self.cycles >= self.max_cycles => {
                DriveAction::Finalize(ReviewResolution::Exhausted {
                    disclosure: self.gates.exhausted(),
                })
            }
            GateDecision::RejectFinish(nudge) => DriveAction::Prompt(nudge),
            GateDecision::Proceed => DriveAction::Prompt(CONTINUE_NUDGE.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::recorder::cycle_turn_outcome;
    use super::super::test_support::{after, before};
    use super::*;

    /// Driver: a coverage bounce re-prompts with the nudge; the follow-up covered finish finalizes as
    /// Finished.
    #[test]
    fn driver_bounces_then_finalizes_finished() {
        let gates = ReviewGates::new(vec!["a.rs".into()], 3, 40);
        let mut driver = ReviewDriver::new(gates, 8);

        let premature = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "f",
                serde_json::json!({ "summary": "lgtm" }),
            ),
            after("lightbridge_finish", "f", "finalize"),
        ]);
        match driver.on_cycle(&premature) {
            DriveAction::Prompt(nudge) => assert!(nudge.contains("a.rs")),
            other => panic!("expected a bounce Prompt, got {other:?}"),
        }
        let covered = cycle_turn_outcome(&[
            before(
                "lightbridge_read_file",
                "r",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "r", "source"),
            before(
                "lightbridge_finish",
                "f2",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "f2", "finalize"),
        ]);
        assert_eq!(
            driver.on_cycle(&covered),
            DriveAction::Finalize(ReviewResolution::Finished { disclosure: None })
        );
    }

    /// Driver: an abort short-circuits straight to Aborted, without consulting the gates.
    #[test]
    fn driver_finalizes_aborted_immediately() {
        let gates = ReviewGates::new(vec!["a.rs".into()], 3, 40);
        let mut driver = ReviewDriver::new(gates, 8);
        let abort = cycle_turn_outcome(&[
            before(
                "lightbridge_abort",
                "a",
                serde_json::json!({"reason": "no PR diff"}),
            ),
            after("lightbridge_abort", "a", "aborted"),
        ]);
        assert_eq!(
            driver.on_cycle(&abort),
            DriveAction::Finalize(ReviewResolution::Aborted("no PR diff".into()))
        );
    }

    /// Driver: repeated gate bounces are ALSO capped by `max_cycles` (gemini #446). With a generous
    /// coverage bounce budget but `max_cycles = 2`, a model that keeps finishing without covering the
    /// file is bounced once, then exhausted at the budget — the re-prompt loop can't overrun the ceiling.
    #[tokio::test]
    async fn repeated_bounces_are_capped_by_max_cycles() {
        // Coverage bounce budget 10 ≫ max_cycles 2, so the gate would keep bouncing if unchecked.
        let gates = ReviewGates::new(vec!["a.rs".into()], 10, 40);
        let mut driver = ReviewDriver::new(gates, 2);
        let premature = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "f0",
                serde_json::json!({"summary": "lgtm"}),
            ),
            after("lightbridge_finish", "f0", "finalize"),
        ]);
        // Cycle 1: bounce.
        assert!(matches!(
            driver.on_cycle(&premature),
            DriveAction::Prompt(_)
        ));
        // Cycle 2: budget spent → exhausted, NOT another bounce.
        assert!(matches!(
            driver.on_cycle(&premature),
            DriveAction::Finalize(ReviewResolution::Exhausted { .. })
        ));
    }

    /// Driver: a model that keeps stopping without a terminal signal is re-prompted to keep going,
    /// then finalized as Exhausted once the re-prompt budget is spent — buffered findings still post.
    #[test]
    fn driver_exhausts_after_budget_of_no_finish_cycles() {
        let gates = ReviewGates::new(vec![], 3, 40);
        let mut driver = ReviewDriver::new(gates, 2);
        let idle = cycle_turn_outcome(&[before(
            "lightbridge_report_progress",
            "p",
            serde_json::json!({"note": "thinking"}),
        )]);
        // Cycle 1: under budget → keep-going nudge.
        match driver.on_cycle(&idle) {
            DriveAction::Prompt(nudge) => assert!(nudge.contains("finish or abort")),
            other => panic!("expected a keep-going Prompt, got {other:?}"),
        }
        // Cycle 2: budget spent → exhausted.
        assert_eq!(
            driver.on_cycle(&idle),
            DriveAction::Finalize(ReviewResolution::Exhausted { disclosure: None })
        );
    }
}
