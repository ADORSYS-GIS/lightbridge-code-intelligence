//! The review drive loop, abstracted over the OpenCode session so the orchestration is testable
//! without a live opencode.
//!
//! [`run_review_loop`] is the host-independent core: prompt the session, reconstruct the cycle's
//! [`TurnOutcome`](lci_agent_loop::TurnOutcome) from the recorder delta, run it through the
//! [`ReviewDriver`], and either re-prompt (a gate bounce or keep-going nudge) or finalize. The only
//! thing it needs from the world is [`ReviewSession::prompt`] — "run one `session/prompt` cycle and
//! give me the recorder events it produced" — so the agent-runner host implements that over
//! `AcpClient` + recorder-file tailing while the tests drive it with a scripted fake. The transcript
//! is reconstructed separately by the host from the full recorder file (ADR-0034), regardless of how
//! the loop ends, so it isn't threaded through here.

use super::driver::{DriveAction, ReviewDriver, ReviewResolution};
use super::recorder::{RecorderEvent, cycle_turn_outcome};

/// One OpenCode `session/prompt` cycle: send `text`, wait for the internal loop to return, and yield
/// the recorder events (ADR-0095) appended during that cycle — the completeness authority the gates
/// read (see [`super::recorder`]).
///
/// `async fn` in a trait is fine here: the loop is awaited inline in the run-once host (never spawned
/// across threads), so the missing `Send` bound the lint warns about is not needed.
#[allow(async_fn_in_trait)]
pub trait ReviewSession {
    async fn prompt(&mut self, text: &str) -> anyhow::Result<Vec<RecorderEvent>>;
}

/// Drive a review to resolution over `session`, starting from `first_prompt` (the rendered review
/// task) and re-prompting with each gate nudge until the driver finalizes. Returns how the run
/// resolved (Finished / Exhausted / Aborted); a transport error from the session propagates as `Err`
/// (the host still submits whatever transcript the recorder captured).
pub async fn run_review_loop<S: ReviewSession>(
    session: &mut S,
    driver: &mut ReviewDriver,
    first_prompt: &str,
) -> anyhow::Result<ReviewResolution> {
    let mut prompt = first_prompt.to_string();
    loop {
        let events = session.prompt(&prompt).await?;
        let outcome = cycle_turn_outcome(&events);
        match driver.on_cycle(&outcome) {
            DriveAction::Prompt(next) => prompt = next,
            DriveAction::Finalize(resolution) => return Ok(resolution),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::gates::ReviewGates;
    use super::super::test_support::{after, before};
    use super::*;

    /// A scripted session: each `prompt` call returns the next pre-baked cycle of recorder events.
    struct FakeSession {
        cycles: std::collections::VecDeque<Vec<RecorderEvent>>,
        prompts_seen: Vec<String>,
    }

    impl FakeSession {
        fn new(cycles: Vec<Vec<RecorderEvent>>) -> Self {
            Self {
                cycles: cycles.into(),
                prompts_seen: Vec::new(),
            }
        }
    }

    impl ReviewSession for FakeSession {
        async fn prompt(&mut self, text: &str) -> anyhow::Result<Vec<RecorderEvent>> {
            self.prompts_seen.push(text.to_string());
            self.cycles
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("fake session ran out of scripted cycles"))
        }
    }

    fn read_and_finish() -> Vec<RecorderEvent> {
        vec![
            before(
                "lightbridge_read_file",
                "r",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "r", "source"),
            before(
                "lightbridge_finish",
                "f",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "f", "finalize"),
        ]
    }

    fn bare_finish() -> Vec<RecorderEvent> {
        vec![
            before(
                "lightbridge_finish",
                "f0",
                serde_json::json!({"summary": "lgtm"}),
            ),
            after("lightbridge_finish", "f0", "finalize"),
        ]
    }

    #[tokio::test]
    async fn covered_review_finishes_in_one_cycle() {
        let mut session = FakeSession::new(vec![read_and_finish()]);
        let mut driver = ReviewDriver::new(ReviewGates::new(vec!["a.rs".into()], 3, 40, false), 8);
        let resolution = run_review_loop(&mut session, &mut driver, "review this PR")
            .await
            .unwrap();
        assert_eq!(resolution, ReviewResolution::Finished { disclosure: None });
        assert_eq!(session.prompts_seen.len(), 1);
        assert_eq!(session.prompts_seen[0], "review this PR");
    }

    #[tokio::test]
    async fn coverage_bounce_reprompts_then_finishes() {
        // Cycle 1 finishes without touching a.rs → bounced; cycle 2 reads it → finished.
        let mut session = FakeSession::new(vec![bare_finish(), read_and_finish()]);
        let mut driver = ReviewDriver::new(ReviewGates::new(vec!["a.rs".into()], 3, 40, false), 8);
        let resolution = run_review_loop(&mut session, &mut driver, "review this PR")
            .await
            .unwrap();
        assert_eq!(resolution, ReviewResolution::Finished { disclosure: None });
        // Two prompts: the task, then the coverage nudge (which names the unexamined file).
        assert_eq!(session.prompts_seen.len(), 2);
        assert!(session.prompts_seen[1].contains("a.rs"));
    }

    #[tokio::test]
    async fn model_that_never_finishes_exhausts_at_the_reprompt_budget() {
        // Every cycle just reports progress — never finishes; the driver keeps nudging until its
        // re-prompt budget (2) is spent, then exhausts.
        let idle = || {
            vec![before(
                "lightbridge_report_progress",
                "p",
                serde_json::json!({"note": "thinking"}),
            )]
        };
        let mut session = FakeSession::new(vec![idle(), idle(), idle()]);
        let mut driver = ReviewDriver::new(ReviewGates::new(vec![], 3, 40, false), 2);
        let resolution = run_review_loop(&mut session, &mut driver, "review this PR")
            .await
            .unwrap();
        assert!(matches!(resolution, ReviewResolution::Exhausted { .. }));
        assert_eq!(session.prompts_seen.len(), 2);
    }

    #[tokio::test]
    async fn a_transport_error_propagates() {
        // No scripted cycles → the fake errors on the first prompt, and the loop surfaces it.
        let mut session = FakeSession::new(vec![]);
        let mut driver = ReviewDriver::new(ReviewGates::new(vec![], 3, 40, false), 8);
        assert!(
            run_review_loop(&mut session, &mut driver, "review this PR")
                .await
                .is_err()
        );
    }
}
