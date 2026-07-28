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
//!
//! `LCI_MCP_MIN_PRIORITY` (optional, ADR-0030): the repo's `severity.min`, when declared — a finding
//! below this priority is acknowledged to the model but never sent to the control plane.
//!
//! SAST (ADR-0073) is opt-in and off unless the supervisor sets the `LCI_MCP_SAST_*` group (the
//! [`SastConfig`] round-trip) AND `LCI_MCP_SAST_CHANGED_FILES` (the diff's file set to scope a scan to).
//! It only sets those when `run_sast` cleared the allowlist + diff-present + enabled gate, so their mere
//! presence is the "offer `run_sast`" signal — absent, the tool isn't registered and never reaches
//! `tools/list` or dispatch (the opt-in surface rule, enforced here structurally).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use lci_agent_clients::{ControlPlaneClient, DiscoveredTool, EmbeddingsClient};
use lci_agent_sast::{ENV_CHANGED_FILES, SastConfig};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use lci_review_agent::policies::SastLeadSink;
use lci_review_agent::tools::{SastToolConfig, Tools};
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
/// canonical names collide on the same advertised name (e.g. both `read_file` and `lightbridge_read_file`);
/// the review surface has no such pair today, but a future tool rename that introduced one would ALSO make
/// `tools/list` advertise two tools under one name — so this fails fast at startup (the server won't run)
/// rather than silently misrouting dispatch in production reviews.
fn advertised_to_canonical<I>(canonical_names: I) -> Result<HashMap<String, String>>
where
    I: IntoIterator<Item = String>,
{
    let mut map = HashMap::new();
    for canonical in canonical_names {
        let advertised = advertised_name(&canonical).to_string();
        if let Some(existing) = map.insert(advertised.clone(), canonical.clone()) {
            anyhow::bail!(
                "tool-name collision: {existing:?} and {canonical:?} both advertise as {advertised:?}"
            );
        }
    }
    Ok(map)
}

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
/// `RegistryError::DuplicateName`, and `Tools::with_sast` propagates it — a hard startup failure. The
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

/// Resolve the `run_sast` tool config (ADR-0073) from the SAST env group, or `None` when SAST wasn't
/// offered this run. Returns `None` unless BOTH the [`SastConfig`] round-trip resolves ([`SastConfig::from_env`])
/// AND the changed-file list is present — the supervisor only sets both together, past the opt-in gate.
/// The changed-file list is the scan's scope (and the widen-guard) — the same `SastToolConfig::changed_files`
/// the native path passes. The leads sink is a local throwaway: `run_sast` pushes into it, but the
/// `SastAnchorGate` that reads leads lives supervisor-side and recovers them from this tool's result
/// digest instead (the two processes don't share memory).
fn resolve_sast_tool_config() -> Result<Option<SastToolConfig>> {
    let Some(config) = SastConfig::from_env() else {
        return Ok(None);
    };
    let Some(list_path) = std::env::var(ENV_CHANGED_FILES)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&list_path)
        .with_context(|| format!("reading the SAST changed-file list at {list_path}"))?;
    let changed_files: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
    Ok(Some(SastToolConfig {
        config,
        changed_files,
        leads,
    }))
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
    let sast = resolve_sast_tool_config()?;
    // Repo `severity.min` (ADR-0030) — absent unless the repo declared one.
    let min_priority = std::env::var("LCI_MCP_MIN_PRIORITY")
        .ok()
        .filter(|v| !v.is_empty());

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

    Tools::with_sast(
        &client,
        &embedder,
        task_id,
        &checkout,
        filter_discovered(discovered),
        sast,
        min_priority,
    )
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
    let tools = Arc::new(build_tools().await.context("initializing lci-review-mcp")?);
    // Advertised (bare) → canonical name map, built once from the registered specs so `tools/call`
    // arriving under the advertised name dispatches to the right registry entry.
    let name_map = Arc::new(
        advertised_to_canonical(tools.specs().into_iter().map(|spec| spec.function.name))
            .context("building the review tool-name map")?,
    );

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
        // A discovered `mcp__…` name has no leading `lightbridge_`, so it advertises unchanged.
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
            tool("read_file"), // non-mcp__: would collide with the built-in read_file → dropped
            tool("mcp__acme__a"), // duplicate → dropped
            tool("mcp__acme__b"),
        ]);
        let names: Vec<_> = out.iter().map(|s| s.name().to_string()).collect();
        assert_eq!(names, vec!["mcp__acme__a", "mcp__acme__b"]);
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
        let map = advertised_to_canonical(canonical.iter().map(|s| s.to_string())).unwrap();
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
    fn name_map_fails_fast_on_an_advertised_name_collision() {
        // A bare tool and its `lightbridge_`-prefixed twin both advertise as `read_file` — that would
        // shadow one tool in dispatch AND emit a duplicate in tools/list, so it must hard-error rather
        // than silently drop one.
        let clash = ["read_file", "lightbridge_read_file"];
        let err = advertised_to_canonical(clash.iter().map(|s| s.to_string()))
            .expect_err("colliding advertised names must fail");
        assert!(err.to_string().contains("read_file"));
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
