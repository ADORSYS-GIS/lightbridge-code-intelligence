//! `lci-review-mcp` — a stdio MCP server that exposes the review agent's tuned tools to an OpenCode
//! ACP host (RFC-0009 / ADR-0094 review cutover, slice 1).
//!
//! This is the load-bearing missing piece for running review on OpenCode: the review tools
//! (`add_review_comment`, `finish`, retrieval, graph, `read_file`, …) live as in-process Rust in
//! `lci-review-agent` and proxy to the control plane's `/internal/tasks/{id}/…` API. OpenCode can't
//! call in-process Rust — so this binary re-exposes them over MCP by REUSING
//! `lci_review_agent::tools::Tools` verbatim (its `specs()` for `tools/list`, its `dispatch()` for
//! `tools/call`). The tuned schemas and behaviour are inherited, not re-declared, so review quality
//! is preserved by construction.
//!
//! The OpenCode review config (slice 2) points its mediated MCP at `command: ["lci-review-mcp"]`;
//! the ACP supervisor (slice 3) sets the env below from the task context and spawns opencode, which
//! spawns this. `finish`/`abort` return a plain result here — their loop-terminal meaning is observed
//! host-side by the gate-interlock/recorder (they see the `finish` tool call), not signalled across
//! this process boundary.
//!
//! Env (set by the supervisor per task):
//!   LCI_MCP_CP_URL, LCI_MCP_RUNNER_TOKEN, LCI_MCP_TASK_ID, LCI_MCP_CHECKOUT,
//!   LCI_MCP_EMBED_URL, LCI_MCP_EMBED_KEY, LCI_MCP_EMBED_MODEL

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use lci_agent_clients::{ControlPlaneClient, DiscoveredTool, EmbeddingsClient};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use lci_review_agent::tools::Tools;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

/// Read a required env var, trimmed — a stray newline/space (e.g. from a here-doc or a k8s
/// `stringData` value) must not break a UUID/URL parse downstream.
fn env(key: &str) -> Result<String> {
    std::env::var(key)
        .map(|value| value.trim().to_string())
        .with_context(|| format!("required env var {key} is not set"))
}

/// Map an ADR-0066 discovered MCP tool to a review `ToolSpec`. The control plane already reports it
/// `mcp__<server>__<tool>`-prefixed, so it folds into the registry verbatim; dispatch of the result
/// routes back through the control plane's `call_knowledge_tool` inside [`Tools`] — the runner never
/// talks to an external MCP server directly.
fn discovered_to_spec(tool: DiscoveredTool) -> ToolSpec {
    ToolSpec::function(tool.name, tool.description, tool.input_schema)
}

/// Defensively fold discovered ADR-0066 tools into specs so a malformed discovery result degrades
/// instead of wedging the review. The registry rejects a repeated name with
/// `RegistryError::DuplicateName`, and `Tools::new` propagates it — a hard startup failure. The
/// control plane already `mcp__`-prefixes discovered names (so a well-formed one can't collide with a
/// built-in — none of which are `mcp__`), but a buggy or duplicate customer `tools/list` must be
/// dropped, not fatal: keep only correctly-prefixed names, first occurrence of each wins.
fn filter_discovered(tools: Vec<DiscoveredTool>) -> Vec<ToolSpec> {
    let mut seen = std::collections::HashSet::new();
    tools
        .into_iter()
        .filter(|tool| {
            if !tool.name.starts_with(lci_review_agent::tools::MCP_TOOL_PREFIX) {
                // stderr only — stdout is the JSON-RPC channel.
                eprintln!(
                    "lci-review-mcp: dropping discovered tool {:?} — not `mcp__`-prefixed (would risk a built-in collision)",
                    tool.name
                );
                return false;
            }
            if !seen.insert(tool.name.clone()) {
                eprintln!("lci-review-mcp: dropping duplicate discovered tool {:?}", tool.name);
                return false;
            }
            true
        })
        .map(discovered_to_spec)
        .collect()
}

async fn build_tools() -> Result<Tools> {
    let client = ControlPlaneClient::new(env("LCI_MCP_CP_URL")?, env("LCI_MCP_RUNNER_TOKEN")?);
    let embedder = EmbeddingsClient::new(
        &env("LCI_MCP_EMBED_URL")?,
        env("LCI_MCP_EMBED_KEY")?,
        env("LCI_MCP_EMBED_MODEL")?,
    );
    let task_id: Uuid = env("LCI_MCP_TASK_ID")?
        .parse()
        .context("LCI_MCP_TASK_ID is not a valid UUID")?;
    let checkout = PathBuf::from(env("LCI_MCP_CHECKOUT")?);

    // ADR-0066 external-knowledge MCP tools: discover whatever MCP servers the control plane is
    // configured with (owner-managed in ai-helm-values) and fold them into the registry as mediated
    // `mcp__<server>__<tool>` tools — the same surface the native review loop offered. RFC-0009
    // slice 1 stubbed this with `std::iter::empty()` ("wired in a later slice"); this IS that slice,
    // so a customer's configured MCP is reachable on the OpenCode review path with zero per-customer
    // runner code. Dispatch routes back through the control plane (`call_knowledge_tool`) inside
    // `Tools`, so the runner never talks to an external MCP directly — mediation preserved (the
    // deliberate boundary of ADR-0097 #6), and the results are size-capped + untrusted-framed CP-side.
    // Discovery is best-effort: a control-plane hiccup degrades to the core review tools rather than
    // failing the review (a customer's flaky MCP must never wedge a review).
    let discovered = client
        .list_knowledge_tools(task_id)
        .await
        .unwrap_or_else(|error| {
            // stderr only — stdout is the JSON-RPC channel and must carry nothing else.
            eprintln!(
                "lci-review-mcp: ADR-0066 knowledge-tool discovery failed; continuing with core review tools only: {error:#}"
            );
            Vec::new()
        });

    Tools::new(
        &client,
        &embedder,
        task_id,
        &checkout,
        filter_discovered(discovered),
    )
    .map_err(|error| anyhow::anyhow!("building the review tool registry: {error}"))
}

/// Map a review `ToolSpec` to an MCP tool definition.
fn mcp_tool(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.function.name,
        "description": spec.function.description,
        "inputSchema": spec.function.parameters,
    })
}

/// Render a dispatched `ToolOutcome` as an MCP `tools/call` result.
fn mcp_result(outcome: ToolOutcome) -> Value {
    let text = match outcome {
        ToolOutcome::Continue(text) => text,
        // `finish`/`abort` are loop-terminal for the native loop; over MCP they just return a result.
        // The OpenCode host observes the `finish`/`abort` tool CALL itself (recorder/gate) and drives
        // finalization — this text is only what the model sees.
        ToolOutcome::Finish => "Review finished; the host will finalize.".to_string(),
        ToolOutcome::Abort(reason) => format!("Review aborted: {reason}"),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

async fn handle(tools: &Tools, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications (no id) get no response.
    id.as_ref()?;

    let result = match method {
        "initialize" => json!({
            "protocolVersion": req.get("params").and_then(|p| p.get("protocolVersion")).cloned()
                .unwrap_or_else(|| json!("2025-03-26")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "lci-review-mcp", "version": env!("CARGO_PKG_VERSION") },
        }),
        "tools/list" => json!({ "tools": tools.specs().iter().map(mcp_tool).collect::<Vec<_>>() }),
        "tools/call" => {
            let params = req.get("params");
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // MCP passes arguments as an object; the review tools deserialize from a JSON string.
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .map(|a| a.to_string())
                .unwrap_or_else(|| "{}".to_string());
            let call = ToolCallReq {
                id: Uuid::new_v4().to_string(),
                kind: "function".to_string(),
                function: FunctionCallReq { name, arguments },
                extra_content: None,
            };
            mcp_result(tools.dispatch(&call).await)
        }
        "ping" => json!({}),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let tools = Arc::new(build_tools().await.context("initializing lci-review-mcp")?);

    // Each request runs in its own task so batched tool calls (the review does parallel read-only
    // tool batching, ADR-0042) don't serialize behind each other; a single writer task owns stdout
    // so the concurrent responses never interleave. JSON-RPC correlates by `id`, so out-of-order
    // responses are fine.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let tools = Arc::clone(&tools);
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(resp) = handle(&tools, &req).await
                && let Ok(line) = serde_json::to_string(&resp)
            {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx); // close the channel so the writer drains and exits after stdin EOF
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_toolspec_to_mcp_shape() {
        let spec = ToolSpec::function("t", "desc", json!({"type": "object"}));
        let m = mcp_tool(&spec);
        assert_eq!(m["name"], "t");
        assert_eq!(m["description"], "desc");
        assert_eq!(m["inputSchema"]["type"], "object");
    }

    #[test]
    fn folds_discovered_mcp_tool_into_the_registry_shape() {
        // ADR-0066: a discovered tool arrives already `mcp__<server>__<tool>`-prefixed and is folded
        // in verbatim (name/description/schema preserved) so opencode can offer it; its dispatch
        // routing to `call_knowledge_tool` is covered by `lci_review_agent::tools`.
        let spec = discovered_to_spec(DiscoveredTool {
            name: "mcp__acme__search".into(),
            description: "acme search".into(),
            input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        });
        assert_eq!(spec.name(), "mcp__acme__search");
        let m = mcp_tool(&spec);
        assert_eq!(m["name"], "mcp__acme__search");
        assert_eq!(m["description"], "acme search");
        assert_eq!(m["inputSchema"]["properties"]["q"]["type"], "string");
    }

    #[test]
    fn filter_discovered_drops_unprefixed_and_duplicate_tools() {
        // A malformed/duplicate customer tools/list must degrade, not wedge the review: the registry
        // rejects a duplicate name (and a non-`mcp__` name could collide with a built-in), which would
        // otherwise be a hard startup failure. Keep only `mcp__`-prefixed names, first occurrence wins.
        let tool = |name: &str| DiscoveredTool {
            name: name.into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        };
        let out = filter_discovered(vec![
            tool("mcp__acme__a"),
            tool("read_file"),    // non-mcp__: would collide with the built-in read_file → dropped
            tool("mcp__acme__a"), // duplicate → dropped
            tool("mcp__acme__b"),
        ]);
        let names: Vec<_> = out.iter().map(|s| s.name().to_string()).collect();
        assert_eq!(names, vec!["mcp__acme__a", "mcp__acme__b"]);
    }

    #[test]
    fn renders_outcomes_as_mcp_results() {
        assert_eq!(
            mcp_result(ToolOutcome::Continue("hi".into()))["content"][0]["text"],
            "hi"
        );
        assert_eq!(
            mcp_result(ToolOutcome::Continue("hi".into()))["isError"],
            false
        );
        assert!(
            mcp_result(ToolOutcome::Finish)["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("finalize")
        );
        assert!(
            mcp_result(ToolOutcome::Abort("boom".into()))["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("boom")
        );
    }
}
