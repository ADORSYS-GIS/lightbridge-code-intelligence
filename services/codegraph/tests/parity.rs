//! Golden-graph harness (ADR-0086 / #354 acceptance criteria). It runs `lci-codegraph` over a
//! committed fixture repo and asserts the canonicalised structural graph is byte-stable against a
//! committed golden JSON — the regression guard for the sole (in-house) graph engine now that the
//! Python Graphify CLI has been retired.
//!
//! Regenerate the golden intentionally with `UPDATE_GOLDEN=1 cargo test -p lci-codegraph --test parity`.

use std::path::PathBuf;

use lci_codegraph::{WalkOptions, walk_checkout};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-repo")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sample-repo.graph.json")
}

/// Canonical JSON for the graph: nodes/edges are already sorted+deduped by `resolve`, so a plain
/// pretty-print is stable.
fn canonical_json(out: &lci_codegraph::WalkOutput) -> String {
    serde_json::to_string_pretty(&out.graph).expect("graph serialises")
}

#[test]
fn graph_matches_committed_golden() {
    let options = WalkOptions::builder().build_graph(true).build();
    let out = walk_checkout(&fixture_root(), &options).expect("walk fixture");
    let actual = canonical_json(&out);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        std::fs::write(golden_path(), format!("{actual}\n")).unwrap();
        eprintln!("golden updated at {}", golden_path().display());
        return;
    }

    let expected = std::fs::read_to_string(golden_path()).unwrap_or_else(|e| {
        panic!(
            "read golden {} ({e}); regenerate with UPDATE_GOLDEN=1",
            golden_path().display()
        )
    });
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "structural graph drifted from the committed golden; if intended, regenerate with \
         UPDATE_GOLDEN=1 cargo test -p lci-codegraph --test parity"
    );
}

#[test]
fn fixture_exercises_cross_file_resolution() {
    // The whole point of the parity target (ADR-0086 R1/R5): a reference in one file resolving to a
    // definition in another. Assert the golden actually contains such an edge so a future refactor
    // can't quietly reduce the harness to intra-file-only.
    let options = WalkOptions::builder().build_graph(true).build();
    let out = walk_checkout(&fixture_root(), &options).expect("walk fixture");

    let cross_file = out.graph.edges.iter().any(|e| {
        if e.relation != "calls" {
            return false;
        }
        let src_file = out
            .graph
            .nodes
            .iter()
            .find(|n| n.node_id == e.source)
            .map(|n| &n.source_file);
        let dst_file = out
            .graph
            .nodes
            .iter()
            .find(|n| n.node_id == e.target)
            .map(|n| &n.source_file);
        matches!((src_file, dst_file), (Some(a), Some(b)) if a != b)
    });
    assert!(
        cross_file,
        "fixture must exercise cross-file call resolution; edges = {:?}",
        out.graph.edges
    );
}
