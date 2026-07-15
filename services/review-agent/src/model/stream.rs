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
use super::serde_ext::null_as_default;

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
    /// Null-tolerant: several backends stream reasoning/content deltas carrying an explicit
    /// `"tool_calls": null` (GLM-5.2, MiMo). `#[serde(default)]` alone rejects that null and fails the
    /// whole chunk, silently dropping the delta's reasoning/content — the `reasoning_chars: 0` bug
    /// (#411). `null_as_default` collapses both an absent key and an explicit `null` to an empty vec.
    #[serde(default, deserialize_with = "null_as_default")]
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
                    let chunk = match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            // A `data:` line that is neither `[DONE]` nor empty but still fails to
                            // parse means we are DROPPING a delta (its content/reasoning/tool_calls).
                            // This is exactly how the #411 null-`tool_calls` bug hid for months, so
                            // surface it — bounded snippet, `debug` so legitimate keep-alives stay
                            // quiet — instead of skipping silently. The next backend that introduces a
                            // novel null shape is then caught immediately.
                            if !data.is_empty() {
                                let snippet: String = data.chars().take(200).collect();
                                tracing::debug!(
                                    %error,
                                    snippet = %snippet,
                                    "dropping unparseable stream chunk"
                                );
                            }
                            continue;
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one full `data:` chunk and return its first choice's `(content, reasoning_content)`.
    /// Panics (fails the test) if the chunk does not deserialize — which is the whole point of the
    /// #411 corpus: the null-`tool_calls` backends used to fail here.
    fn delta_of(data: &str) -> (Option<String>, Option<String>) {
        let chunk: StreamChunk = serde_json::from_str(data)
            .unwrap_or_else(|e| panic!("chunk failed to parse: {e}\n{data}"));
        let delta = chunk.choices.into_iter().next().expect("a choice").delta;
        (delta.content, delta.reasoning_content)
    }

    // --- Backends that OMIT `tool_calls` when there are none: must keep parsing (no regression). ---

    #[test]
    fn ministral_absent_tool_calls_still_parse() {
        // Mistral: first delta then a content delta, neither carries a `tool_calls` key.
        let (c, r) = delta_of(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}]}"#,
        );
        assert_eq!(c.as_deref(), Some(""));
        assert_eq!(r, None);

        let (c, r) =
            delta_of(r#"{"choices":[{"index":0,"delta":{"content":"H"},"finish_reason":null}]}"#);
        assert_eq!(c.as_deref(), Some("H"));
        assert_eq!(r, None);
    }

    #[test]
    fn qwen_absent_tool_calls_reasoning_captured() {
        // Fireworks qwen3p7-plus: reasoning delta, then an empty delta — no `tool_calls` key.
        let (c, r) = delta_of(
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Thinking"},"finish_reason":null}]}"#,
        );
        assert_eq!(c, None);
        assert_eq!(r.as_deref(), Some("Thinking"));

        let (c, r) = delta_of(r#"{"choices":[{"index":0,"delta":{},"finish_reason":null}]}"#);
        assert_eq!(c, None);
        assert_eq!(r, None);
    }

    // --- Backends that send explicit `"tool_calls": null`: used to drop the WHOLE chunk (#411). ---

    #[test]
    fn glm_null_tool_calls_reasoning_now_captured() {
        // zai-org GLM-5.2 (the deep tier). Extra top-level fields (`service_tier`, `usage`) are
        // unknown to `StreamChunk`/`StreamChoice` and must be ignored, not fail the parse.
        let (c, r) = delta_of(
            r#"{"service_tier":"default","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":"alyze","tool_calls":null},"logprobs":null,"finish_reason":null}],"usage":null}"#,
        );
        assert_eq!(c.as_deref(), Some(""));
        assert_eq!(r.as_deref(), Some("alyze"));
    }

    #[test]
    fn mimo_null_tool_calls_and_null_role_reasoning_captured() {
        // XiaomiMiMo mimo-v2p5: also `"role": null` (harmless — `StreamDelta` has no `role` field).
        let (c, r) = delta_of(
            r#"{"service_tier":"default","choices":[{"index":0,"delta":{"role":null,"content":"","reasoning_content":"The user is","tool_calls":null},"logprobs":null,"finish_reason":null}],"usage":null}"#,
        );
        assert_eq!(c.as_deref(), Some(""));
        assert_eq!(r.as_deref(), Some("The user is"));
    }

    #[test]
    fn mimo_pro_null_tool_calls_content_now_captured() {
        // XiaomiMiMo mimo-v2p5-pro: content deltas ALSO carry `"tool_calls": null` and
        // `"reasoning_content": null`, so before the fix even the answer TEXT was lost.
        let (c, r) = delta_of(
            r#"{"choices":[{"index":0,"delta":{"role":null,"content":"Hello","reasoning_content":null,"tool_calls":null},"logprobs":null,"finish_reason":null}]}"#,
        );
        assert_eq!(c.as_deref(), Some("Hello"));
        assert_eq!(r, None);
    }

    #[test]
    fn null_tool_calls_leaves_tool_calls_empty() {
        // The null must collapse to an empty vec, not a phantom tool call.
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"content":"x","tool_calls":null},"finish_reason":null}]}"#,
        )
        .expect("parses");
        assert!(chunk.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn real_tool_call_array_still_reassembles() {
        // A genuine tool-call delta (non-null array) must still deserialize its index/id/function.
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"p\":"}}]},"finish_reason":null}]}"#,
        )
        .expect("parses");
        let tc = &chunk.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].index, 0);
        assert_eq!(tc[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            tc[0].function.as_ref().unwrap().name.as_deref(),
            Some("read_file")
        );
    }
}
