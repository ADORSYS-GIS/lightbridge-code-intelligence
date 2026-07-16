// Mock mediated Lightbridge MCP (stdio) for the RFC-0009 loop simulation.
//
// Stubs the mediated tools the agent calls — `submit_findings` (terminal), `refute_finding` (a gate
// precondition), and `search` (retrieval) — each returning a distinct MARKER the mock provider keys
// off to script the next turn, and appending every call to LCI_SIM_MCP_LOG. Newline-delimited
// JSON-RPC on stdin/stdout; Node built-ins only.

import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";

const logPath = process.env.LCI_SIM_MCP_LOG ?? "/tmp/sim-mcp.log";
const record = (obj) => {
  try {
    appendFileSync(logPath, `${JSON.stringify({ ts: new Date().toISOString(), ...obj })}\n`);
  } catch {}
};

const TOOLS = [
  {
    name: "submit_findings",
    description: "Terminal tool: submit the final review findings. Call once, at the end.",
    inputSchema: {
      type: "object",
      properties: { summary: { type: "string" }, findings: { type: "array" } },
      required: ["summary"],
    },
  },
  {
    name: "refute_finding",
    description: "Attempt to refute a P0/P1 finding before submitting (gate precondition).",
    inputSchema: {
      type: "object",
      properties: { finding: { type: "string" }, conclusion: { type: "string" } },
      required: ["finding"],
    },
  },
  {
    name: "search",
    description: "Semantic/graph retrieval over the indexed repo.",
    inputSchema: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
  },
];

const reply = (id, result) =>
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);

function callTool(name, args) {
  record({ event: "tool_call", name, args });
  switch (name) {
    case "submit_findings":
      return "SUBMIT_OK: findings accepted by the mediated control plane (stub).";
    case "refute_finding":
      return "REFUTE_OK: refutation recorded; the finding stands (stub).";
    case "search":
      return "SEARCH_OK: 0 results (stub retrieval).";
    default:
      return `UNKNOWN_TOOL: ${name}`;
  }
}

createInterface({ input: process.stdin }).on("line", (line) => {
  if (!line.trim()) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  if (req.id === undefined) return; // notification
  switch (req.method) {
    case "initialize":
      reply(req.id, {
        protocolVersion: req.params?.protocolVersion ?? "2025-03-26",
        capabilities: { tools: {} },
        serverInfo: { name: "lci-sim-mcp", version: "0.0.0" },
      });
      break;
    case "tools/list":
      reply(req.id, { tools: TOOLS });
      break;
    case "tools/call": {
      const text = callTool(req.params?.name, req.params?.arguments ?? {});
      reply(req.id, { content: [{ type: "text", text }], isError: false });
      break;
    }
    case "ping":
      reply(req.id, {});
      break;
    default:
      process.stdout.write(
        `${JSON.stringify({ jsonrpc: "2.0", id: req.id, error: { code: -32601, message: `method not found: ${req.method}` } })}\n`,
      );
  }
});
