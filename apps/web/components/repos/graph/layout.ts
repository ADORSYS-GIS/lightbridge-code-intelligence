import dagre from "@dagrejs/dagre";
import type { Edge, Node } from "@xyflow/react";

const NODE_WIDTH = 200;
const NODE_HEIGHT = 44;

/** Lay out `nodes`/`edges` into a top-down hierarchy via dagre — a code graph reads as a hierarchy
 * (callers above callees, containers above what they contain), not as a random force-directed cloud.
 * Runs once per data change, not continuously (dagre is a one-shot layout, not a physics simulation). */
export function layoutGraph(nodes: Node[], edges: Edge[]): Node[] {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "TB", nodesep: 40, ranksep: 80 });

  for (const node of nodes) {
    g.setNode(node.id, { width: NODE_WIDTH, height: NODE_HEIGHT });
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  return nodes.map((node) => {
    const pos = g.node(node.id);
    return {
      ...node,
      position: pos ? { x: pos.x - NODE_WIDTH / 2, y: pos.y - NODE_HEIGHT / 2 } : { x: 0, y: 0 },
    };
  });
}

export const GRAPH_NODE_WIDTH = NODE_WIDTH;
