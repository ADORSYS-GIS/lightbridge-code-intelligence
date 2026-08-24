"use client";

import { useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { StatusLine } from "@/components/ui/states";
import { RELATION_STYLE, SYMBOL_KIND_STYLE } from "@/lib/domain/graph";
import { CodeGraphCanvas } from "./code-graph-canvas";
import { NodeInspector } from "./node-inspector";
import { type GraphMode, useCodeGraph } from "./use-code-graph";

/** Top-level composition for the repo detail page's Graph tab: canvas + legend + node inspector,
 * wired to the two-tier query design (ADR-0114 follow-up) — structural browse and "find similar by
 * meaning" via a node's own stored embedding. No free-text search here by design: every query this
 * view can issue is either pure graph traversal or reuses a value already in the database, so nothing
 * a person types can produce an unpredictable or failing search. */
export function CodeGraphPanel({ repoId }: { repoId: number }) {
  const [mode, setMode] = useState<GraphMode>({ kind: "browse", hops: 1 });
  const [selectedNodeId, setSelectedNodeId] = useState<string | undefined>(undefined);
  const { data, loading, error, reload } = useCodeGraph(repoId, mode);

  const selectedNode = useMemo(
    () => data?.nodes.find((n) => n.node_id === selectedNodeId) ?? null,
    [data, selectedNodeId],
  );

  return (
    <Card>
      <CardHeader className="flex items-center justify-between">
        <CardTitle>Code graph</CardTitle>
        <div className="flex items-center gap-2">
          {mode.kind === "similar" && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setMode({ kind: "browse", nodeId: selectedNodeId, hops: 1 })}
            >
              Back to structural view
            </Button>
          )}
          <Button size="sm" variant="ghost" onClick={reload}>
            Refresh
          </Button>
        </div>
      </CardHeader>
      <CardBody>
        {error && <StatusLine tone="error">{error}</StatusLine>}
        {!error && loading && !data && <StatusLine>Loading the graph…</StatusLine>}
        {!error && data && data.nodes.length === 0 && (
          <StatusLine>No indexed symbols yet for this repository.</StatusLine>
        )}
        {!error && data && data.nodes.length > 0 && (
          <div className="grid grid-cols-1 gap-3 lg:grid-cols-[1fr_260px]">
            <div className="flex flex-col gap-2">
              <Legend />
              <CodeGraphCanvas
                data={data}
                selectedNodeId={selectedNodeId}
                onNodeSelect={(nodeId) => setSelectedNodeId(nodeId)}
              />
            </div>
            <div className="rounded-lg border border-base-content/15 bg-base-100">
              <NodeInspector
                node={selectedNode}
                similarActive={mode.kind === "similar"}
                onExpand={() => {
                  if (!selectedNodeId) return;
                  setMode({ kind: "browse", nodeId: selectedNodeId, hops: 1 });
                }}
                onFindSimilar={() => {
                  if (!selectedNodeId) return;
                  setMode({ kind: "similar", nodeId: selectedNodeId });
                }}
              />
            </div>
          </div>
        )}
      </CardBody>
    </Card>
  );
}

function Legend() {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-base-content/70">
      {Object.values(SYMBOL_KIND_STYLE).map((s) => (
        <span key={s.label} className="flex items-center gap-1.5">
          <span
            className={`inline-block size-2.5 rounded-full ${s.bg}`}
            style={{ border: `1.5px solid var(--color-${s.token})` }}
          />
          {s.label}
        </span>
      ))}
      <span className="mx-1 text-base-content/30">|</span>
      {Object.values(RELATION_STYLE).map((s) => (
        <span key={s.label} className="flex items-center gap-1.5">
          <span className="inline-block h-0.5 w-4" style={{ background: s.stroke }} />
          {s.label}
        </span>
      ))}
    </div>
  );
}
