"use client";

import { Button } from "@/components/ui/button";
import type { GraphSymbol } from "@/lib/server/admin";

/** A query against the selected node (e.g. "find similar") came back with a reason rather than a
 * result — rendered as one short line right in this box, not by blanking the graph. */
export interface InspectorNotice {
  tone: "muted" | "error";
  message: string;
}

/** Side panel for the currently-selected symbol: identity, the two actions that move the graph
 * view — expand its structural neighborhood, or find symbols like it by meaning — and, if the last
 * action didn't produce a result, one brief line saying why, right here instead of replacing the
 * whole graph view. */
export function NodeInspector({
  node,
  onExpand,
  onFindSimilar,
  similarActive,
  notice,
}: {
  node: GraphSymbol | null;
  onExpand: () => void;
  onFindSimilar: () => void;
  similarActive: boolean;
  notice?: InspectorNotice | null;
}) {
  if (!node) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-sm text-base-content/60">
        Click a node to see its details and explore from there.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col gap-3 px-4 py-3">
      <div>
        <p className="font-mono text-sm font-medium break-all">{node.label}</p>
        <p className="mt-1 text-xs text-base-content/60">
          {node.source_file}:{node.start_line}
        </p>
      </div>
      <div className="flex flex-col gap-2">
        <Button size="sm" variant="outline" onClick={onExpand}>
          Expand neighborhood
        </Button>
        <Button size="sm" variant={similarActive ? "primary" : "outline"} onClick={onFindSimilar}>
          Find similar (by meaning)
        </Button>
      </div>
      {notice && (
        <p className={`text-xs ${notice.tone === "error" ? "text-error" : "text-base-content/60"}`}>
          {notice.message}
        </p>
      )}
    </div>
  );
}
