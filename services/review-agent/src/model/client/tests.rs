use super::*;
use lci_agent_types::{FunctionCall, ToolCall};
use wiremock::matchers::{bearer_token, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn search_tool() -> ToolDef {
    ToolDef::function(
        "vector_semantic_search",
        "Search the repo by meaning.",
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
        }),
    )
}

#[tokio::test]
async fn complete_sends_model_messages_tools_and_parses_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(bearer_token("key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "vector_semantic_search",
                            "arguments": "{\"query\":\"session expiry\"}"
                        }
                    }]
                }
            }]
        })))
        .mount(&server)
        .await;

    // base URL includes /v1, like LLM_BASE_URL.
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "qwen-coder");
    let out = client
        .complete(
            &[
                ChatMessage::system("review the diff"),
                ChatMessage::user("@lightbridge review"),
            ],
            &[search_tool()],
            ChatParams {
                temperature: Some(0.2),
                max_tokens: Some(4096),
                ..ChatParams::default()
            },
        )
        .await
        .expect("complete");

    assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    assert!(out.message.content.is_none());
    assert_eq!(out.message.tool_calls.len(), 1);
    let call = &out.message.tool_calls[0];
    assert_eq!(call.id, "call_1");
    assert_eq!(call.function.name, "vector_semantic_search");
    assert_eq!(call.function.arguments, "{\"query\":\"session expiry\"}");

    // The request we sent carries the model, both messages, the advertised tool, tool_choice, and
    // the generation params; unset params are omitted.
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "qwen-coder");
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["tools"][0]["function"]["name"],
        "vector_semantic_search"
    );
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["temperature"], serde_json::json!(0.2));
    assert_eq!(body["max_tokens"], serde_json::json!(4096));
    assert!(body.get("top_p").is_none(), "unset params are omitted");
}

// A passthrough (`review.extra`) — e.g. a reasoning budget to cap an over-reasoning model like
// glm-5 — is flattened verbatim into the request body, so an operator can tune it without a code
// change.
#[tokio::test]
async fn with_extra_flattens_passthrough_params_into_the_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;

    let mut extra = serde_json::Map::new();
    extra.insert(
        "thinking".to_string(),
        serde_json::json!({ "type": "disabled" }),
    );
    extra.insert("reasoning_effort".to_string(), serde_json::json!("low"));
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_extra(extra);

    client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("complete");

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    // Passthrough fields land at the TOP LEVEL of the body (flattened), beside model/messages.
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["model"], "glm-5");
}

// The default (no passthrough) adds nothing — an empty map flattens to zero fields.
#[tokio::test]
async fn empty_extra_adds_no_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("complete");
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("thinking").is_none());
    assert!(body.get("reasoning_effort").is_none());
}

// A reserved structural key placed in `extra` (operator typo / footgun) is stripped, so it can NOT
// override `model`/`temperature`/… via the flatten merge. A non-reserved key still passes through.
#[tokio::test]
async fn with_extra_strips_reserved_keys() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;

    let mut extra = serde_json::Map::new();
    extra.insert("model".to_string(), serde_json::json!("evil-override"));
    extra.insert("temperature".to_string(), serde_json::json!(1.9));
    extra.insert("reasoning_effort".to_string(), serde_json::json!("low"));
    let client =
        ChatClient::new(&format!("{}/v1", server.uri()), "key", "real-model").with_extra(extra);

    client
        .complete(
            &[ChatMessage::user("hi")],
            &[],
            ChatParams {
                temperature: Some(0.2),
                ..ChatParams::default()
            },
        )
        .await
        .expect("complete");

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "real-model", "extra cannot override model");
    assert_eq!(
        body["temperature"],
        serde_json::json!(0.2),
        "extra cannot override the structural temperature"
    );
    assert_eq!(
        body["reasoning_effort"], "low",
        "non-reserved key passes through"
    );
}

// Streaming spike: a tool call whose `name`/`arguments` are split across SSE chunks is reassembled
// by `index` into the same `Completion` the non-stream path would produce, with the final usage.
#[tokio::test]
async fn stream_reassembles_tool_call_deltas_and_usage() {
    let server = MockServer::start().await;
    let events = [
        serde_json::json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"vector_semantic_search","arguments":"{\"query\":"}}]}}]}),
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"auth\"}"}}]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
    ];
    let mut sse = String::new();
    for e in &events {
        sse.push_str(&format!("data: {e}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_stream(true);
    let out = client
        .complete(
            &[ChatMessage::user("hi")],
            &[search_tool()],
            ChatParams::default(),
        )
        .await
        .expect("stream completes");

    // The request asked for a stream + usage.
    let body: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);

    // The fragmented tool call is reassembled verbatim.
    assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(out.message.tool_calls.len(), 1);
    let call = &out.message.tool_calls[0];
    assert_eq!(call.id, "call_1");
    assert_eq!(call.function.name, "vector_semantic_search");
    assert_eq!(call.function.arguments, r#"{"query":"auth"}"#);
    // Usage from the final chunk is captured.
    assert_eq!(out.usage.and_then(|u| u.prompt_tokens), Some(10));
}

// Non-stream: a GLM/DeepSeek `reasoning_content` on the message is surfaced into
// `Completion::reasoning`, and a top-level `usage.reasoning_tokens` (the shape camer.digital's
// gateway returns) is read even though it isn't nested under `completion_tokens_details`.
#[tokio::test]
async fn non_stream_captures_reasoning_content_and_top_level_reasoning_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Final answer.",
                    "reasoning_content": "Let me think step by step: 1, 2, 3."
                }
            }],
            "usage": { "prompt_tokens": 19, "completion_tokens": 219, "reasoning_tokens": 0 }
        })))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5");
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("completes");

    assert_eq!(
        out.reasoning.as_deref(),
        Some("Let me think step by step: 1, 2, 3.")
    );
    assert_eq!(out.message.content.as_deref(), Some("Final answer."));
    // Top-level reasoning_tokens is found by the accessor (here it's the gateway's `0`, not absent).
    assert_eq!(out.usage.and_then(|u| u.reasoning_tokens()), Some(0));
}

// Streaming: `reasoning_content` deltas are reassembled into `Completion::reasoning`, separate from
// the visible `content`, and not echoed into the assistant message.
#[tokio::test]
async fn stream_reassembles_reasoning_content_deltas() {
    let server = MockServer::start().await;
    let events = [
        serde_json::json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"think "}}]}),
        serde_json::json!({"choices":[{"delta":{"reasoning_content":"harder"}}]}),
        serde_json::json!({"choices":[{"delta":{"content":"the answer"}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":9}}),
    ];
    let mut sse = String::new();
    for e in &events {
        sse.push_str(&format!("data: {e}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_stream(true);
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("stream completes");

    assert_eq!(out.reasoning.as_deref(), Some("think harder"));
    assert_eq!(out.message.content.as_deref(), Some("the answer"));
}

// CRLF SSE (standards-compliant gateways) must parse identically: the byte buffer strips `\r`, so
// `\r\n\r\n` normalises to the `\n\n` event boundary instead of never matching. (#206 review.)
#[tokio::test]
async fn stream_handles_crlf_line_endings() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\r\n\r\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\r\n\r\n\
               data: [DONE]\r\n\r\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("crlf stream completes");
    assert_eq!(out.message.content.as_deref(), Some("hello world"));
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
}

// The streaming path must carry the gateway's rate-limit headers onto the Completion too — the
// headers are on the response before the SSE body is consumed. Regression guard alongside the
// non-streaming `complete_parses_rate_limit_headers`: the #206 streaming refactor dropped this
// wiring on both paths, so each path needs its own guard (lightbridge review on #209).
#[tokio::test]
async fn stream_parses_rate_limit_headers() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("x-ratelimit-limit", "1000")
                .insert_header("x-ratelimit-remaining", "40")
                .insert_header("x-ratelimit-reset", "12")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("stream completes");
    assert_eq!(out.message.content.as_deref(), Some("ok"));
    assert_eq!(out.rate_limit.limit, Some(1000));
    assert_eq!(out.rate_limit.remaining, Some(40));
    assert_eq!(out.rate_limit.reset, Some(Duration::from_secs(12)));
    assert!(
        out.rate_limit.is_low(0.1),
        "40/1000 is below the 10% threshold"
    );
}

// A stream that closes before a terminal signal ([DONE] / finish_reason) is a truncated response —
// surfaced as a transient error so the turn retries, not a "successful" partial completion. (#206.)
#[tokio::test]
async fn stream_truncated_without_finish_is_transient_error() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial...\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse))
        .mount(&server)
        .await;
    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
    let err = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect_err("a truncated stream is an error");
    assert!(
        format!("{err:#}").contains("closed before completion"),
        "got: {err:#}"
    );
}

#[tokio::test]
async fn complete_parses_a_plain_text_reply_and_omits_tools_when_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": "looks good" }
            }]
        })))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("complete");
    assert_eq!(out.message.content.as_deref(), Some("looks good"));
    assert!(out.message.tool_calls.is_empty());

    // With no tools, neither `tools` nor `tool_choice` is sent.
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

// The gateway's draft-03 rate-limit headers are parsed onto the completion (advisory telemetry).
// Regression guard: the #206 streaming refactor dropped this wiring; keep a test so it can't
// silently vanish again.
#[tokio::test]
async fn complete_parses_rate_limit_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-limit", "1000")
                .insert_header("x-ratelimit-remaining", "40")
                .insert_header("x-ratelimit-reset", "12")
                .set_body_json(serde_json::json!({
                    "choices": [{ "finish_reason": "stop",
                        "message": { "role": "assistant", "content": "ok" } }]
                })),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let out = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("complete");
    assert_eq!(out.rate_limit.limit, Some(1000));
    assert_eq!(out.rate_limit.remaining, Some(40));
    assert_eq!(out.rate_limit.reset, Some(Duration::from_secs(12)));
    assert!(
        out.rate_limit.is_low(0.1),
        "40/1000 is below the 10% threshold"
    );
}

#[tokio::test]
async fn complete_surfaces_an_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let err = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect_err("500 is an error");
    assert!(format!("{err:#}").contains("returned 500"), "got: {err:#}");
}

// ── ADR-0039 resilience tests ───────────────────────────────────────────────────────────────

fn ok_reply() -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "finish_reason": "stop",
            "message": { "role": "assistant", "content": "ok" } }]
    })
}

fn fast_policy() -> RetryPolicy {
    // Tiny backoff so tests don't actually sleep meaningfully.
    RetryPolicy {
        max_retries: 2,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
    }
}

// A 5xx is transient → retried; once the gateway recovers, the turn succeeds.
#[tokio::test]
async fn retries_on_5xx_then_succeeds() {
    let server = MockServer::start().await;
    // First call: 503. Then: 200.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let out = client
        .complete_with_retry(
            &[ChatMessage::user("hi")],
            &[],
            ChatParams::default(),
            fast_policy(),
        )
        .await
        .expect("recovers after a transient 503");
    assert_eq!(out.message.content.as_deref(), Some("ok"));
}

// A 400 is deterministic → NOT retried (exactly one request hits the server) and the body surfaces.
#[tokio::test]
async fn does_not_retry_on_400_and_surfaces_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": { "message": "unknown model 'm'" }
        })))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let err = client
        .complete_with_retry(
            &[ChatMessage::user("hi")],
            &[],
            ChatParams::default(),
            fast_policy(),
        )
        .await
        .expect_err("400 is deterministic");
    assert!(!err.transient, "400 is not transient");
    let msg = format!("{err}");
    assert!(msg.contains("returned 400"), "status surfaced: {msg}");
    assert!(msg.contains("unknown model"), "body surfaced: {msg}");

    // Exactly one request — no retry.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "400 must not be retried");
}

// The HTTP error body is folded into the error returned by the plain `complete`.
#[tokio::test]
async fn error_body_is_surfaced_in_complete() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(502).set_body_string("upstream connect error or disconnect"),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let err = client
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect_err("502");
    let msg = format!("{err:#}");
    assert!(msg.contains("502"), "status: {msg}");
    assert!(msg.contains("upstream connect error"), "body: {msg}");
}

// A per-request timeout aborts a slow response and classifies it transient.
#[tokio::test]
async fn times_out_a_slow_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_reply())
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;

    // 50ms timeout < 300ms delay → times out.
    let client = ChatClient::with_timeout(
        &format!("{}/v1", server.uri()),
        "key",
        "m",
        Duration::from_millis(50),
    );
    let err = client
        .complete_inner(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect_err("should time out");
    assert!(err.transient, "a timeout is transient");
    assert!(format!("{err}").contains("request failed"), "got: {err}");
}

// `for_model` retargets the model id while sharing the gateway/key/timeout.
#[tokio::test]
async fn for_model_retargets_the_model_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;

    let primary = ChatClient::new(&format!("{}/v1", server.uri()), "key", "primary-model");
    let secondary = primary.for_model("fallback-model");
    assert_eq!(secondary.model(), "fallback-model");
    secondary
        .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
        .await
        .expect("fallback client works");

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "fallback-model");
}

#[test]
fn tool_messages_serialize_with_id_and_assistant_tool_calls_round_trip() {
    // A tool-result message carries role + content + tool_call_id, no tool_calls.
    let tool_msg = ChatMessage::tool("call_1", "results...");
    let v = serde_json::to_value(&tool_msg).unwrap();
    assert_eq!(v["role"], "tool");
    assert_eq!(v["tool_call_id"], "call_1");
    assert!(v.get("tool_calls").is_none(), "empty tool_calls omitted");

    // An assistant turn with tool_calls round-trips (we echo it back into the next request).
    let assistant = ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "submit_findings".to_string(),
                arguments: "{}".to_string(),
            },
            extra_content: None,
        }],
        tool_call_id: None,
    };
    let round: ChatMessage =
        serde_json::from_value(serde_json::to_value(&assistant).unwrap()).unwrap();
    assert_eq!(round, assistant);
}

// Gemini 3 attaches an opaque `thought_signature` to each tool call under
// `extra_content.google.thought_signature`, then **400s the *next* request** if it isn't echoed
// back verbatim ("Function call is missing a thought_signature in functionCall parts" — RunID
// 0a210c73). The client must parse the blob off the response tool call and re-serialize it
// unchanged when that assistant turn is sent again. Verifies both halves.
#[tokio::test]
async fn tool_call_extra_content_is_captured_and_echoed_back() {
    let server = MockServer::start().await;
    let signature = serde_json::json!({ "google": { "thought_signature": "abc123==" } });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{}" },
                        "extra_content": { "google": { "thought_signature": "abc123==" } }
                    }]
                }
            }]
        })))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro");
    let out = client
        .complete(
            &[ChatMessage::user("hi")],
            &[search_tool()],
            ChatParams::default(),
        )
        .await
        .expect("complete");

    // Parsed off the response verbatim.
    assert_eq!(
        out.message.tool_calls[0].extra_content.as_ref(),
        Some(&signature)
    );

    // And re-serialized verbatim when the assistant turn is echoed back into the next request —
    // the exact round-trip Gemini requires (missing → 400).
    let echoed = serde_json::to_value(&out.message).unwrap();
    assert_eq!(echoed["tool_calls"][0]["extra_content"], signature);
}

// A tool call with no provider blob (any non-Gemini provider, or the non-first call of a Gemini
// parallel batch, which Gemini leaves unsigned) must NOT emit `extra_content: null` — the field is
// simply absent from the wire, so it can't inject a spurious `null` a strict gateway might reject.
#[test]
fn tool_call_without_extra_content_omits_the_field() {
    let call = ToolCall {
        id: "c".to_string(),
        kind: "function".to_string(),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        },
        extra_content: None,
    };
    let v = serde_json::to_value(&call).unwrap();
    assert!(
        v.get("extra_content").is_none(),
        "None must be omitted, not serialized as null"
    );
}

// Streaming: Gemini streams the `thought_signature` envelope on the tool-call delta. It must be
// captured into the reassembled `ToolCall::extra_content` (alongside the fragmented arguments) so
// it survives the echo-back, exactly as the non-stream path does.
#[tokio::test]
async fn stream_captures_tool_call_extra_content() {
    let server = MockServer::start().await;
    let events = [
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{}"},"extra_content":{"google":{"thought_signature":"sig=="}}}]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ];
    let mut sse = String::new();
    for e in &events {
        sse.push_str(&format!("data: {e}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client =
        ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro").with_stream(true);
    let out = client
        .complete(
            &[ChatMessage::user("hi")],
            &[search_tool()],
            ChatParams::default(),
        )
        .await
        .expect("stream completes");

    assert_eq!(
        out.message.tool_calls[0].extra_content,
        Some(serde_json::json!({ "google": { "thought_signature": "sig==" } }))
    );
}

// The signature usually arrives on the *first* tool-call delta (with `id`/`name`), while later
// deltas carry only `arguments` fragments and omit `extra_content` entirely. A later delta must not
// clobber the captured signature. This also covers a provider that emits an explicit
// `"extra_content": null` on a follow-up chunk: `Option<Value>` deserializes JSON `null` to `None`
// (serde_json's `deserialize_option` maps `null` → none), so the `if let Some(ec)` guard skips it —
// no defensive `is_null()` check needed (gemini-code-assist review on #262). Regression guard.
#[tokio::test]
async fn stream_extra_content_survives_later_deltas() {
    let server = MockServer::start().await;
    let events = [
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"pa"},"extra_content":{"google":{"thought_signature":"sig=="}}}]}}]}),
        // A follow-up delta: more argument bytes, and an explicit null extra_content.
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"},"extra_content":null}]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ];
    let mut sse = String::new();
    for e in &events {
        sse.push_str(&format!("data: {e}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client =
        ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro").with_stream(true);
    let out = client
        .complete(
            &[ChatMessage::user("hi")],
            &[search_tool()],
            ChatParams::default(),
        )
        .await
        .expect("stream completes");

    // Arguments reassembled across the two deltas, and the signature from delta 1 is intact.
    let call = &out.message.tool_calls[0];
    assert_eq!(call.function.arguments, r#"{"path":"x"}"#);
    assert_eq!(
        call.extra_content,
        Some(serde_json::json!({ "google": { "thought_signature": "sig==" } })),
        "a later delta (incl. explicit null) must not clobber the captured signature"
    );
}

// ── ModelClient (engine) path ─────────────────────────────────────────────────────────────────

/// Build the engine request the loop presents to a [`ModelClient`], borrowing `messages`/`tools`.
fn engine_request<'a>(
    messages: &'a [ChatMessage],
    tools: &'a [ToolDef],
    extra: &'a serde_json::Map<String, serde_json::Value>,
) -> ChatRequest<'a> {
    ChatRequest {
        model: "m",
        messages,
        tools,
        tool_choice: (!tools.is_empty()).then_some("auto"),
        temperature: None,
        top_p: None,
        max_tokens: None,
        stream: None,
        stream_options: None,
        extra,
    }
}

// The engine path returns the model-visible `AssistantTurn` and records the turn's token/reasoning
// telemetry on the side-channel (ADR-0034), keyed positionally to the turn.
#[tokio::test]
async fn model_client_complete_returns_turn_and_records_telemetry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "thinking...",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }]
                }
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7,
                "completion_tokens_details": { "reasoning_tokens": 3 } }
        })))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let telemetry = client.telemetry_handle();
    let messages = [ChatMessage::user("hi")];
    let tools = [search_tool()];
    let extra = serde_json::Map::new();
    let turn = ModelClient::complete(&client, engine_request(&messages, &tools, &extra))
        .await
        .expect("engine turn");

    assert!(turn.content.is_none());
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].function.name, "read_file");

    let recorded = telemetry.lock().unwrap();
    assert_eq!(recorded.len(), 1, "one turn ⇒ one telemetry row");
    assert_eq!(recorded[0].model, "m");
    assert_eq!(recorded[0].prompt_tokens, Some(11));
    assert_eq!(recorded[0].completion_tokens, Some(7));
    assert_eq!(recorded[0].reasoning_tokens, Some(3));
    assert_eq!(recorded[0].reasoning.as_deref(), Some("thinking..."));
}

// A deterministic 4xx maps to a TERMINAL StepError with the response body folded into the reason —
// the exact text the loop's context-overflow detection matches on. No telemetry is recorded.
#[tokio::test]
async fn model_client_maps_deterministic_failure_to_terminal_error_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("This model's maximum context length is 8192 tokens"),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
    let telemetry = client.telemetry_handle();
    let messages = [ChatMessage::user("hi")];
    let extra = serde_json::Map::new();
    let err = ModelClient::complete(&client, engine_request(&messages, &[], &extra))
        .await
        .expect_err("400 is terminal");
    assert!(!err.is_transient(), "a 400 is deterministic");
    assert!(
        err.to_string().to_lowercase().contains("context length"),
        "body folded into the terminal reason for overflow detection: {err}"
    );
    assert!(
        telemetry.lock().unwrap().is_empty(),
        "a failed turn records no telemetry"
    );
}

// A transient 5xx is retried under the client's own retry policy (as the legacy `complete_with_retry`
// did) and succeeds once the gateway recovers — the loop never sees the transient failure.
#[tokio::test]
async fn model_client_retries_transient_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
        .mount(&server)
        .await;

    let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m")
        .with_retry_policy(fast_policy());
    let messages = [ChatMessage::user("hi")];
    let extra = serde_json::Map::new();
    let turn = ModelClient::complete(&client, engine_request(&messages, &[], &extra))
        .await
        .expect("recovers after a transient 503");
    assert_eq!(turn.content.as_deref(), Some("ok"));
}
