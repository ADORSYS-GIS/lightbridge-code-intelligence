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

use anyhow::{Context, Result};
use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use lci_review_agent::tools::Tools;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("required env var {key} is not set"))
}

fn build_tools() -> Result<Tools> {
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
    // No discovered knowledge tools in slice 1 (the outbound brave-search/context7 MCP, ADR-0066,
    // is wired in a later slice); the empty iterator keeps the registry to the core review tools.
    Tools::new(&client, &embedder, task_id, &checkout, std::iter::empty())
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
    let tools = build_tools().context("initializing lci-review-mcp")?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = handle(&tools, &req).await {
            stdout
                .write_all(serde_json::to_string(&resp)?.as_bytes())
                .await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
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
