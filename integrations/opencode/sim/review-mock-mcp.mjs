// Mock mediated Lightbridge REVIEW MCP (stdio) for the RFC-0009 review-host e2e proof.
//
// Stubs the review tools the agent calls — read_file, add_review_comment (returns the EXACT
// "recorded finding at …" message the refute gate keys on), retract_finding, and finish — each
// returning a marker the mock provider scripts the next turn off. Newline-delimited JSON-RPC on
// stdin/stdout; Node built-ins only. Names match the native review tools so the Rust host's
// normalize_tool_name maps them (opencode prefixes them `lightbridge_`).

import { createInterface } from "node:readline";

const TOOLS = [
  {
    name: "read_file",
    description: "Read a changed file from the checkout (mediated).",
    inputSchema: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
  },
  {
    name: "add_review_comment",
    description: "Record an inline review finding.",
    inputSchema: {
      type: "object",
      properties: {
        file: { type: "string" },
        line: { type: "number" },
        priority: { type: "string" },
        title: { type: "string" },
        body: { type: "string" },
        evidence: { type: "string" },
      },
      required: ["file", "line", "priority", "title", "body"],
    },
  },
  {
    name: "retract_finding",
    description: "Retract a previously recorded finding.",
    inputSchema: {
      type: "object",
      properties: { file: { type: "string" }, line: { type: "number" } },
      required: ["file", "line"],
    },
  },
  {
    name: "finish",
    description: "Terminal: finish the review with a verdict.",
    inputSchema: {
      type: "object",
      properties: { summary: { type: "string" } },
      required: ["summary"],
    },
  },
];

const reply = (id, result) =>
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);

function callTool(name, args) {
  switch (name) {
    case "read_file":
      return `READ_OK ${args.path ?? "?"}: fn main() {}`;
    case "add_review_comment":
      // The EXACT prefix the refute gate matches on ("recorded finding …").
      return `recorded finding at ${args.file ?? "?"}:${args.line ?? 0}`;
    case "retract_finding":
      return `retracted finding at ${args.file ?? "?"}:${args.line ?? 0}`;
    case "finish":
      return "Review finished; the host will finalize.";
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
        serverInfo: { name: "lci-review-mock-mcp", version: "0.0.0" },
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
