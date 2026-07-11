//! Contract tests for the runner↔control-plane client (ADR-0017), against a mocked control plane
//! (wiremock) — no live service. They pin the wire shape the control plane's `internal.rs` must
//! keep: bearer auth, the task-context JSON, and the status callback.

use lci_agent_clients::ControlPlaneClient;
use uuid::Uuid;
use wiremock::matchers::{bearer_token, body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn get_context_sends_bearer_and_parses_the_response() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("GET"))
        .and(path(format!("/internal/tasks/{task_id}")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "task_id": task_id,
            "repository_id": 5,
            "owner": "octo",
            "name": "repo",
            "default_branch": "main",
            "clone_url": "https://github.com/octo/repo.git",
            "token": "test-install-tok",
            "target_type": "pull_request",
            "target_id": 7,
            "command": "review",
            "base_sha": "base123",
            "head_sha": "head456",
            "repo_indexed": true
        })))
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    let context = client.get_context(task_id).await.expect("context");

    assert_eq!(context.owner, "octo");
    assert_eq!(context.name, "repo");
    assert_eq!(context.command, "review");
    assert_eq!(context.head_sha.as_deref(), Some("head456"));
    assert!(context.repo_indexed, "repo_indexed parses from the context");
    assert_eq!(
        context.authenticated_clone_url(),
        "https://x-access-token:test-install-tok@github.com/octo/repo.git"
    );
}

#[tokio::test]
async fn task_status_sends_bearer_and_parses_status() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("GET"))
        .and(path(format!("/internal/tasks/{task_id}/status")))
        .and(bearer_token("runner-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": "cancelled" })),
        )
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    let status = client.task_status(task_id).await.expect("status");
    assert_eq!(status, "cancelled");
}

#[tokio::test]
async fn report_status_posts_the_status_with_bearer() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/status")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    client
        .report_status(task_id, "succeeded", Some("done"))
        .await
        .expect("status reported");
    // `expect(1)` is verified on server drop.
}

#[tokio::test]
async fn submit_chunks_posts_batch_with_bearer() {
    use lci_agent_clients::{ChunkBatch, ChunkPayload};

    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/chunks")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    client
        .submit_chunks(
            task_id,
            ChunkBatch {
                commit_sha: "abc123".to_string(),
                chunks: vec![ChunkPayload {
                    file_path: "src/main.rs".to_string(),
                    language: "rust".to_string(),
                    chunk_type: "function".to_string(),
                    symbol_name: Some("main".to_string()),
                    start_line: 0,
                    end_line: 5,
                    content: "fn main() {}".to_string(),
                    embedding: vec![0.0; 4],
                }],
            },
        )
        .await
        .expect("chunks submitted");
}

#[tokio::test]
async fn submit_review_telemetry_posts_tools_and_config_with_bearer() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    let tools = serde_json::json!([
        { "name": "read_file", "source": "builtin" },
        { "name": "mcp__context7__get_docs", "source": "mcp" },
    ]);
    // Pin the wire shape the control plane's `record_review_telemetry` deserializes: `{ tools, config_b64 }`.
    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/review/telemetry")))
        .and(bearer_token("runner-secret"))
        .and(body_json(serde_json::json!({
            "tools": tools,
            "config_b64": "cfg-b64",
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    client
        .submit_review_telemetry(task_id, &tools, "cfg-b64")
        .await
        .expect("telemetry submitted");
    // `expect(1)` + `body_json` are verified on server drop.
}

#[tokio::test]
async fn submit_graph_posts_nodes_and_edges_with_bearer() {
    use lci_agent_clients::{GraphBatch, GraphEdgePayload, GraphNodePayload};

    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/graph")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    client
        .submit_graph(
            task_id,
            GraphBatch {
                commit_sha: "abc123".to_string(),
                nodes: vec![GraphNodePayload {
                    node_id: "src_math_add".to_string(),
                    label: "add()".to_string(),
                    source_file: "src/math.rs".to_string(),
                    start_line: 2,
                }],
                edges: vec![GraphEdgePayload {
                    source: "src_math_calc_bump".to_string(),
                    target: "src_math_add".to_string(),
                    relation: "calls".to_string(),
                }],
            },
        )
        .await
        .expect("graph submitted");
}

#[tokio::test]
async fn search_posts_embedding_and_parses_hits() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/search")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "file_path": "src/auth.rs", "language": "rust", "chunk_type": "function",
                "symbol_name": "validate", "start_line": 10, "end_line": 40,
                "content": "fn validate() {}", "score": 0.93
            }
        ])))
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    let hits = client
        .search(task_id, &[0.1, 0.2, 0.3], 5)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_path, "src/auth.rs");
    assert!((hits[0].score - 0.93).abs() < 1e-9);
}

#[tokio::test]
async fn graph_get_callers_posts_op_and_parses_symbols() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/graph/query")))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "node_id": "src_math_calc_bump", "label": "bump()", "source_file": "src/math.rs", "start_line": 6 }
        ])))
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    let callers = client
        .graph_get_callers(task_id, "src_math_add", 10)
        .await
        .expect("callers");
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].node_id, "src_math_calc_bump");
}

#[tokio::test]
async fn review_and_knowledge_endpoints_preserve_their_wire_contracts() {
    use lci_agent_clients::TranscriptEntry;

    let server = MockServer::start().await;
    let task_id = Uuid::nil();
    let authenticated_post = |suffix: &str, body: serde_json::Value| {
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{task_id}{suffix}")))
            .and(bearer_token("runner-secret"))
            .and(body_json(body))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
    };

    authenticated_post(
        "/review/inline",
        serde_json::json!({
            "file": "src/lib.rs",
            "line": 12,
            "start_line": 10,
            "title": "Bounds check",
            "priority": "P1",
            "category": "correctness",
            "suggestion": "check the lower bound",
            "body": "The range is incomplete."
        }),
    )
    .mount(&server)
    .await;
    authenticated_post(
        "/review/inline/retract",
        serde_json::json!({ "file": "src/lib.rs", "line": 12 }),
    )
    .mount(&server)
    .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/internal/tasks/{task_id}/review/inline/clear"
        )))
        .and(bearer_token("runner-secret"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    authenticated_post(
        "/review/comment",
        serde_json::json!({ "body": "A plain reply" }),
    )
    .mount(&server)
    .await;
    authenticated_post(
        "/review/summary",
        serde_json::json!({ "body": "One issue found" }),
    )
    .mount(&server)
    .await;
    authenticated_post(
        "/review/finalize",
        serde_json::json!({ "outcome": "finished" }),
    )
    .mount(&server)
    .await;
    authenticated_post(
        "/transcript",
        serde_json::json!({
            "entries": [{
                "role": "assistant",
                "content": "checking",
                "tool_calls": [{"id": "call-1"}],
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "reasoning_tokens": 2,
                "model": "review-model"
            }]
        }),
    )
    .mount(&server)
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/graph/query")))
        .and(bearer_token("runner-secret"))
        .and(body_json(serde_json::json!({
            "op": "find_symbol",
            "term": "parse",
            "limit": 3
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "node_id": "src_parser_parse",
                "label": "parse()",
                "source_file": "src/parser.rs",
                "start_line": 8
            }])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/tasks/{task_id}/knowledge/tools")))
        .and(bearer_token("runner-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "name": "mcp__docs__lookup",
                "description": "look up docs",
                "input_schema": {"type": "object"}
            }])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/internal/tasks/{task_id}/knowledge/call")))
        .and(bearer_token("runner-secret"))
        .and(body_json(serde_json::json!({
            "tool": "mcp__docs__lookup",
            "arguments": {"query": "retry policy"}
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "documentation" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "runner-secret");
    client
        .add_review_comment(
            task_id,
            "src/lib.rs",
            12,
            Some(10),
            Some("Bounds check"),
            Some("P1"),
            Some("correctness"),
            Some("check the lower bound"),
            "The range is incomplete.",
        )
        .await
        .expect("inline finding");
    client
        .retract_finding(task_id, "src/lib.rs", 12)
        .await
        .expect("retract finding");
    client
        .clear_findings(task_id)
        .await
        .expect("clear findings");
    client
        .add_review_reply(task_id, "A plain reply")
        .await
        .expect("reply");
    client
        .set_review_summary(task_id, "One issue found")
        .await
        .expect("summary");
    client
        .finalize_review(task_id, "finished")
        .await
        .expect("finalize");
    client
        .submit_transcript(
            task_id,
            &[TranscriptEntry {
                role: "assistant".to_string(),
                content: Some("checking".to_string()),
                tool_calls: Some(serde_json::json!([{"id": "call-1"}])),
                tool_name: None,
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
                reasoning_tokens: Some(2),
                model: Some("review-model".to_string()),
            }],
        )
        .await
        .expect("transcript");

    let symbols = client
        .graph_find_symbol(task_id, "parse", 3)
        .await
        .expect("find symbol");
    assert_eq!(symbols[0].source_file, "src/parser.rs");
    let tools = client
        .list_knowledge_tools(task_id)
        .await
        .expect("list tools");
    assert_eq!(tools[0].name, "mcp__docs__lookup");
    let result = client
        .call_knowledge_tool(
            task_id,
            "mcp__docs__lookup",
            serde_json::json!({"query": "retry policy"}),
        )
        .await
        .expect("call tool");
    assert_eq!(result, "documentation");
}

#[tokio::test]
async fn get_context_errors_on_non_2xx() {
    let server = MockServer::start().await;
    let task_id = Uuid::nil();

    Mock::given(method("GET"))
        .and(path(format!("/internal/tasks/{task_id}")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = ControlPlaneClient::new(server.uri(), "wrong");
    assert!(client.get_context(task_id).await.is_err());
}
