//! Streaming (SSE) collection (spike): the chunk wire shapes and the reassembly loop that turns a
//! `data: {chunk}\n\n` event stream into the same [`Completion`] the non-stream path produces.
//!
//! A streamed completion arrives as `data: {chunk}\n\n` events; each chunk carries *deltas* in
//! `choices[0].delta`: `content` fragments, and `tool_calls` whose `function.name`/`arguments` are
//! split across chunks and reassembled by `index`. The final chunk carries `finish_reason` and (with
//! `include_usage`) `usage`.

use serde::Deserialize;

use lci_agent_clients::ratelimit::RateLimitSnapshot;
use lci_agent_loop::ChatMessage;
use lci_agent_types::{FunctionCall, ToolCall};

use super::client::ChatClient;
use super::completion::{Completion, Usage};
use super::retry::ChatError;

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    /// A provider may report a mid-stream failure as a `data: {"error": …}` event (no `choices`).
    /// Surfaced so the collector fails the turn instead of finishing with an empty message.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning-model thinking deltas (DeepSeek/GLM lineage), reassembled across chunks into
    /// [`Completion::reasoning`] for the transcript/logs (epic #137 proof-of-work). Not echoed back to
    /// the model on the next turn. `reasoning` alias: some gateways stream the deltas under that key, so
    /// without it a streamed reasoning model logs `reasoning_chars: 0` (the deep-tier GLM-5.2 symptom).
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFn>,
    /// Provider round-trip blob (Gemini's `thought_signature` envelope). Streamed on the tool-call
    /// delta — captured verbatim so it survives into the reassembled [`ToolCall::extra_content`] and is
    /// echoed back on the next turn. Arrives whole (not split like `arguments`), so last-writer-wins.
    #[serde(default)]
    extra_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// One tool call being reassembled across stream chunks.
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
    /// Provider round-trip blob (Gemini `thought_signature`), captured verbatim from the delta.
    extra_content: Option<serde_json::Value>,
}

impl ChatClient {
    /// Collect a streamed (SSE) completion (spike): reassemble `content` + `tool_calls` deltas (and the
    /// final `usage`) from `data:` chunks, bounding the silence between chunks by `idle_timeout` so a
    /// stalled stream fails fast (transient → retryable) while a long-but-progressing one completes.
    pub(super) async fn collect_stream(
        &self,
        response: reqwest::Response,
        rate_limit: RateLimitSnapshot,
    ) -> Result<Completion, ChatError> {
        use futures::StreamExt;
        let transient = |error: anyhow::Error| ChatError {
            error,
            transient: true,
            retry_after: None,
        };

        let mut stream = response.bytes_stream();
        // Raw byte buffer: HTTP chunks split at arbitrary byte boundaries, so we must NOT decode each
        // chunk on its own (a multi-byte UTF-8 char split across chunks would corrupt). We strip `\r`
        // as bytes arrive (normalising CRLF SSE `\r\n\r\n` → `\n\n`), then decode only *complete*
        // events. (Gemini/Codex review on #206.)
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;
        let mut tools: Vec<ToolCallAcc> = Vec::new();
        let mut done = false; // saw an explicit `data: [DONE]`

        loop {
            let chunk = match tokio::time::timeout(self.idle_timeout, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    return Err(transient(
                        anyhow::Error::new(e).context("reading chat stream chunk"),
                    ));
                }
                Ok(None) => break, // stream closed — completeness checked after the loop
                Err(_) => {
                    return Err(transient(anyhow::anyhow!(
                        "chat stream idle for {:?} (no chunk) — treating as a stall",
                        self.idle_timeout
                    )));
                }
            };
            buf.extend(chunk.iter().copied().filter(|&b| b != b'\r'));

            // SSE events are separated by a blank line; drain + decode each *complete* event whole.
            while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                let event = buf.drain(..pos + 2).collect::<Vec<u8>>();
                let event = String::from_utf8_lossy(&event);
                for line in event.lines() {
                    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    if data == "[DONE]" {
                        done = true;
                        continue;
                    }
                    let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                        continue; // keep-alive / unparseable fragment — skip
                    };
                    // A mid-stream provider error (no `choices`) must fail the turn, not finish empty.
                    if let Some(err) = chunk.error {
                        return Err(transient(anyhow::anyhow!(
                            "chat stream returned an error event: {err}"
                        )));
                    }
                    if chunk.usage.is_some() {
                        usage = chunk.usage;
                    }
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };
                    if choice.finish_reason.is_some() {
                        finish_reason = choice.finish_reason;
                    }
                    if let Some(c) = choice.delta.content {
                        content.push_str(&c);
                    }
                    if let Some(r) = choice.delta.reasoning_content {
                        reasoning.push_str(&r);
                    }
                    for tc in choice.delta.tool_calls {
                        if tools.len() <= tc.index {
                            tools.resize_with(tc.index + 1, ToolCallAcc::default);
                        }
                        let acc = &mut tools[tc.index];
                        if let Some(id) = tc.id {
                            acc.id = id;
                        }
                        if let Some(ec) = tc.extra_content {
                            acc.extra_content = Some(ec);
                        }
                        if let Some(f) = tc.function {
                            if let Some(n) = f.name {
                                acc.name.push_str(&n);
                            }
                            if let Some(a) = f.arguments {
                                acc.arguments.push_str(&a);
                            }
                        }
                    }
                }
            }
        }

        // An upstream/proxy that closed the stream before a terminal signal left us with a partial
        // (possibly half-built tool call). Treat it as transient so the turn retries, rather than
        // returning a "successful" empty/partial completion. (Codex review on #206.)
        if !done && finish_reason.is_none() {
            return Err(transient(anyhow::anyhow!(
                "chat stream closed before completion (no finish_reason / [DONE])"
            )));
        }

        let tool_calls: Vec<ToolCall> = tools
            .into_iter()
            .filter(|a| !a.id.is_empty() || !a.name.is_empty())
            .map(|a| ToolCall {
                id: a.id,
                kind: "function".to_string(),
                function: FunctionCall {
                    name: a.name,
                    arguments: a.arguments,
                },
                extra_content: a.extra_content,
            })
            .collect();

        Ok(Completion {
            finish_reason,
            usage,
            rate_limit,
            reasoning: (!reasoning.trim().is_empty()).then_some(reasoning),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: (!content.is_empty()).then_some(content),
                tool_calls,
                tool_call_id: None,
            },
        })
    }
}
