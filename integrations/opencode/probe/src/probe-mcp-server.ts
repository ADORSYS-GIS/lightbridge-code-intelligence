/**
 * Minimal stdio MCP server for the RFC-0009 probe.
 *
 * Exposes one tool, `probe_echo`, and writes a marker file (LCI_PROBE_MARKER) the moment the tool
 * is actually called — hard evidence for probe item (a): OpenCode honored the MCP server the ACP
 * client passed at `session/new` (or the rendered config) all the way through to a live call.
 *
 * Deliberately dependency-free: newline-delimited JSON-RPC on stdin/stdout.
 */

import { writeFileSync } from "node:fs";
import { createInterface } from "node:readline";

type JsonRpcRequest = {
  jsonrpc: "2.0";
  id?: number | string;
  method: string;
  params?: Record<string, unknown>;
};

const markerPath = process.env.LCI_PROBE_MARKER;

function respond(id: number | string, result: unknown): void {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}

const probeEchoTool = {
  name: "probe_echo",
  description:
    'Probe tool: echoes its arguments back verbatim. Call it with {"message": "..."} when asked.',
  inputSchema: {
    type: "object",
    properties: { message: { type: "string" } },
    required: ["message"],
  },
};

const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
  if (line.trim().length === 0) return;
  let request: JsonRpcRequest;
  try {
    request = JSON.parse(line) as JsonRpcRequest;
  } catch {
    return;
  }
  if (request.id === undefined) return; // notifications (e.g. notifications/initialized)

  switch (request.method) {
    case "initialize":
      respond(request.id, {
        protocolVersion: (request.params?.protocolVersion as string) ?? "2025-03-26",
        capabilities: { tools: {} },
        serverInfo: { name: "lci-probe-mcp", version: "0.0.0" },
      });
      break;
    case "tools/list":
      respond(request.id, { tools: [probeEchoTool] });
      break;
    case "tools/call": {
      const args = (request.params?.arguments as Record<string, unknown>) ?? {};
      if (markerPath) {
        writeFileSync(
          markerPath,
          JSON.stringify({ calledAt: new Date().toISOString(), args }, null, 2),
        );
      }
      respond(request.id, {
        content: [{ type: "text", text: `probe_echo received: ${JSON.stringify(args)}` }],
        isError: false,
      });
      break;
    }
    case "ping":
      respond(request.id, {});
      break;
    default:
      process.stdout.write(
        `${JSON.stringify({
          jsonrpc: "2.0",
          id: request.id,
          error: { code: -32601, message: `method not found: ${request.method}` },
        })}\n`,
      );
  }
});
