//! Golden scenario fixtures and the legacy-vs-extracted trace-parity harness.
//!
//! [`GoldenScenario`] enumerates the frozen R1 comparison scenarios, each pairing a scripted model
//! transcript ([`GoldenScript`]) with the loop settings it was captured under ([`GoldenSettings`]).
//! [`GoldenHarness`] loads the checked-in [`LegacyTrace`] fixture for a scenario and asserts
//! byte-for-byte parity against an actual run.

use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq, ToolOutcome};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldenScenario {
    PlainConvergeFinish,
    WindDownEntry,
    ContextTrimTrigger,
    FastTierRefusal,
    CoverageBounce,
    ExhaustedBackstop,
}

impl GoldenScenario {
    pub const ALL: [Self; 6] = [
        Self::PlainConvergeFinish,
        Self::WindDownEntry,
        Self::ContextTrimTrigger,
        Self::FastTierRefusal,
        Self::CoverageBounce,
        Self::ExhaustedBackstop,
    ];
    fn fixture(self) -> &'static str {
        match self {
            Self::PlainConvergeFinish => include_str!("../goldens/plain_converge_finish.json"),
            Self::WindDownEntry => include_str!("../goldens/wind_down_entry.json"),
            Self::ContextTrimTrigger => include_str!("../goldens/context_trim_trigger.json"),
            Self::FastTierRefusal => include_str!("../goldens/fast_tier_refusal.json"),
            Self::CoverageBounce => include_str!("../goldens/coverage_bounce.json"),
            Self::ExhaustedBackstop => include_str!("../goldens/exhausted_backstop.json"),
        }
    }

    /// One source of truth for the model replies used by both the legacy and extracted loops.
    #[must_use]
    pub fn script(self) -> GoldenScript {
        let call = |id: &str, name: &str, arguments: &str, extra_content| AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: arguments.into(),
                },
                extra_content,
            }],
            ..Default::default()
        };
        let turns = match self {
            Self::PlainConvergeFinish => vec![
                call(
                    "plain-record",
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"Issue","priority":"P2","category":"quality","body":"body","evidence":"line 2"}"#,
                    Some(serde_json::json!({"provider":{"signature":"opaque"}})),
                ),
                call(
                    "plain-finish",
                    "finish",
                    r#"{"summary":"one finding"}"#,
                    None,
                ),
            ],
            Self::WindDownEntry => vec![
                call(
                    "wind-progress",
                    "report_progress",
                    r#"{"note":"working"}"#,
                    None,
                ),
                call("wind-finish", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::ContextTrimTrigger => vec![
                call("trim-read", "read_file", r#"{"path":"big.txt"}"#, None),
                call(
                    "trim-progress",
                    "report_progress",
                    r#"{"note":"working"}"#,
                    None,
                ),
                call("trim-finish", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::FastTierRefusal => vec![call(
                "fast-illegal",
                "read_file",
                r#"{"path":"a.rs"}"#,
                None,
            )],
            Self::CoverageBounce => vec![
                call(
                    "coverage-finish-1",
                    "finish",
                    r#"{"summary":"early"}"#,
                    None,
                ),
                call("coverage-read", "read_file", r#"{"path":"a.rs"}"#, None),
                call("coverage-finish-2", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::ExhaustedBackstop => vec![AssistantTurn {
                content: Some("still thinking".into()),
                tool_calls: Vec::new(),
                ..Default::default()
            }],
        };
        GoldenScript { turns }
    }

    #[must_use]
    pub fn settings(self) -> GoldenSettings {
        match self {
            Self::PlainConvergeFinish => GoldenSettings::new(5).with_diff(),
            Self::WindDownEntry => GoldenSettings::new(2),
            Self::ContextTrimTrigger => GoldenSettings::new(5)
                .with_diff()
                .with_context_window(2_000),
            Self::FastTierRefusal => GoldenSettings::new(1).with_diff().fast(),
            Self::CoverageBounce => GoldenSettings::new(5).with_diff().with_coverage_bounces(1),
            Self::ExhaustedBackstop => GoldenSettings::new(2),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GoldenScript {
    pub turns: Vec<AssistantTurn>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoldenSettings {
    pub max_turns: usize,
    pub diff_present: bool,
    pub context_window: Option<usize>,
    pub fast: bool,
    pub max_coverage_bounces: usize,
}

impl GoldenSettings {
    fn new(max_turns: usize) -> Self {
        Self {
            max_turns,
            diff_present: false,
            context_window: None,
            fast: false,
            max_coverage_bounces: 3,
        }
    }

    fn with_diff(mut self) -> Self {
        self.diff_present = true;
        self
    }

    fn with_context_window(mut self, context_window: usize) -> Self {
        self.context_window = Some(context_window);
        self
    }

    fn fast(mut self) -> Self {
        self.fast = true;
        self
    }

    fn with_coverage_bounces(mut self, bounces: usize) -> Self {
        self.max_coverage_bounces = bounces;
        self
    }
}

/// Canonical legacy-side trace. Chat requests are the exact JSON bodies observed by wiremock, so
/// messages retain assistant tool calls, tool_call_id, and provider extra_content, while each turn's
/// complete descriptions/schemas/order are frozen under `tools` in that request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LegacyTrace {
    pub scenario: GoldenScenario,
    pub chat_requests: Vec<serde_json::Value>,
    pub calls: Vec<ObservedCall>,
    pub policy_events: Vec<serde_json::Value>,
    pub control_plane_writes: Vec<ObservedWrite>,
    pub outcome: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObservedCall {
    pub turn: usize,
    pub call: ToolCallReq,
    pub outcome: ToolOutcome,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObservedWrite {
    pub endpoint: String,
    pub body: serde_json::Value,
}

pub struct GoldenHarness;
impl GoldenHarness {
    #[must_use]
    pub fn expected(scenario: GoldenScenario) -> LegacyTrace {
        serde_json::from_str(scenario.fixture()).expect("checked-in legacy trace is valid JSON")
    }
    #[must_use]
    pub fn canonical_bytes(trace: &LegacyTrace) -> Vec<u8> {
        serde_json::to_vec_pretty(trace).expect("legacy trace serializes")
    }
    pub fn assert_fixture(scenario: GoldenScenario, actual: &LegacyTrace) {
        let expected = Self::expected(scenario);
        assert_eq!(
            Self::canonical_bytes(actual),
            Self::canonical_bytes(&expected),
            "actual run_native_agent trace changed for {scenario:?}"
        );
    }
    pub fn assert_parity(legacy: &LegacyTrace, extracted: &LegacyTrace) {
        assert_eq!(
            Self::canonical_bytes(extracted),
            Self::canonical_bytes(legacy),
            "extracted loop changed the canonical legacy trace"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_list_is_frozen() {
        assert_eq!(GoldenScenario::ALL.len(), 6);
        for scenario in GoldenScenario::ALL {
            let trace = GoldenHarness::expected(scenario);
            assert_eq!(trace.scenario, scenario);
            assert!(!trace.chat_requests.is_empty());
            GoldenHarness::assert_fixture(scenario, &trace);
        }
    }
    #[test]
    fn parity_detects_protocol_drift() {
        let base = LegacyTrace {
            scenario: GoldenScenario::ExhaustedBackstop,
            chat_requests: vec![],
            calls: vec![],
            policy_events: vec![],
            control_plane_writes: vec![],
            outcome: serde_json::json!("exhausted"),
        };
        let mut changed = base.clone();
        changed.outcome = serde_json::json!("finished");
        assert!(
            std::panic::catch_unwind(|| GoldenHarness::assert_parity(&base, &changed)).is_err()
        );
    }
    #[test]
    fn frozen_fixture_rejects_full_tool_spec_drift() {
        let mut actual = GoldenHarness::expected(GoldenScenario::PlainConvergeFinish);
        actual.chat_requests[0]["tools"][0]["function"]["description"] =
            serde_json::json!("drifted");
        assert!(
            std::panic::catch_unwind(|| GoldenHarness::assert_fixture(
                GoldenScenario::PlainConvergeFinish,
                &actual
            ))
            .is_err()
        );
    }
}
