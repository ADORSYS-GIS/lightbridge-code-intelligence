//! Deterministic support for the R1 legacy-vs-extracted agent comparison.

use std::sync::Mutex;

use lci_agent_tools::{BoxFuture, ReplaySafety, Tool, ToolCx, ToolKind};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
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

pub struct StaticTool {
    spec: ToolSpec,
    kind: ToolKind,
    replay: ReplaySafety,
    outcome: ToolOutcome,
    calls: Mutex<Vec<ToolCallReq>>,
}
impl StaticTool {
    #[must_use]
    pub fn new(spec: ToolSpec, kind: ToolKind, replay: ReplaySafety, outcome: ToolOutcome) -> Self {
        Self {
            spec,
            kind,
            replay,
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }
    #[must_use]
    pub fn calls(&self) -> Vec<ToolCallReq> {
        self.calls.lock().expect("static tool mutex").clone()
    }
}
impl Tool for StaticTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        self.kind
    }
    fn replay(&self) -> ReplaySafety {
        self.replay
    }
    fn call<'a>(&'a self, _: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("static tool mutex")
                .push(call.clone());
            self.outcome.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_tools::{Workspace, WorkspaceError};
    use lci_agent_types::FunctionCallReq;
    use std::path::Path;
    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
    }
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
    fn static_tool_preserves_the_full_call() {
        let tool = StaticTool::new(
            ToolSpec::function("x", "x", serde_json::json!({})),
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
            ToolOutcome::Continue("ok".into()),
        );
        let call = ToolCallReq {
            id: "actual-id".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: "x".into(),
                arguments: "{}".into(),
            },
            extra_content: Some(serde_json::json!({"provider":"opaque"})),
        };
        tool.calls.lock().unwrap().push(call.clone());
        assert_eq!(tool.spec().name(), "x");
        assert_eq!(tool.kind(), ToolKind::Write);
        assert_eq!(tool.replay(), ReplaySafety::NeedsDedupKey);
        assert_eq!(tool.calls(), vec![call]);
    }
    #[tokio::test]
    async fn static_tool_executes_and_captures_the_actual_call() {
        let tool = StaticTool::new(
            ToolSpec::function("x", "x", serde_json::json!({})),
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
            ToolOutcome::Continue("ok".into()),
        );
        let call = ToolCallReq {
            id: "call-id".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: "x".into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        };
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        assert_eq!(
            tool.call(&cx, &call).await,
            ToolOutcome::Continue("ok".into())
        );
        assert_eq!(tool.calls(), vec![call]);
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
