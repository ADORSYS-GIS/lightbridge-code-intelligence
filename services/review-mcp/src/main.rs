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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use lci_review_agent::tools::Tools;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

/// OpenCode prefixes every MCP tool with this server's config key (`lightbridge`), so an advertised
/// `read_file` reaches the model as `lightbridge_read_file`. A few review tools are ALSO natively
/// `lightbridge_`-prefixed (`lightbridge_vector_semantic_search`, `lightbridge_graph_find_symbol`,
/// `lightbridge_graph_get_callers`) — advertising those verbatim makes OpenCode render
/// `lightbridge_lightbridge_…` (a doubled prefix), inconsistent with the bare-named peers.
const SERVER_PREFIX: &str = "lightbridge_";

/// The name to advertise for a canonical tool over MCP: strip a leading `lightbridge_` so OpenCode
/// adds exactly one server prefix and every tool reads `lightbridge_<tool>`. Bare names pass through
/// unchanged (`read_file` → `read_file` → OpenCode → `lightbridge_read_file`). This is the inverse of
/// the recorder's `normalize_tool_name`, which maps the OpenCode-observed id back to the canonical
/// name for the reused coverage gates — so the const names, the native path, and the goldens are all
/// untouched; only the MCP-advertised surface changes.
fn advertised_name(canonical: &str) -> &str {
    canonical.strip_prefix(SERVER_PREFIX).unwrap_or(canonical)
}

/// Build the advertised→canonical map from the registered specs, so a `tools/call` arriving under the
/// advertised (bare) name dispatches to the canonical registry entry. Stripping is only lossy if two
/// canonical names collide on the same advertised name (e.g. both `read_file` and
/// `lightbridge_read_file`); the review surface has no such pair, so the map is a clean bijection.
fn advertised_to_canonical<I>(canonical_names: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = String>,
{
    canonical_names
        .into_iter()
        .map(|canonical| (advertised_name(&canonical).to_string(), canonical))
        .collect()
}

/// Read a required env var, trimmed — a stray newline/space (e.g. from a here-doc or a k8s
/// `stringData` value) must not break a UUID/URL parse downstream.
fn env(key: &str) -> Result<String> {
    std::env::var(key)
        .map(|value| value.trim().to_string())
        .with_context(|| format!("required env var {key} is not set"))
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

/// Map a review `ToolSpec` to an MCP tool definition. The advertised name drops a leading
/// `lightbridge_` (see [`advertised_name`]) so OpenCode's server-prefixing yields a single
/// `lightbridge_<tool>` for every tool.
fn mcp_tool(spec: &ToolSpec) -> Value {
    json!({
        "name": advertised_name(&spec.function.name),
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

async fn handle(tools: &Tools, name_map: &HashMap<String, String>, req: &Value) -> Option<Value> {
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
            let advertised = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // OpenCode calls back with the name we advertised (bare); map it to the canonical registry
            // name so dispatch matches. Unknown names pass through so `dispatch` renders its own
            // "unknown tool" error rather than us swallowing it.
            let name = name_map
                .get(advertised)
                .cloned()
                .unwrap_or_else(|| advertised.to_string());
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
    let tools = Arc::new(build_tools().context("initializing lci-review-mcp")?);
    // Advertised (bare) → canonical name map, built once from the registered specs so `tools/call`
    // arriving under the advertised name dispatches to the right registry entry.
    let name_map = Arc::new(advertised_to_canonical(
        tools.specs().into_iter().map(|spec| spec.function.name),
    ));

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
        let name_map = Arc::clone(&name_map);
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(resp) = handle(&tools, &name_map, &req).await
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
    fn advertises_bare_names_so_opencode_single_prefixes() {
        // A natively `lightbridge_`-prefixed tool is advertised WITHOUT the prefix, so OpenCode's own
        // server-prefix yields exactly `lightbridge_vector_semantic_search` (not a doubled prefix).
        assert_eq!(
            advertised_name("lightbridge_vector_semantic_search"),
            "vector_semantic_search"
        );
        assert_eq!(
            advertised_name("lightbridge_graph_find_symbol"),
            "graph_find_symbol"
        );
        // A bare tool passes through untouched.
        assert_eq!(advertised_name("read_file"), "read_file");
        // The MCP shape reflects the stripped name.
        let spec = ToolSpec::function(
            "lightbridge_vector_semantic_search",
            "search",
            json!({"type": "object"}),
        );
        assert_eq!(mcp_tool(&spec)["name"], "vector_semantic_search");
    }

    #[test]
    fn name_map_round_trips_advertised_back_to_canonical() {
        let canonical = [
            "lightbridge_vector_semantic_search",
            "lightbridge_graph_find_symbol",
            "lightbridge_graph_get_callers",
            "read_file",
            "add_review_comment",
            "finish",
        ];
        let map = advertised_to_canonical(canonical.iter().map(|s| s.to_string()));
        // A call under the advertised (bare) name resolves to the canonical registry name.
        assert_eq!(
            map.get("vector_semantic_search").map(String::as_str),
            Some("lightbridge_vector_semantic_search")
        );
        assert_eq!(
            map.get("graph_get_callers").map(String::as_str),
            Some("lightbridge_graph_get_callers")
        );
        // Bare tools map to themselves.
        assert_eq!(map.get("read_file").map(String::as_str), Some("read_file"));
        assert_eq!(map.get("finish").map(String::as_str), Some("finish"));
        // Every advertised key is distinct (clean bijection — no lossy collision).
        assert_eq!(map.len(), canonical.len());
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
