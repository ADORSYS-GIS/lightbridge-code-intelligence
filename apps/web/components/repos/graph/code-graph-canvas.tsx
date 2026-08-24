"use client";

import {
  Background,
  Controls,
  type Edge,
  MiniMap,
  type Node,
  ReactFlow,
  useEdgesState,
  useNodesState,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useEffect, useMemo } from "react";
import { RELATION_STYLE, relationKind, SYMBOL_KIND_STYLE, symbolKind } from "@/lib/domain/graph";
import type { GraphResponse } from "@/lib/server/admin";
import { GRAPH_NODE_WIDTH, layoutGraph } from "./layout";

/** The `@xyflow/react` canvas: turns a `GraphResponse` into a dagre-laid-out, kind-styled,
 * relation-styled diagram. Purely a renderer — all data fetching and interaction state (which node is
 * selected, browse vs. similar mode) lives in the parent panel; this component only calls back on a
 * node click. */
export function CodeGraphCanvas({
  data,
  selectedNodeId,
  onNodeSelect,
}: {
  data: GraphResponse;
  selectedNodeId?: string;
  onNodeSelect: (nodeId: string) => void;
}) {
  const { flowNodes, flowEdges } = useMemo(() => {
    const rawNodes: Node[] = data.nodes.map((n) => {
      const kind = symbolKind(n.label);
      const style = SYMBOL_KIND_STYLE[kind];
      const selected = n.node_id === selectedNodeId;
      const borderToken = selected ? "warning" : style.token;
      return {
        id: n.node_id,
        // `kindToken` rides along on `data` purely for the MiniMap's `nodeColor` callback below — the
        // MiniMap paints its own small swatch per node and doesn't reliably resolve a `color-mix(...)`
        // background the way the real DOM does, so it needs a plain `var(--color-token)` it can use
        // directly instead of re-deriving the kind from label shape a second time.
        data: { label: n.label, kindToken: style.token },
        position: { x: 0, y: 0 },
        // Inline styles, not a Tailwind className: `@xyflow/react`'s own stylesheet
        // (`react-flow__node-default`) ships a `background: white` rule that otherwise wins the
        // cascade over a utility class regardless of specificity tricks — inline styles are the only
        // reliable way to override it. `color-mix` tints the daisyUI token against the surface color
        // (`base-100`) instead of a flat opacity, so the text color (also a daisyUI token) reads
        // correctly against it in both light and dark themes — a plain `<color>/15` alpha cut had near-
        // white text landing on a near-white background in dark mode, which is what made every label
        // unreadable before this fix.
        style: {
          width: GRAPH_NODE_WIDTH,
          borderRadius: 8,
          border: `2px solid var(--color-${borderToken})`,
          background: `color-mix(in oklch, var(--color-${style.token}) 18%, var(--color-base-100))`,
          color: "var(--color-base-content)",
          fontSize: 12,
          fontFamily: "var(--font-mono, monospace)",
          padding: "6px 10px",
        },
        title: `${n.source_file}:${n.start_line}`,
      };
    });

    const rawEdges: Edge[] = data.edges.map((e, i) => {
      const kind = relationKind(e.relation);
      const style = RELATION_STYLE[kind];
      // `contains` is structural scaffolding, not the interesting signal — with dozens of edges on
      // screen at once it was drowning out `calls`/`method` in a dense hairball. Rendered thin,
      // translucent, and arrowless so it recedes into the background instead of dominating the view.
      const isStructural = kind === "contains";
      return {
        id: `${e.source}->${e.target}-${i}`,
        source: e.source,
        target: e.target,
        // No per-edge text label: with more than a handful of edges, a label box on every single one
        // is its own source of clutter. The legend already explains what each color means.
        style: {
          stroke: style.stroke,
          strokeWidth: isStructural ? 1 : 2,
          opacity: isStructural ? 0.35 : 0.9,
        },
        markerEnd: isStructural ? undefined : { type: "arrowclosed" as const, color: style.stroke },
        animated: style.animated,
      };
    });

    return { flowNodes: layoutGraph(rawNodes, rawEdges), flowEdges: rawEdges };
  }, [data, selectedNodeId]);

  const [nodes, setNodes, onNodesChange] = useNodesState(flowNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(flowEdges);

  useEffect(() => {
    setNodes(flowNodes);
    setEdges(flowEdges);
  }, [flowNodes, flowEdges, setNodes, setEdges]);

  return (
    <div className="h-[600px] w-full overflow-hidden rounded-lg border border-base-content/15">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={(_, node) => onNodeSelect(node.id)}
        fitView
        proOptions={{ hideAttribution: true }}
      >
        <Background />
        <MiniMap
          pannable
          zoomable
          className="!bg-base-200"
          nodeColor={(node) =>
            `var(--color-${(node.data as { kindToken?: string }).kindToken ?? "primary"})`
          }
        />
        <Controls />
      </ReactFlow>
    </div>
  );
}
