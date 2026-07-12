//! Parity-harness scaffold (ADR-0086 / #354 acceptance criteria: golden tests + a side-by-side diff
//! target). It runs `lci-codegraph` over a committed fixture repo and compares the canonicalised
//! structural graph against a committed golden JSON.
//!
//! Two roles, one growing into the other:
//!   1. **Golden (this PR):** assert the in-house graph is byte-stable against `tests/golden/*.json`.
//!      Regenerate intentionally with `UPDATE_GOLDEN=1 cargo test -p lci-codegraph --test parity`.
//!   2. **Graphify parity (a later PR):** the same canonical form is what a future test diffs against
//!      Graphify's `graph.json` (run Graphify over the *same* fixture, normalise, compare
//!      precision/recall). The comparison helpers here (`canonical_json`) are written to be reused by
//!      that diff — see `graphify_parity_placeholder`.

use std::path::PathBuf;

use lci_codegraph::{WalkOptions, walk_checkout};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-repo")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sample-repo.graph.json")
}

/// Canonical JSON for the graph: nodes/edges are already sorted+deduped by `resolve`, so a plain
/// pretty-print is stable. This is the exact shape a Graphify diff will normalise onto.
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

/// Placeholder for the language-by-language Graphify parity diff (ADR-0086 merge bar). A later PR
/// runs Graphify over the same fixture, normalises its `graph.json` into the [`canonical_json`] shape,
/// and asserts precision/recall thresholds per language. Kept as an ignored test so the intent — and
/// the reused normalisation seam — is committed now, not rediscovered later.
#[test]
#[ignore = "enabled in the Graphify cutover PR (ADR-0086): diffs canonical graph vs graphify graph.json"]
fn graphify_parity_placeholder() {}
