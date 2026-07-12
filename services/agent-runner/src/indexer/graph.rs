//! Structural graph indexing via Graphify (ADR-0019).
//!
//! Graphify is a standalone, multi-language (36 grammars) AST→graph extractor bundled into the
//! runner image. We spawn it as **`graphify update … --no-cluster`** — the AST-only, no-LLM path
//! ("re-extract code files and update the graph"). NOT `graphify extract`, which always runs a
//! semantic-LLM pass over docs and exits non-zero without an API key on any repo containing
//! markdown/docs. We parse the `graph.json` it writes and hand the **code** nodes+edges to the
//! control plane, which owns the Neo4j write. We deliberately do *not* use Graphify for embeddings —
//! it has none; the semantic (pgvector) path stays with our own tree-sitter chunker.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use lci_agent_clients::{
    ControlPlaneClient, GraphBatch, GraphEdgePayload, GraphNodePayload, TaskContext,
};

/// Graphify's `graph.json` shape (only the fields we consume). `update` writes edges under `links`
/// (older `extract` used `edges`); accept either.
#[derive(Debug, Deserialize)]
struct GraphFile {
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default, alias = "links")]
    edges: Vec<RawEdge>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    source_file: Option<String>,
    /// Graphify encodes the line as `"L42"`; may be absent for some node kinds.
    #[serde(default)]
    source_location: Option<String>,
    /// `code` | `document` | … — we keep only `code` so markdown structure stays out of the graph.
    #[serde(default)]
    file_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEdge {
    source: String,
    target: String,
    #[serde(default)]
    relation: String,
}

/// Run Graphify over the checkout, parse its graph, and submit it to the control plane.
/// Returns `(nodes, edges)` submitted. Best-effort: a Graphify failure is an error the caller can
/// log without failing the whole task (the semantic index may still have succeeded).
pub async fn index_graph(
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
) -> anyhow::Result<(usize, usize)> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    let graph_json = run_graphify(checkout).await?;
    let parsed: GraphFile =
        serde_json::from_str(&graph_json).context("parsing graphify graph.json")?;

    let nodes: Vec<GraphNodePayload> = parsed
        .nodes
        .into_iter()
        .filter_map(|n| {
            // Keep only code nodes: `update` also emits document/heading nodes (markdown) and
            // synthetic nodes (no source_file) — neither belongs in the structural code graph.
            let source_file = n.source_file?;
            if n.file_type.as_deref() != Some("code") {
                return None;
            }
            Some(GraphNodePayload {
                node_id: n.id,
                label: n.label,
                source_file,
                start_line: parse_line(n.source_location.as_deref()),
            })
        })
        .collect();

    // Drop edges that touch a non-code node (e.g. a `contains` from a markdown file to its heading).
    let code_ids: std::collections::HashSet<&str> =
        nodes.iter().map(|n| n.node_id.as_str()).collect();
    let edges: Vec<GraphEdgePayload> = parsed
        .edges
        .into_iter()
        .filter(|e| code_ids.contains(e.source.as_str()) && code_ids.contains(e.target.as_str()))
        .map(|e| GraphEdgePayload {
            source: e.source,
            target: e.target,
            relation: e.relation,
        })
        .collect();

    if nodes.is_empty() {
        tracing::info!("graphify produced no code nodes; skipping graph submit");
        return Ok((0, 0));
    }

    let (n, e) = (nodes.len(), edges.len());
    client
        .submit_graph(
            context.task_id,
            GraphBatch {
                commit_sha,
                nodes,
                edges,
            },
        )
        .await
        .context("submitting structural graph")?;
    tracing::info!(nodes = n, edges = e, "structural graph submitted");
    Ok((n, e))
}

/// Spawn `graphify update <checkout> --no-cluster` (AST-only, no LLM) and return the graph.json.
///
/// Output goes to a **private dir outside the checkout** via `GRAPHIFY_OUT` (an absolute path), not
/// the repo-owned `graphify-out/`. A cloned repo that commits its own `graphify-out/graph.json` would
/// otherwise have that stale/foreign graph merged into ours, or its node count could trip Graphify's
/// shrink-guard and block our rebuild — so we never read or write the repository's artifact.
async fn run_graphify(checkout: &Path) -> anyhow::Result<String> {
    // GRAPHIFY_OUT MUST be absolute: graphify resolves its output as `watch_path / GRAPHIFY_OUT`, so a
    // *relative* value (e.g. when WORKDIR is relative for local/dev) would make graphify write under
    // the checkout while we read the sibling dir → graph silently skipped. Canonicalize the checkout
    // (it exists — we just cloned it) and hang the output dir off its parent (the workdir), outside
    // the repo and per-Job isolated. An absolute GRAPHIFY_OUT wins the join, so both sides agree.
    let checkout_abs = tokio::fs::canonicalize(checkout)
        .await
        .with_context(|| format!("canonicalizing {}", checkout.display()))?;
    let out_dir = checkout_abs
        .parent()
        .unwrap_or(&checkout_abs)
        .join("graphify-run");
    // Create it up front — graphify won't necessarily mkdir its output dir.
    tokio::fs::create_dir_all(&out_dir)
        .await
        .with_context(|| format!("creating {}", out_dir.display()))?;
    let status = tokio::process::Command::new("graphify")
        .arg("update")
        .arg(checkout)
        .arg("--no-cluster")
        .env("GRAPHIFY_OUT", &out_dir)
        // `update` is AST-only and needs no key; strip any so a stray key can't change behaviour
        // or trigger paid calls.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .status()
        .await
        .context("spawning graphify (is it on PATH in the image?)")?;

    if !status.success() {
        anyhow::bail!("graphify update exited with {status}");
    }

    // With `GRAPHIFY_OUT` set, graphify writes `<GRAPHIFY_OUT>/graph.json` directly.
    let graph_path = out_dir.join("graph.json");
    tokio::fs::read_to_string(&graph_path)
        .await
        .with_context(|| format!("reading {}", graph_path.display()))
}

/// Parse Graphify's `"L42"` location into a 1-based line; defaults to 0 when absent/garbled.
fn parse_line(loc: Option<&str>) -> i64 {
    loc.and_then(|s| s.trim_start_matches(['L', 'l']).parse::<i64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_handles_graphify_format() {
        assert_eq!(parse_line(Some("L42")), 42);
        assert_eq!(parse_line(Some("L1")), 1);
        assert_eq!(parse_line(None), 0);
        assert_eq!(parse_line(Some("garbage")), 0);
    }

    #[test]
    fn parses_a_graphify_update_graph_json() {
        // A trimmed real sample from `graphify update --no-cluster`: edges live under `links`, nodes
        // carry `file_type`, and document/heading nodes are mixed in with code.
        let json = r#"{
            "nodes": [
                {"id": "src_math", "label": "math.rs", "source_file": "src/math.rs", "source_location": "L1", "file_type": "code"},
                {"id": "src_math_add", "label": "add()", "source_file": "src/math.rs", "source_location": "L2", "file_type": "code"},
                {"id": "readme", "label": "README.md", "source_file": "README.md", "source_location": "L1", "file_type": "document"},
                {"id": "community_0", "label": "Community 0"}
            ],
            "links": [
                {"source": "src_math", "target": "src_math_add", "relation": "contains"},
                {"source": "readme", "target": "readme_h1", "relation": "contains"}
            ]
        }"#;
        let parsed: GraphFile = serde_json::from_str(json).unwrap();

        // `links` is read into `edges` via the serde alias.
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.edges[0].relation, "contains");

        // Code-only filter: drop the document node + the synthetic (no source_file) node.
        let code: std::collections::HashSet<&str> = parsed
            .nodes
            .iter()
            .filter(|n| n.source_file.is_some() && n.file_type.as_deref() == Some("code"))
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(
            code.len(),
            2,
            "two code nodes kept; document + synthetic dropped"
        );
        // The markdown `contains` edge (readme → heading) is dropped (endpoints aren't code).
        let kept = parsed
            .edges
            .iter()
            .filter(|e| code.contains(e.source.as_str()) && code.contains(e.target.as_str()))
            .count();
        assert_eq!(kept, 1, "only the code-to-code edge survives");
    }
}
