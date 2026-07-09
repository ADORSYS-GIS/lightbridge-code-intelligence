//! Conformance/round-trip tests for the A2A SDK evaluation probe (RFC-0006 / #298).
//!
//! Everything here runs in-process with no network: the card router is driven via
//! `tower::ServiceExt::oneshot`, and the `GetTask` round-trip goes straight through
//! `DefaultRequestHandler` reading from our own [`ProbeTaskStore`]. The assertions pin the
//! v1.0.1 wire shapes the RFC calls out (well-known path, PascalCase methods, `TASK_STATE_*`
//! enums, camelCase card with `supportedInterfaces[]`, OIDC scheme) so a future SDK bump that
//! regresses them fails loudly.

use std::collections::HashMap;

use a2a::jsonrpc::methods;
use a2a::{GetTaskRequest, Task, TaskState, TaskStatus};
use a2a_probe::{build_agent_card, build_card_router, ProbeTaskStore};
use a2a_server::executor::ExecutorContext;
use a2a_server::handler::{DefaultRequestHandler, RequestHandler};
use a2a_server::{AgentExecutor, ServiceParams, TaskStore};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use http_body_util::BodyExt;
use tower::ServiceExt;

const BASE_URL: &str = "https://a2a.lightbridge.example/a2a";

/// Minimal executor — Phase-1 evaluation never runs it; it only satisfies the handler
/// constructor so we can exercise the read path (`GetTask`) against our store.
struct NoopExecutor;

impl AgentExecutor for NoopExecutor {
    fn execute(
        &self,
        _ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<a2a::StreamResponse, a2a::A2AError>> {
        stream::empty().boxed()
    }

    fn cancel(
        &self,
        _ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<a2a::StreamResponse, a2a::A2AError>> {
        stream::empty().boxed()
    }
}

fn submitted_task(id: &str, ctx: &str) -> Task {
    Task {
        id: id.to_string(),
        context_id: ctx.to_string(),
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

#[test]
fn agent_card_serializes_to_v1_0_1_wire_shapes() {
    let card = serde_json::to_value(build_agent_card(BASE_URL)).expect("card serializes");

    // camelCase field names (spec uses camelCase JSON, not snake_case).
    assert!(card.get("supportedInterfaces").is_some());
    assert!(card.get("defaultInputModes").is_some());
    assert!(card.get("securitySchemes").is_some());

    // Ordered supportedInterfaces[]: JSON-RPC preferred (first), REST/HTTP+JSON second.
    let interfaces = card["supportedInterfaces"].as_array().expect("array");
    assert_eq!(interfaces.len(), 2, "expected two transports");
    assert_eq!(interfaces[0]["protocolBinding"], "JSONRPC");
    assert_eq!(interfaces[1]["protocolBinding"], "HTTP+JSON");
    // Every interface advertises a protocolVersion (the SDK defaults it to the impl version).
    for iface in interfaces {
        assert!(
            iface
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .is_some(),
            "interface missing protocolVersion: {iface}"
        );
    }

    // OIDC security scheme is emitted under the spec's field-presence key.
    let oidc = &card["securitySchemes"]["keycloak-oidc"];
    assert!(
        oidc.get("openIdConnectSecurityScheme").is_some(),
        "expected openIdConnectSecurityScheme variant, got {oidc}"
    );
    assert!(oidc["openIdConnectSecurityScheme"]
        .get("openIdConnectUrl")
        .is_some());

    // Both skills present, addressable by their ids.
    let skill_ids: Vec<&str> = card["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(skill_ids, vec!["review", "ask"]);
}

#[tokio::test]
async fn card_router_serves_the_well_known_path() {
    let router = build_card_router(BASE_URL);

    // Wrong path 404s; the exact well-known path 200s and returns a parseable card.
    let missing = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agent.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let card: a2a::AgentCard = serde_json::from_slice(&bytes).expect("body is a valid AgentCard");
    assert_eq!(card.skills.len(), 2);
}

#[tokio::test]
async fn get_task_round_trip_through_our_store() {
    // The R2 proof: our own TaskStore impl, injected into the SDK's DefaultRequestHandler,
    // serves a real GetTask read. The handler is NOT hard-wired to InMemoryTaskStore.
    let store = ProbeTaskStore::with_task(submitted_task("task-1", "ctx-1"));
    let handler = DefaultRequestHandler::new(NoopExecutor, store);

    let params: ServiceParams = HashMap::new();
    let task = handler
        .get_task(
            &params,
            GetTaskRequest {
                id: "task-1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect("task found via our store");

    assert_eq!(task.id, "task-1");
    assert_eq!(task.context_id, "ctx-1");
    // State serializes to the SCREAMING_SNAKE v1.0 wire value.
    let state = serde_json::to_value(task.status.state).unwrap();
    assert_eq!(state, "TASK_STATE_SUBMITTED");

    // Unknown id is a clean not-found, not a panic.
    let missing = handler
        .get_task(
            &params,
            GetTaskRequest {
                id: "does-not-exist".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn probe_store_list_filters_by_context_and_status() {
    // Exercises the remaining TaskStore methods (create/update/list) so the reusable store is
    // covered end-to-end, not just the get() path the handler happens to hit.
    let store = ProbeTaskStore::new();
    store.create(submitted_task("a", "ctx-1")).await.unwrap();
    store.create(submitted_task("b", "ctx-2")).await.unwrap();

    // Duplicate create is rejected.
    assert!(store.create(submitted_task("a", "ctx-1")).await.is_err());

    // Move "a" to WORKING via update.
    let mut working = submitted_task("a", "ctx-1");
    working.status.state = TaskState::Working;
    let version = store.update(working).await.unwrap();
    assert_eq!(version, 2, "update bumps the version");
    assert!(store.update(submitted_task("z", "ctx-9")).await.is_err());

    // Filter by context.
    let by_ctx = store
        .list(&a2a::ListTasksRequest {
            context_id: Some("ctx-1".to_string()),
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(by_ctx.tasks.len(), 1);
    assert_eq!(by_ctx.tasks[0].id, "a");

    // Filter by status.
    let by_status = store
        .list(&a2a::ListTasksRequest {
            context_id: None,
            status: Some(TaskState::Working),
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(by_status.tasks.len(), 1);
    assert_eq!(by_status.tasks[0].id, "a");
    assert_eq!(by_status.total_size, 1);
}

#[test]
fn json_rpc_method_names_are_pascal_case_v1() {
    // Guards against the stale 0.2/0.3 dotted names most tutorials still show.
    assert_eq!(methods::SEND_MESSAGE, "SendMessage");
    assert_eq!(methods::GET_TASK, "GetTask");
    assert_eq!(methods::CANCEL_TASK, "CancelTask");
    assert!(methods::is_valid("SendMessage"));
    assert!(!methods::is_valid("message.send"));
    assert!(!methods::is_valid("tasks.get"));
}
