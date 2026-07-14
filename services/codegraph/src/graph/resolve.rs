//! The cross-file name resolver: turns every file's recorded call sites into `calls` edges and returns
//! the canonicalised whole-repo [`Graph`]. Resolution policy (precision-favouring, ADR-0086 R5):
//! same-file definitions win; otherwise the global table is consulted. In **both** tables a call
//! resolves only to a **single** matching definition — a bare name matching several same-named defs is
//! **dropped and counted, not fanned out** to every candidate ([`pick`]); a path qualifier (`A::new`)
//! is used solely as a tiebreaker to single out the right one when there is genuine ambiguity.

use std::collections::HashMap;

use super::{Callable, FileSymbols, Graph, GraphEdge, GraphNode};

/// Outcome of resolving one call against a candidate set.
enum Pick<'a> {
    /// Exactly one definition matched — emit the edge.
    One(&'a str),
    /// Several same-named defs and no qualifier singles one out — drop and count (never fan out).
    Ambiguous,
    /// Nothing matched here — try the next table (or count as unresolved).
    None,
}

/// Resolve a call against `candidates` (all defs sharing the callee's bare name). A single candidate
/// resolves unless a **type** qualifier positively contradicts it (a *module* qualifier — the
/// candidate has no type scope — still matches, so `math::add` resolves to a free `add`). Several
/// candidates are [`Pick::Ambiguous`] unless the qualifier singles exactly one out — we never emit an
/// edge to every same-named def (the same-file fan-out bug).
fn pick<'a>(candidates: Option<&[&'a Callable]>, qualifier: Option<&str>) -> Pick<'a> {
    let candidates = match candidates {
        Some(c) if !c.is_empty() => c,
        _ => return Pick::None,
    };
    if let [only] = candidates {
        if let (Some(q), Some(scope)) = (qualifier, only.scope.as_deref())
            && q != scope
        {
            return Pick::None;
        }
        return Pick::One(only.node_id.as_str());
    }
    // Genuine same-name ambiguity: a type qualifier is the only thing that can break it.
    if let Some(q) = qualifier {
        let mut narrowed = candidates.iter().filter(|c| c.scope.as_deref() == Some(q));
        if let Some(first) = narrowed.next()
            && narrowed.next().is_none()
        {
            return Pick::One(first.node_id.as_str());
        }
    }
    Pick::Ambiguous
}

/// Resolve every file's call sites into `calls` edges with cross-file resolution, and return the
/// canonicalised whole-repo [`Graph`].
#[must_use]
pub fn resolve(files: Vec<FileSymbols>) -> Graph {
    // Global callable table: name → all callables across files (for cross-file resolution).
    let mut global: HashMap<&str, Vec<&Callable>> = HashMap::new();
    for f in &files {
        for c in &f.callables {
            global.entry(c.name.as_str()).or_default().push(c);
        }
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut ambiguous = 0usize;
    let mut unresolved = 0usize;

    for f in &files {
        nodes.extend(f.nodes.iter().cloned());
        edges.extend(f.contains.iter().cloned());

        // Per-file callable table for same-file-first resolution.
        let mut local: HashMap<&str, Vec<&Callable>> = HashMap::new();
        for c in &f.callables {
            local.entry(c.name.as_str()).or_default().push(c);
        }

        for call in &f.calls {
            let qualifier = call.qualifier.as_deref();
            let local_pick = pick(local.get(call.name.as_str()).map(Vec::as_slice), qualifier);
            // Same-file definitions win on a clean single hit. Otherwise — no local match, OR a local
            // set too ambiguous to attribute — consult the global table, where a qualifier may still
            // single out exactly one definition (e.g. locals `A::new`+`B::new` don't shadow a global
            // `C::new()` call). A local ambiguity is only *counted* ambiguous if the global fails too.
            let local_ambiguous = matches!(local_pick, Pick::Ambiguous);
            let target = match local_pick {
                Pick::One(id) => Some(id.to_string()),
                Pick::None | Pick::Ambiguous => {
                    match pick(global.get(call.name.as_str()).map(Vec::as_slice), qualifier) {
                        Pick::One(id) => Some(id.to_string()),
                        Pick::Ambiguous => {
                            ambiguous += 1;
                            None
                        }
                        Pick::None => {
                            if local_ambiguous {
                                ambiguous += 1;
                            } else {
                                unresolved += 1;
                            }
                            None
                        }
                    }
                }
            };
            if let Some(target) = target {
                edges.push(GraphEdge {
                    source: call.caller.clone(),
                    target,
                    relation: "calls".to_string(),
                });
            }
        }
    }

    // Canonicalise: sort + dedup so output is deterministic (stable goldens, stable submit).
    nodes.sort();
    nodes.dedup();
    edges.sort();
    edges.dedup();

    tracing::debug!(
        nodes = nodes.len(),
        edges = edges.len(),
        ambiguous_calls = ambiguous,
        unresolved_calls = unresolved,
        "codegraph: resolved structural graph"
    );

    Graph { nodes, edges }
}
