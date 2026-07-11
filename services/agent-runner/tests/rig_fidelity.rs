//! Rig transport-fidelity harness (ADR-0075 Phase-1 enabler, ticket #300, epic #295).
//!
//! Our review agent talks **OpenAI-compatible Chat Completions** to the internal eaig gateway, not to
//! native provider SDKs. The hand-rolled transport that carries that traffic is
//! [`crate::review::native::chat`](../src/review/native/chat.rs). It depends on three provider-quirk
//! fields that a naive typed deserializer silently drops:
//!
//!   1. **`thought_signature`** — Gemini 3 hangs an opaque round-trip blob off each tool call as
//!      `extra_content = {"google":{"thought_signature":"…"}}`. It MUST be echoed back verbatim on the
//!      next turn or the model 400s ("Function call is missing a thought_signature …"). This is the
//!      #262 failure family. Our `ToolCall` keeps it as an opaque `extra_content: Option<Value>`.
//!   2. **`reasoning_content`** (alias `reasoning`) — the model's chain-of-thought, read off the
//!      response message for the transcript.
//!   3. **`usage` reasoning-token detail** — `completion_tokens_details.reasoning_tokens` plus a
//!      top-level `reasoning_tokens` fallback some gateways (camer.digital's) emit.
//!
//! ADR-0075 proposes moving new agent surfaces onto `rig-core`. Rig fixed `thought_signature`
//! round-trip in its **native `gemini`** provider — but we would use the **OpenAI-compatible** provider
//! pointed at a custom `base_url`, where typed deserialization is exactly where nonstandard fields get
//! dropped. This harness answers, with fixtures that mirror what eaig emits: does rig's OpenAI provider
//! preserve these fields?
//!
//! ## Verdict (rig-core 0.39.0, `providers::openai::completion`, Chat Completions API)
//!
//! | field                         | preserved? | mechanism                                                        |
//! |-------------------------------|------------|-----------------------------------------------------------------|
//! | `thought_signature`/`extra_content` | **NO**     | rig's `ToolCall` has only `{id,type,function}` — no field for it |
//! | `reasoning_content`           | **YES**    | `Message::Assistant.reasoning` (`rename = "reasoning_content"`)  |
//! | bare `reasoning` alias        | **NO**     | rig maps only `reasoning_content`, not the bare `reasoning` key  |
//! | `usage.*reasoning_tokens`     | **NO**     | rig's chat `Usage` is `{prompt,total,prompt_tokens_details}` only; the conversion hardcodes `reasoning_tokens: 0` |
//!
//! The offline `#[test]`s below encode that verdict as executable, network-free assertions. The real
//! #300 verification against the live gateway lives in the sibling `rig_live_probe.rs` target (an
//! `#[ignore]`d, post-merge step); see the run command in that file.

use rig_core::providers::openai::completion::{CompletionResponse, Message};
use serde_json::{Value, json};

/// A per-field verdict: did rig's OpenAI provider preserve the field through a parse round-trip?
#[derive(Debug, Clone, PartialEq)]
struct FieldVerdict {
    field: &'static str,
    preserved: bool,
    detail: String,
}

impl FieldVerdict {
    fn new(field: &'static str, preserved: bool, detail: impl Into<String>) -> Self {
        Self {
            field,
            preserved,
            detail: detail.into(),
        }
    }

    /// A single-line, human-readable row for the PR verdict table / test output.
    fn line(&self) -> String {
        format!(
            "[{}] {} — {}",
            if self.preserved {
                "PRESERVED"
            } else {
                "DROPPED"
            },
            self.field,
            self.detail
        )
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────────────────────────────
// Realistic OpenAI-compatible `chat.completion` bodies, shaped to match what eaig emits. Cross-checked
// against `src/review/native/chat.rs`: `content: null` on a tool-call turn (parsed as `Option::None`
// there), the `extra_content = {"google":{"thought_signature"}}` envelope from the `ToolCall` doc, the
// `reasoning_content` field (with the `reasoning` alias), and the top-level + nested `reasoning_tokens`
// shapes from the `Usage` doc.

/// The `thought_signature` value we expect to survive a round-trip verbatim.
const THOUGHT_SIGNATURE: &str = "CikBq9F0eXAMPLEsignatureBytesBase64==";

/// A Gemini-3-style assistant turn: one tool call carrying its `thought_signature` in `extra_content`.
fn gemini_tool_call_response() -> Value {
    json!({
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "created": 1_720_000_000u64,
        "model": "gemini-3-pro",
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
                    },
                    "extra_content": { "google": { "thought_signature": THOUGHT_SIGNATURE } }
                }]
            }
        }],
        "usage": { "prompt_tokens": 100, "total_tokens": 140 }
    })
}

/// A plain reasoning-model turn. `reasoning_field` selects the wire key (`reasoning_content` — the
/// standard DeepSeek/GLM name — vs the bare `reasoning` alias our transport also accepts).
fn reasoning_response(reasoning_field: &str) -> Value {
    json!({
        "id": "chatcmpl-def456",
        "object": "chat.completion",
        "created": 1_720_000_001u64,
        "model": "glm-5.2",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "Final answer.",
                reasoning_field: "Let me think step by step: 1, 2, 3."
            }
        }],
        "usage": { "prompt_tokens": 19, "total_tokens": 238 }
    })
}

/// A turn whose `usage` carries the reasoning-token breakdown both ways: nested under
/// `completion_tokens_details` (OpenAI-style) and at the top level (camer.digital's gateway).
fn usage_with_reasoning_tokens() -> Value {
    json!({
        "id": "chatcmpl-ghi789",
        "object": "chat.completion",
        "created": 1_720_000_002u64,
        "model": "gpt-5.2",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": "Done." }
        }],
        "usage": {
            "prompt_tokens": 200,
            "completion_tokens": 90,
            "total_tokens": 290,
            "completion_tokens_details": { "reasoning_tokens": 64 },
            "reasoning_tokens": 64
        }
    })
}

// ── Assertion helpers (deserialize → inspect / re-serialize through rig's typed model) ─────────────

/// Parse a JSON body through rig's OpenAI Chat Completions response type. Panics with a legible message
/// if rig cannot even parse the shape (itself a fidelity failure worth surfacing loudly).
fn parse(body: &Value) -> CompletionResponse {
    serde_json::from_value(body.clone())
        .expect("rig-core should deserialize a standard OpenAI-compatible chat.completion body")
}

/// The first assistant tool call rig parsed, **re-serialized** to JSON — this is exactly the shape rig
/// would echo back on the next turn. Any provider-quirk field it can't model is gone from this output.
fn first_tool_call_roundtrip(resp: &CompletionResponse) -> Value {
    let choice = resp.choices.first().expect("a choice");
    match &choice.message {
        Message::Assistant { tool_calls, .. } => {
            let call = tool_calls.first().expect("a tool call");
            serde_json::to_value(call).expect("rig ToolCall re-serializes")
        }
        other => panic!("expected an assistant message, got {other:?}"),
    }
}

/// Verdict for `thought_signature`: does the `extra_content` envelope survive a rig parse→serialize
/// round-trip (so it could be echoed back verbatim)?
fn thought_signature_verdict() -> FieldVerdict {
    let resp = parse(&gemini_tool_call_response());
    let roundtripped = first_tool_call_roundtrip(&resp);

    // Sanity: rig DID parse the call itself (id + function survive) — only the quirk field is at risk.
    assert_eq!(
        roundtripped["id"], "call_1",
        "rig should keep the tool call id"
    );
    assert_eq!(
        roundtripped["function"]["name"], "vector_semantic_search",
        "rig should keep the function name"
    );

    let recovered = roundtripped
        .pointer("/extra_content/google/thought_signature")
        .and_then(Value::as_str);
    let preserved = recovered == Some(THOUGHT_SIGNATURE);
    let detail = if preserved {
        "extra_content round-trips verbatim; safe to echo back".to_string()
    } else {
        format!(
            "extra_content absent after round-trip (rig ToolCall models only id/type/function); \
             re-serialized call = {roundtripped}"
        )
    };
    FieldVerdict::new("thought_signature", preserved, detail)
}

/// Verdict for reasoning capture under a given wire key (`reasoning_content` or the `reasoning` alias).
fn reasoning_verdict(reasoning_field: &'static str) -> FieldVerdict {
    let resp = parse(&reasoning_response(reasoning_field));
    let choice = resp.choices.first().expect("a choice");
    let recovered = match &choice.message {
        Message::Assistant { reasoning, .. } => reasoning.clone(),
        other => panic!("expected an assistant message, got {other:?}"),
    };
    let preserved = recovered.as_deref() == Some("Let me think step by step: 1, 2, 3.");
    let detail = if preserved {
        format!("`{reasoning_field}` recovered off the assistant message")
    } else {
        format!("`{reasoning_field}` not recovered (rig maps only `reasoning_content`)")
    };
    FieldVerdict::new("reasoning_content", preserved, detail)
}

/// Verdict for reasoning-token usage: does the count survive rig's conversion into its provider-neutral
/// `completion::Usage`?
fn usage_reasoning_verdict() -> FieldVerdict {
    let resp = parse(&usage_with_reasoning_tokens());
    let core: rig_core::completion::CompletionResponse<CompletionResponse> = resp
        .try_into()
        .expect("rig converts a valid chat.completion into its neutral response type");

    // Prompt/total tokens are expected to survive; only the reasoning slice is at risk.
    assert_eq!(core.usage.input_tokens, 200, "prompt tokens should survive");
    assert_eq!(core.usage.total_tokens, 290, "total tokens should survive");

    let preserved = core.usage.reasoning_tokens == 64;
    let detail = if preserved {
        "reasoning_tokens survives into completion::Usage".to_string()
    } else {
        format!(
            "reasoning_tokens dropped: fixture reported 64 (nested + top-level), rig exposes {}",
            core.usage.reasoning_tokens
        )
    };
    FieldVerdict::new("usage.reasoning_tokens", preserved, detail)
}

/// The full verdict, one line per field — printed by the tests and reproduced in the PR body.
fn full_verdict() -> Vec<FieldVerdict> {
    vec![
        thought_signature_verdict(),
        reasoning_verdict("reasoning_content"),
        reasoning_verdict("reasoning"),
        usage_reasoning_verdict(),
    ]
}

// ── Tests: the executable verdict (network-free; green in CI) ──────────────────────────────────────

/// FIDELITY GAP: rig's OpenAI-compatible provider **drops** the Gemini `thought_signature`. Its
/// `ToolCall` type has no `extra_content` field, so the blob vanishes on parse and cannot be echoed
/// back — reproducing the #262 400 on the next turn. Our hand-rolled transport keeps it; rig does not.
#[test]
fn thought_signature_is_dropped_by_rig_openai_provider() {
    let v = thought_signature_verdict();
    println!("{}", v.line());
    assert!(
        !v.preserved,
        "regression check: if rig 0.39.0 started preserving extra_content this assertion should be \
         flipped and ADR-0075 revisited — {}",
        v.detail
    );
}

/// PRESERVED: the standard `reasoning_content` key is recovered off the assistant message.
#[test]
fn reasoning_content_is_preserved() {
    let v = reasoning_verdict("reasoning_content");
    println!("{}", v.line());
    assert!(v.preserved, "{}", v.detail);
}

/// FIDELITY GAP: rig maps only `reasoning_content`, not the bare `reasoning` alias some gateways emit —
/// which our transport handles via `#[serde(alias = "reasoning")]`. A model that reports thinking under
/// `reasoning` would log `reasoning_chars: 0` through rig (the deep-tier GLM symptom, #220).
#[test]
fn bare_reasoning_alias_is_dropped_by_rig() {
    let v = reasoning_verdict("reasoning");
    println!("{}", v.line());
    assert!(
        !v.preserved,
        "regression check: rig started honoring the bare `reasoning` alias — {}",
        v.detail
    );
}

/// FIDELITY GAP: rig's Chat Completions `Usage` cannot represent reasoning tokens — the conversion
/// hardcodes `reasoning_tokens: 0`. Both the nested `completion_tokens_details.reasoning_tokens` and
/// the top-level fallback our transport reads are lost. (Rig's newer **Responses API** path does keep
/// them, but eaig speaks Chat Completions, so that path is not the one we'd use.)
#[test]
fn usage_reasoning_tokens_are_dropped_by_rig_chat_completions() {
    let v = usage_reasoning_verdict();
    println!("{}", v.line());
    assert!(
        !v.preserved,
        "regression check: rig's chat Usage started exposing reasoning tokens — {}",
        v.detail
    );
}

/// Prints the whole verdict table and asserts the overall shape (exactly one field — `reasoning_content`
/// — is preserved by rig's OpenAI provider). This is the summary reproduced in the PR Verification body.
#[test]
fn fidelity_verdict_summary() {
    let verdict = full_verdict();
    println!("\n=== rig-core 0.39.0 OpenAI-provider fidelity verdict ===");
    for v in &verdict {
        println!("{}", v.line());
    }
    let preserved: Vec<&str> = verdict
        .iter()
        .filter(|v| v.preserved)
        .map(|v| v.field)
        .collect();
    assert_eq!(
        preserved,
        vec!["reasoning_content"],
        "only reasoning_content should survive rig's OpenAI Chat Completions provider"
    );
}
