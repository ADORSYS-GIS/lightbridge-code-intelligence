/**
 * RFC-0009 Phase-0 fidelity probe — a scripted ACP client driving a pinned OpenCode.
 *
 * Automates checklist items (a)–(c) and dumps raw evidence for the manual items:
 *   (a) client-passed `session/new.mcpServers` honored → probe MCP server's marker file written
 *   (b) tool-call fidelity → tool_call/tool_call_update updates carry rawInput/rawOutput
 *   (c) reasoning visible → agent_thought_chunk updates observed
 * Every inbound message is appended to probe-output.jsonl for items (d)–(f) and postmortems.
 *
 * Usage: OPENCODE_BIN=opencode pnpm --filter @lightbridge/opencode-probe probe [targetDir]
 */

import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

type JsonRpcMessage = {
  jsonrpc: "2.0";
  id?: number | string;
  method?: string;
  params?: Record<string, unknown>;
  result?: Record<string, unknown>;
  error?: { code: number; message: string };
};

const opencodeBin = process.env.OPENCODE_BIN ?? "opencode";
const targetDir = resolve(process.argv[2] ?? process.cwd());
const timeoutMs = Number(process.env.LCI_PROBE_TIMEOUT_MS ?? "180000");

const scratch = mkdtempSync(join(tmpdir(), "lci-opencode-probe-"));
const markerPath = join(scratch, "probe-mcp-marker.json");
const evidencePath = join(process.cwd(), "probe-output.jsonl");
const mcpServerScript = fileURLToPath(new URL("./probe-mcp-server.ts", import.meta.url));

rmSync(evidencePath, { force: true });
const logEvidence = (direction: "in" | "out", message: unknown): void => {
  appendFileSync(
    evidencePath,
    `${JSON.stringify({ ts: new Date().toISOString(), direction, message })}\n`,
  );
};

const child = spawn(opencodeBin, ["acp"], {
  cwd: targetDir,
  stdio: ["pipe", "pipe", "inherit"],
});

let nextId = 1;
const pending = new Map<number | string, (message: JsonRpcMessage) => void>();

function send(message: JsonRpcMessage): void {
  logEvidence("out", message);
  child.stdin.write(`${JSON.stringify(message)}\n`);
}

function request(method: string, params: Record<string, unknown>): Promise<JsonRpcMessage> {
  const id = nextId++;
  return new Promise((resolvePromise, rejectPromise) => {
    pending.set(id, (message) => {
      if (message.error) rejectPromise(new Error(`${method}: ${message.error.message}`));
      else resolvePromise(message);
    });
    send({ jsonrpc: "2.0", id, method, params });
  });
}

// Evidence accumulators for the automated checklist items.
const seen = {
  thoughtChunks: 0,
  toolCallUpdates: 0,
  toolCallsWithRawInput: 0,
  toolCallsWithRawOutput: 0,
  probeEchoToolCalls: 0,
  permissionRequests: 0,
};

function inspectUpdate(update: Record<string, unknown>): void {
  const kind = update.sessionUpdate as string | undefined;
  if (kind === "agent_thought_chunk") seen.thoughtChunks++;
  if (kind === "tool_call" || kind === "tool_call_update") {
    seen.toolCallUpdates++;
    if (update.rawInput !== undefined) seen.toolCallsWithRawInput++;
    if (update.rawOutput !== undefined) seen.toolCallsWithRawOutput++;
    const title = JSON.stringify(update);
    if (title.includes("probe_echo")) seen.probeEchoToolCalls++;
  }
}

const rl = createInterface({ input: child.stdout });
rl.on("line", (line) => {
  if (line.trim().length === 0) return;
  let message: JsonRpcMessage;
  try {
    message = JSON.parse(line) as JsonRpcMessage;
  } catch {
    logEvidence("in", { unparseable: line });
    return;
  }
  logEvidence("in", message);

  if (message.id !== undefined && message.method === undefined) {
    pending.get(message.id)?.(message);
    pending.delete(message.id);
    return;
  }
  if (message.method === "session/update") {
    const update = message.params?.update as Record<string, unknown> | undefined;
    if (update) inspectUpdate(update);
    return;
  }
  if (message.method === "session/request_permission" && message.id !== undefined) {
    // Answer like the agent-plane supervisor would: pick the first allow-ish option.
    seen.permissionRequests++;
    const options = (message.params?.options as { optionId: string; kind?: string }[]) ?? [];
    const allow = options.find((option) => option.kind?.startsWith("allow")) ?? options[0];
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: allow
        ? { outcome: { outcome: "selected", optionId: allow.optionId } }
        : { outcome: { outcome: "cancelled" } },
    });
  }
});

function verdict(label: string, pass: boolean | undefined, detail: string): void {
  const status = pass === undefined ? "UNKNOWN" : pass ? "PASS" : "FAIL";
  console.log(`  [${status}] ${label} — ${detail}`);
}

async function main(): Promise<void> {
  const timer = setTimeout(() => {
    console.error(`probe timed out after ${timeoutMs}ms`);
    child.kill();
    process.exit(2);
  }, timeoutMs);

  await request("initialize", {
    protocolVersion: 1,
    clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
  });

  // ⚠️ opencode 1.0.196's initialize advertises mcpCapabilities {http, sse} — NOT stdio (probe run
  // 2026-07-16, see ../README.md). A stdio mcpServers entry over ACP is therefore expected to be
  // ignored, so item (a) as written is INCONCLUSIVE, not a real FAIL. To make (a) conclusive, stand
  // up an HTTP MCP server and pass it here as { name, url } — the production shape
  // (config/opencode.json already uses type:remote http). Kept stdio here to also detect if a future
  // opencode gains stdio-over-ACP support.
  const session = await request("session/new", {
    cwd: targetDir,
    mcpServers: [
      {
        name: "lci-probe",
        command: process.execPath,
        args: ["--experimental-strip-types", mcpServerScript],
        env: [{ name: "LCI_PROBE_MARKER", value: markerPath }],
      },
    ],
  });
  const sessionId = session.result?.sessionId as string;

  const prompt = await request("session/prompt", {
    sessionId,
    prompt: [
      {
        type: "text",
        text:
          "This is an automated integration probe; follow it literally. " +
          'First, call the MCP tool `probe_echo` with {"message": "right-bytes"}. ' +
          "Second, read one small file from this repository. " +
          "Third, briefly reason step by step about what you just did. " +
          "Finally reply with exactly: PROBE DONE",
      },
    ],
  });

  clearTimeout(timer);
  child.kill();

  console.log(`\nRFC-0009 probe against '${opencodeBin} acp' in ${targetDir}`);
  console.log(`stopReason: ${String(prompt.result?.stopReason)}\n`);
  verdict(
    "(a) client-passed mcpServers honored",
    existsSync(markerPath) || seen.probeEchoToolCalls > 0,
    existsSync(markerPath)
      ? `marker written by probe MCP server (${markerPath})`
      : `${seen.probeEchoToolCalls} probe_echo tool_call updates, no marker`,
  );
  verdict(
    "(b) tool-call fidelity over ACP",
    seen.toolCallUpdates === 0
      ? undefined
      : seen.toolCallsWithRawInput > 0 && seen.toolCallsWithRawOutput > 0,
    `${seen.toolCallUpdates} tool-call updates; rawInput on ${seen.toolCallsWithRawInput}, rawOutput on ${seen.toolCallsWithRawOutput} (recorder plugin remains the completeness authority — item (f))`,
  );
  verdict(
    "(c) reasoning visible as thought chunks",
    seen.thoughtChunks > 0 ? true : undefined,
    `${seen.thoughtChunks} agent_thought_chunk updates (if 0: check the model/provider reasoning config before calling this a FAIL)`,
  );
  console.log(
    `\n  permission requests answered: ${seen.permissionRequests}` +
      `\n  full wire evidence: ${evidencePath}` +
      "\n  items (d)-(f) are plugin-side: run a session with the recorder + gate-interlock plugins loaded and compare (see README).",
  );
}

main().catch((error) => {
  console.error("probe failed:", error);
  child.kill();
  process.exit(1);
});
