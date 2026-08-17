import type { RepoGraph } from "@/lib/server/api";

/**
 * Native code-graph view (ADR-0113 / #615) — replaces the Grafana-embed approach investigated in
 * #515. Presentational only: `page.tsx` fetches the bounded neighborhood via `getRepoGraph` and
 * handles the not-yet-indexed / error states the same way it already does for every other card
 * (`ApiErrorLine` / `StatusLine`), so this component only ever sees a real graph.
 *
 * Renders a plain list for now. The rendering library (candidates: `@xyflow/react`, `sigma.js`) is a
 * follow-up decision — this fixes the data contract and the "always bounded, never the whole graph"
 * behavior first.
 */
export function RepoGraphView({ graph }: { graph: RepoGraph }) {
  const edgesBySource = new Map<string, RepoGraph["edges"]>();
  for (const edge of graph.edges) {
    const bucket = edgesBySource.get(edge.source) ?? [];
    bucket.push(edge);
    edgesBySource.set(edge.source, bucket);
  }

  return (
    <div className="flex flex-col gap-3">
      <p className="text-xs text-base-content/60">
        {graph.nodes.length} symbols, {graph.edges.length} relationships at commit{" "}
        <code className="rounded bg-base-200 px-1.5 py-0.5">{graph.commit.slice(0, 8)}</code>
      </p>
      <ul className="flex flex-col gap-1.5 text-sm">
        {graph.nodes.map((node) => {
          const outgoing = edgesBySource.get(node.node_id) ?? [];
          return (
            <li key={node.node_id} className="rounded-md border border-base-content/10 px-3 py-2">
              <div className="flex items-center justify-between gap-2">
                <code className="font-mono">{node.label}</code>
                <span className="text-xs text-base-content/50">
                  {node.source_file}:{node.start_line}
                </span>
              </div>
              {outgoing.length > 0 && (
                <p className="mt-1 text-xs text-base-content/60">
                  {outgoing.map((edge) => (
                    <span key={`${edge.source}-${edge.target}-${edge.relation}`} className="mr-2">
                      {edge.relation} → <code className="font-mono">{edge.target}</code>
                    </span>
                  ))}
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
