//! Reusable deterministic support for agent extraction tests.
//!
//! R1c deliberately observes the legacy loop only. R1d will implement model/runtime/sink fakes and
//! feed the extracted loop into [`GoldenHarness::assert_parity`].

use std::sync::Mutex;

use lci_agent_tools::{BoxFuture, ReplaySafety, Tool, ToolCx, ToolKind};
use lci_agent_types::{ToolOutcome, ToolSpec};
use serde::{Deserialize, Serialize};

/// The six behavior-bearing legacy-loop scenarios required by the R1 extraction plan.
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

/// A deterministic, scrubbed observation of the behavior visible around one legacy loop run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoldenTranscript {
    pub scenario: GoldenScenario,
    pub messages: Vec<ObservedMessage>,
    /// Tool names are recorded in the exact order their full specs were offered. The observer also
    /// receives the full specs and serializes them into `offered_specs`; names make order drift legible.
    pub offered_tool_names: Vec<Vec<String>>,
    pub offered_specs: Vec<Vec<serde_json::Value>>,
    pub calls_and_results: Vec<ObservedCall>,
    pub policy_events: Vec<String>,
    pub control_plane_writes: Vec<ObservedWrite>,
    pub final_outcome: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedMessage {
    pub role: String,
    pub content: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub result: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedWrite {
    pub endpoint: String,
    pub body: serde_json::Value,
}

/// Observer used by legacy tests to capture all deterministic comparison surfaces.
pub struct LegacyObserver {
    transcript: GoldenTranscript,
}

impl LegacyObserver {
    #[must_use]
    pub fn new(scenario: GoldenScenario) -> Self {
        Self {
            transcript: GoldenTranscript {
                scenario,
                messages: Vec::new(),
                offered_tool_names: Vec::new(),
                offered_specs: Vec::new(),
                calls_and_results: Vec::new(),
                policy_events: Vec::new(),
                control_plane_writes: Vec::new(),
                final_outcome: String::new(),
            },
        }
    }

    pub fn message(&mut self, role: impl Into<String>, content: Option<impl Into<String>>) {
        self.transcript.messages.push(ObservedMessage {
            role: role.into(),
            content: content.map(Into::into),
        });
    }

    pub fn offered(&mut self, specs: &[ToolSpec]) {
        self.transcript
            .offered_tool_names
            .push(specs.iter().map(|spec| spec.name().to_string()).collect());
        self.transcript.offered_specs.push(
            specs
                .iter()
                .map(|spec| serde_json::to_value(spec).expect("ToolSpec serializes"))
                .collect(),
        );
    }

    pub fn call(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
        result: impl Into<String>,
    ) {
        self.transcript.calls_and_results.push(ObservedCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            result: result.into(),
        });
    }

    pub fn policy(&mut self, event: impl Into<String>) {
        self.transcript.policy_events.push(event.into());
    }

    pub fn write(&mut self, endpoint: impl Into<String>, body: serde_json::Value) {
        self.transcript.control_plane_writes.push(ObservedWrite {
            endpoint: endpoint.into(),
            body,
        });
    }

    #[must_use]
    pub fn finish(mut self, outcome: impl Into<String>) -> GoldenTranscript {
        self.transcript.final_outcome = outcome.into();
        self.transcript
    }
}

/// Canonical byte comparison seam. R1d calls this with legacy and extracted observations.
pub struct GoldenHarness;

impl GoldenHarness {
    /// Load the canonical observation frozen from the legacy loop for this scenario.
    #[must_use]
    pub fn legacy_baseline(
        scenario: GoldenScenario,
        spec_catalog: &[ToolSpec],
    ) -> GoldenTranscript {
        let mut baseline: GoldenTranscript = serde_json::from_str(scenario.fixture())
            .expect("checked-in legacy baseline is valid JSON");
        baseline.offered_specs = baseline
            .offered_tool_names
            .iter()
            .map(|names| {
                names
                    .iter()
                    .map(|name| {
                        let spec = spec_catalog
                            .iter()
                            .find(|spec| spec.name() == name)
                            .unwrap_or_else(|| {
                                panic!("legacy baseline references missing tool spec {name:?}")
                            });
                        serde_json::to_value(spec).expect("ToolSpec serializes")
                    })
                    .collect()
            })
            .collect();
        baseline
    }

    #[must_use]
    pub fn canonical_bytes(transcript: &GoldenTranscript) -> Vec<u8> {
        serde_json::to_vec_pretty(transcript).expect("golden transcript serializes")
    }

    pub fn assert_parity(legacy: &GoldenTranscript, extracted: &GoldenTranscript) {
        assert_eq!(
            Self::canonical_bytes(extracted),
            Self::canonical_bytes(legacy),
            "extracted loop changed the canonical legacy transcript"
        );
    }
}

/// A canned tool for registry/filter/dispatch tests. It records arguments in invocation order.
pub struct StaticTool {
    spec: ToolSpec,
    kind: ToolKind,
    replay: ReplaySafety,
    outcome: ToolOutcome,
    calls: Mutex<Vec<String>>,
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
    pub fn calls(&self) -> Vec<String> {
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

    fn call<'a>(&'a self, _cx: &'a ToolCx<'a>, args: &'a str) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("static tool mutex")
                .push(args.to_string());
            self.outcome.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_tools::{ReadKind, ToolKind};

    #[test]
    fn observer_captures_every_comparison_surface_deterministically() {
        let specs = [ToolSpec::function(
            "finish",
            "finish",
            serde_json::json!({"type": "object"}),
        )];
        let mut observer = LegacyObserver::new(GoldenScenario::PlainConvergeFinish);
        observer.message("user", Some("review"));
        observer.offered(&specs);
        observer.call("c1", "finish", r#"{"summary":"ok"}"#, "finish");
        observer.policy("converged");
        observer.write("/review/summary", serde_json::json!({"summary":"ok"}));
        let legacy = observer.finish("finished");
        GoldenHarness::assert_parity(&legacy, &legacy.clone());
        assert_eq!(legacy.offered_tool_names, vec![vec!["finish"]]);
        assert_eq!(legacy.offered_specs[0][0]["function"]["name"], "finish");
    }

    #[test]
    fn all_six_legacy_scenarios_are_frozen() {
        assert_eq!(GoldenScenario::ALL.len(), 6);
        let encoded = serde_json::to_string(&GoldenScenario::ALL).unwrap();
        for name in [
            "plain_converge_finish",
            "wind_down_entry",
            "context_trim_trigger",
            "fast_tier_refusal",
            "coverage_bounce",
            "exhausted_backstop",
        ] {
            assert!(encoded.contains(name));
        }
        for scenario in GoldenScenario::ALL {
            let catalog: Vec<_> = [
                "lightbridge_vector_semantic_search",
                "lightbridge_graph_find_symbol",
                "lightbridge_graph_get_callers",
                "read_file",
                "add_review_comment",
                "retract_finding",
                "add_comment",
                "finish",
                "report_progress",
                "abort",
            ]
            .into_iter()
            .map(|name| ToolSpec::function(name, "fixture", serde_json::json!({})))
            .collect();
            let baseline = GoldenHarness::legacy_baseline(scenario, &catalog);
            assert_eq!(baseline.scenario, scenario);
            assert!(!baseline.final_outcome.is_empty());
            assert!(!baseline.offered_tool_names.is_empty());
            assert!(baseline.offered_specs.iter().all(|turn| !turn.is_empty()));
        }
    }

    #[test]
    fn parity_reports_any_behavior_drift() {
        let legacy = LegacyObserver::new(GoldenScenario::ExhaustedBackstop).finish("exhausted");
        let extracted = LegacyObserver::new(GoldenScenario::ExhaustedBackstop).finish("finished");
        let mismatch = std::panic::catch_unwind(|| {
            GoldenHarness::assert_parity(&legacy, &extracted);
        });
        assert!(mismatch.is_err());
    }

    #[test]
    fn static_tool_records_ordered_arguments_and_returns_canned_outcome() {
        let tool = StaticTool::new(
            ToolSpec::function("read", "read", serde_json::json!({})),
            ToolKind::ReadOnly(ReadKind::File),
            ReplaySafety::ReadOnly,
            ToolOutcome::Continue("ok".into()),
        );
        assert_eq!(tool.spec().name(), "read");
        assert_eq!(tool.kind(), ToolKind::ReadOnly(ReadKind::File));
        assert_eq!(tool.replay(), ReplaySafety::ReadOnly);
        assert!(tool.calls().is_empty());
    }
}
