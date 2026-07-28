//! ADR-0073: opengrep runs ONLY when the review agent calls `run_sast` — never automatically. This
//! drives `agent_runner::review::run_native_agent` (the same host the real runner uses) over a mocked
//! LLM gateway and control plane, with a stub `opengrep` binary standing in for the real one (none is
//! installed in this dev/CI environment; `lci_agent_sast::process::run_opengrep` spawns whatever
//! `SastConfig.bin` names and only cares that it writes SARIF to `--sarif-output=PATH`). Proves both
//! directions: a model that never calls `run_sast` never spawns opengrep at all, and one that does gets
//! the finding buffered into the same review channel and surfaced back as the tool result.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_runner::bootstrap::config::{
    ResilienceConfig, ReviewConfig, ReviewTool, ReviewToolSelector,
};
use agent_runner::clone::PrDiff;
use agent_runner::review::{ReviewOutcome, run_native_agent};
use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_sast::SastConfig;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A stub `opengrep`: touches `marker` (proof it ran at all) and writes `sarif_body` to whatever
/// `--sarif-output=PATH` it's given. See `services/review-agent/src/tools/sast.rs`'s own tests for the
/// same trick at the tool-unit level; this is the end-to-end version.
fn write_stub_opengrep(dir: &Path, marker: &Path, sarif_body: &str) -> PathBuf {
    let bin = dir.join("opengrep-stub.sh");
    let script = format!(
        "#!/bin/sh\ntouch '{marker}'\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --sarif-output=*)\n      out=\"${{arg#--sarif-output=}}\"\n      cat > \"$out\" <<'SARIF'\n{sarif_body}\nSARIF\n      ;;\n  esac\ndone\nexit 0\n",
        marker = marker.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

const ONE_FINDING_SARIF: &str = r#"{
  "runs": [{
    "tool": {"driver": {"name": "opengrep", "rules": [
      {"id": "rust.security.exec", "defaultConfiguration": {"level": "error"}}
    ]}},
    "results": [
      {"ruleId": "rust.security.exec",
       "message": {"text": "Command injection via untrusted input."},
       "locations": [{"physicalLocation": {
         "artifactLocation": {"uri": "src/exec.rs"}, "region": {"startLine": 42}}}]}
    ]
  }]
}"#;

fn sast_config(bin: PathBuf) -> SastConfig {
    SastConfig {
        bin: bin.display().to_string(),
        rules: "unused-in-this-test".to_string(),
        min_severity: "warning".to_string(),
        max_findings: 50,
        timeout_secs: 5,
    }
}

/// Both tiers require an explicit `run_sast` allowlist entry (ADR-0073) — a deep tier with no allowlist
/// at all would NOT offer it (see `tool_surface::tests` for that gate's own coverage), so this test's
/// config must list it alongside the other tools the scripted model needs.
fn review_config(base_url: String) -> ReviewConfig {
    ReviewConfig {
        base_url,
        api_key: "key".to_string(),
        model: "test-model".to_string(),
        system_prompt: "You are a reviewer.".to_string(),
        max_diff_chars: 60_000,
        max_turns: 10,
        max_batch_size: 8,
        max_files_read: 30,
        max_searches: 15,
        max_batches: 6,
        max_coverage_bounces: 0,
        max_cycles: 8,
        context_window: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        extra: serde_json::Map::new(),
        stream: false,
        resilience: ResilienceConfig::default(),
        tools: Some(vec![
            ReviewToolSelector::Builtin(ReviewTool::RunSast),
            ReviewToolSelector::Builtin(ReviewTool::AddReviewComment),
            ReviewToolSelector::Builtin(ReviewTool::Finish),
            ReviewToolSelector::Builtin(ReviewTool::Abort),
        ]),
        opencode_overlay: None,
    }
}

async fn mount_control_plane(cp: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/knowledge/tools",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(cp)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/review/telemetry",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(cp)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/review/inline",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(cp)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/internal/tasks/{}/review/summary",
            Uuid::nil()
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(cp)
        .await;
}

/// Serves one scripted chat-completions response per call, in order; repeats the last one if the loop
/// somehow asks for more turns than scripted (never expected to happen in these two scenarios).
struct ScriptedChatTurns {
    turns: Vec<serde_json::Value>,
    calls: AtomicUsize,
}

impl Respond for ScriptedChatTurns {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = self
            .turns
            .get(i)
            .or_else(|| self.turns.last())
            .expect("at least one scripted turn")
            .clone();
        ResponseTemplate::new(200).set_body_json(body)
    }
}

fn tool_call_turn(id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            }
        }]
    })
}

async fn mount_llm(llm: &MockServer, turns: Vec<serde_json::Value>) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedChatTurns {
            turns,
            calls: AtomicUsize::new(0),
        })
        .mount(llm)
        .await;
}

/// The never-called case: the model finishes on turn 1 without ever calling `run_sast`. Opengrep must
/// never be spawned at all (the stub's marker file must not exist), and no SAST finding is buffered.
#[tokio::test]
async fn opengrep_never_runs_when_the_agent_never_calls_run_sast() {
    let stub_dir = tempfile::tempdir().unwrap();
    let marker = stub_dir.path().join("ran");
    let bin = write_stub_opengrep(stub_dir.path(), &marker, ONE_FINDING_SARIF);

    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join("src")).unwrap();
    std::fs::write(checkout.path().join("src/exec.rs"), "fn main() {}\n").unwrap();

    let cp = MockServer::start().await;
    mount_control_plane(&cp).await;
    let llm = MockServer::start().await;
    mount_llm(
        &llm,
        vec![tool_call_turn(
            "f1",
            "finish",
            r#"{"summary":"Diff-only pass, nothing to flag."}"#,
        )],
    )
    .await;

    let review = review_config(format!("{}/v1", llm.uri()));
    let client = ControlPlaneClient::new(cp.uri(), "tok");
    let embedder = EmbeddingsClient::new("http://unused", "key", "model");
    let diff = PrDiff {
        diff: "@@ -1,1 +1,1 @@\n-fn main() {}\n+fn main() {}\n".to_string(),
        files: vec!["src/exec.rs".to_string()],
    };
    let outcome = run_native_agent(
        &review,
        "review",
        Some(&diff),
        None,
        None,
        None,
        Some(&sast_config(bin)),
        &[],
        &client,
        &embedder,
        Uuid::nil(),
        checkout.path(),
        None,
    )
    .await
    .expect("run_native_agent");

    assert!(matches!(outcome, ReviewOutcome::Finished));
    assert!(
        !marker.exists(),
        "opengrep must never be spawned when the model never calls run_sast"
    );
    let inline_writes = cp
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/review/inline"))
        .count();
    assert_eq!(inline_writes, 0, "no SAST finding is buffered");
}

/// The called case: the model calls `run_sast` on turn 1, then `finish` on turn 2. Opengrep runs
/// (marker exists), the finding is buffered via the mediated `add_review_comment` channel, and the
/// digest surfaces back to the model as the tool's result (visible in the turn-2 request's messages).
#[tokio::test]
async fn opengrep_runs_and_surfaces_findings_when_the_agent_calls_run_sast() {
    let stub_dir = tempfile::tempdir().unwrap();
    let marker = stub_dir.path().join("ran");
    let bin = write_stub_opengrep(stub_dir.path(), &marker, ONE_FINDING_SARIF);

    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join("src")).unwrap();
    std::fs::write(checkout.path().join("src/exec.rs"), "fn main() {}\n").unwrap();

    let cp = MockServer::start().await;
    mount_control_plane(&cp).await;
    let llm = MockServer::start().await;
    mount_llm(
        &llm,
        vec![
            tool_call_turn("s1", "run_sast", "{}"),
            tool_call_turn(
                "f1",
                "finish",
                r#"{"summary":"Flagged the command-injection finding opengrep caught."}"#,
            ),
        ],
    )
    .await;

    let review = review_config(format!("{}/v1", llm.uri()));
    let client = ControlPlaneClient::new(cp.uri(), "tok");
    let embedder = EmbeddingsClient::new("http://unused", "key", "model");
    let diff = PrDiff {
        diff: "@@ -1,1 +1,1 @@\n-fn main() {}\n+fn main() {}\n".to_string(),
        files: vec!["src/exec.rs".to_string()],
    };
    let outcome = run_native_agent(
        &review,
        "review",
        Some(&diff),
        None,
        None,
        None,
        Some(&sast_config(bin)),
        &[],
        &client,
        &embedder,
        Uuid::nil(),
        checkout.path(),
        None,
    )
    .await
    .expect("run_native_agent");

    assert!(matches!(outcome, ReviewOutcome::Finished));
    assert!(
        marker.exists(),
        "opengrep actually ran once the model called run_sast"
    );

    let reqs = cp.received_requests().await.unwrap();
    let inline_bodies: Vec<serde_json::Value> = reqs
        .iter()
        .filter(|r| r.url.path().ends_with("/review/inline"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        inline_bodies.len(),
        1,
        "exactly the one SAST finding was buffered"
    );
    assert_eq!(inline_bodies[0]["file"], "src/exec.rs");
    assert_eq!(inline_bodies[0]["line"], 42);
    assert_eq!(inline_bodies[0]["category"], "security");

    // The digest — the same content lci_agent_sast::digest produces — reached the model as the
    // run_sast tool's result, visible in the turn-2 request's tool message.
    let llm_reqs = llm.received_requests().await.unwrap();
    assert_eq!(llm_reqs.len(), 2, "two chat-completions turns");
    let turn2: serde_json::Value = serde_json::from_slice(&llm_reqs[1].body).unwrap();
    let messages = turn2["messages"].as_array().unwrap();
    let tool_result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a tool result message for run_sast");
    let content = tool_result["content"].as_str().unwrap();
    assert!(
        content.contains("src/exec.rs:42"),
        "digest names the finding: {content}"
    );
    assert!(
        content.contains("rust.security.exec"),
        "digest names the rule: {content}"
    );
}
