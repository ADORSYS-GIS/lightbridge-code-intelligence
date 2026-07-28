//! Golden byte-parity for the review flow (R1e).
//!
//! Drives [`lci_review_agent::flows::run_review`] over the deterministic testkit (scripted model,
//! failing-runtime step journal, capturing sink) and a wiremock control plane, for all five frozen
//! [`GoldenScenario`]s, and asserts the produced [`LegacyTrace`] is byte-identical to the checked-in
//! fixture. After the legacy `run_native_agent` loop is deleted this is the surviving merge bar: it
//! locks the exact chat requests (messages + per-turn tool schemas), the dispatched tool calls and
//! outcomes, the policy events, the control-plane writes, the terminal outcome, AND the journaled step
//! sequence.

use std::sync::Arc;

use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_loop::{Conversation, LoopOutcome, RequestOptions, TranscriptEvent};
use lci_agent_testkit::{
    CapturingSink, FailingRuntime, GoldenHarness, GoldenScenario, LegacyTrace, ObservedCall,
    ObservedWrite, ScriptedModel,
};
use lci_agent_tools::{RuntimeCaps, ToolCx, TurnFilter};
use lci_review_agent::flows::{self, ReviewRunParams};
use lci_review_agent::prompt::{self, PrDiffRef, PromptConfig};
use lci_review_agent::tools::{self, ADD_REVIEW_COMMENT, tool_defs};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The control-plane surface the frozen scenarios touch: knowledge-tool discovery (unused on the flow
/// path — discovery is host glue), the telemetry sink, and the buffered review write endpoints. Every
/// write returns 204 so the loop never fails on a mediated action.
async fn mount_golden_control_plane(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/knowledge/tools",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/review/telemetry",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
    for endpoint in ["inline", "reply", "summary", "retract"] {
        Mock::given(method("POST"))
            .and(path(format!(
                "/api/v2/internal/tasks/{}/review/{endpoint}",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
    }
}

/// Drive `run_review` for one scenario and return the produced trace plus the runtime's step journal.
async fn drive(scenario: GoldenScenario) -> (LegacyTrace, Vec<String>) {
    let cp = MockServer::start().await;
    mount_golden_control_plane(&cp).await;
    let checkout = tempfile::tempdir().unwrap();
    tokio::fs::write(checkout.path().join("a.rs"), "one\ntwo\nthree\n")
        .await
        .unwrap();
    tokio::fs::write(checkout.path().join("big.txt"), "x".repeat(32 * 1024))
        .await
        .unwrap();

    let settings = scenario.settings();
    let diff_files = vec!["a.rs".to_string()];
    let diff = settings.diff_present.then_some(PrDiffRef {
        diff: "@@ -1,1 +1,2 @@\n one\n+two\n",
        files: &diff_files,
    });

    // Seed the conversation exactly as the legacy runner did: operator persona + tool-protocol system
    // message, the request + packed diff user message, and the initial offered-tool filter.
    let config = PromptConfig {
        system_prompt: "You are a reviewer.".to_string(),
        max_diff_chars: 60_000,
        context_window: settings.context_window,
    };
    let messages = prompt::build_messages(&config, "review", diff, None, None, None);
    let initial_names = tool_defs()
        .into_iter()
        .filter(|spec| settings.diff_present || spec.function.name != ADD_REVIEW_COMMENT)
        .map(|spec| spec.function.name)
        .collect::<Vec<_>>();
    let conversation = Conversation::new(
        messages,
        RequestOptions {
            model: "m".to_string(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: None,
            extra: serde_json::Map::new(),
        },
    )
    .with_filter(TurnFilter::only_names(initial_names));

    let client = ControlPlaneClient::new(cp.uri(), "tok");
    let embedder = EmbeddingsClient::new("http://unused", "key", "model");
    let registry = tools::tool_registry(
        Arc::new(client.clone()),
        Arc::new(embedder),
        [],
        RuntimeCaps::default(),
        None,
    )
    .unwrap();
    let workspace = flows::eager_workspace(checkout.path().to_path_buf());
    let cx = ToolCx {
        task_id: Uuid::nil(),
        workspace: &workspace,
    };
    // The legacy `review_config` numeric defaults the goldens were frozen against.
    let params = ReviewRunParams {
        max_turns: settings.max_turns,
        max_batch_size: 8,
        max_batches: 6,
        max_files_read: 30,
        max_searches: 15,
        max_coverage_bounces: settings.max_coverage_bounces,
        circuit_breaker_threshold: 3,
        context_window: settings.context_window,
        diff_present: settings.diff_present,
        diff_files: if settings.diff_present {
            diff_files.clone()
        } else {
            Vec::new()
        },
        sast_leads: Arc::new(std::sync::Mutex::new(Vec::new())),
    };

    let model = ScriptedModel::new(scenario.script().turns);
    let model_handle = model.clone();
    let sink = CapturingSink::default();
    let sink_handle = sink.clone();
    let runtime = FailingRuntime::default();
    let runtime_handle = runtime.clone();

    let outcome = flows::run_review(
        runtime,
        model,
        Box::new(sink),
        &cx,
        registry,
        conversation,
        params,
        &client,
    )
    .await
    .unwrap();

    let mut calls = Vec::new();
    let mut policy_events = Vec::new();
    for event in sink_handle.entries() {
        match event {
            TranscriptEvent::Tool {
                turn,
                call,
                outcome,
            } => calls.push(ObservedCall {
                turn,
                call,
                outcome,
            }),
            TranscriptEvent::Policy { turn, name, detail } => policy_events.push(json!({
                "type": "policy",
                "turn": turn,
                "name": name,
                "detail": detail,
            })),
            TranscriptEvent::Assistant { .. } => {}
        }
    }
    let control_plane_writes = cp
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|request| {
            let endpoint = request.url.path().to_string();
            (request.method.as_str() == "POST"
                && endpoint.contains("/review/")
                && !endpoint.ends_with("/telemetry"))
            .then(|| ObservedWrite {
                endpoint,
                body: serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    let outcome = match outcome {
        LoopOutcome::Finished => json!({"status":"finished"}),
        LoopOutcome::Exhausted => json!({"status":"exhausted"}),
        LoopOutcome::Aborted { reason } => json!({"status":"aborted","reason":reason}),
    };
    (
        LegacyTrace {
            scenario,
            chat_requests: model_handle.requests(),
            calls,
            policy_events,
            control_plane_writes,
            outcome,
        },
        runtime_handle.steps(),
    )
}

#[tokio::test]
async fn flows_run_review_matches_all_five_frozen_traces() {
    for scenario in GoldenScenario::ALL {
        let (trace, steps) = drive(scenario).await;

        // Provider round-trip fidelity: the opaque signature on the recorded finding + its tool_call_id
        // survive verbatim into the follow-up request the model sees.
        if scenario == GoldenScenario::PlainConvergeFinish {
            assert_eq!(
                trace.chat_requests[1]["messages"][2]["tool_calls"][0]["extra_content"]["provider"]
                    ["signature"],
                "opaque"
            );
            assert_eq!(
                trace.chat_requests[1]["messages"][3]["tool_call_id"],
                "plain-record"
            );
        }

        GoldenHarness::assert_fixture(scenario, &trace);

        let expected_steps: &[&str] = match scenario {
            GoldenScenario::PlainConvergeFinish => &[
                "llm_turn:0",
                "tool:0:plain-record",
                "llm_turn:1",
                "tool:1:plain-finish",
            ],
            GoldenScenario::WindDownEntry => &[
                "llm_turn:0",
                "tool:0:wind-progress",
                "llm_turn:1",
                "tool:1:wind-finish",
            ],
            GoldenScenario::ContextTrimTrigger => &[
                "llm_turn:0",
                "tools:0",
                "llm_turn:1",
                "tool:1:trim-progress",
                "llm_turn:2",
                "tool:2:trim-finish",
                "llm_turn:3",
                "tool:3:trim-finish",
            ],
            GoldenScenario::CoverageBounce => &[
                "llm_turn:0",
                "tool:0:coverage-finish-1",
                "llm_turn:1",
                "tools:1",
                "llm_turn:2",
                "tool:2:coverage-finish-2",
            ],
            GoldenScenario::ExhaustedBackstop => &["llm_turn:0", "llm_turn:1"],
        };
        assert_eq!(steps, expected_steps, "{scenario:?}: journal step sequence");
    }
}
