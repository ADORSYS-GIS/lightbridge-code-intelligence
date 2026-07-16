// Mock OpenAI-compatible provider for the RFC-0009 offline loop simulation.
//
// Drives opencode's `@ai-sdk/openai-compatible` provider with SCRIPTED turns — no real LLM. It is
// SELF-ADAPTING: it reads the `tools` opencode advertises in each request and picks the real tool
// names by substring, so it doesn't hard-code opencode's MCP naming. The script exercises the
// gate-interlock: submit_findings (blocked) -> refute_finding -> submit_findings (allowed) -> done.
//
// Serves /v1/models and /v1/chat/completions (streaming SSE + non-streaming). Logs every request to
// LCI_SIM_PROVIDER_LOG for inspection. Node built-ins only.

import { appendFileSync } from "node:fs";
import { createServer } from "node:http";

const port = Number(process.env.LCI_SIM_PROVIDER_PORT ?? "8899");
const logPath = process.env.LCI_SIM_PROVIDER_LOG ?? "/tmp/sim-provider.log";

const logReq = (obj) => {
  try {
    appendFileSync(logPath, `${JSON.stringify(obj)}\n`);
  } catch {}
};

// Decide the next assistant turn from the conversation so far + the advertised tools.
function decide(messages, tools) {
  const toolNames = (tools ?? [])
    .map((t) => t?.function?.name ?? t?.name)
    .filter((n) => typeof n === "string");
  const find = (re) => toolNames.find((n) => re.test(n));
  const submit = find(/submit_findings/i);
  const refute = find(/refute/i);

  const transcript = JSON.stringify(messages ?? []);
  const submitOk = /SUBMIT_OK/.test(transcript);
  const refuteOk = /REFUTE_OK/.test(transcript);
  const gateBlocked = /[Gg]ate interlock/.test(transcript);

  if (submitOk) return { kind: "text", text: "Review complete. Findings submitted. DONE." };
  if (!submit) return { kind: "text", text: "No submit_findings tool advertised; nothing to do." };
  if (refuteOk) return { kind: "tool", name: submit, args: { summary: "verified", findings: [] } };
  if (gateBlocked && refute)
    return { kind: "tool", name: refute, args: { finding: "F1", conclusion: "stands" } };
  // First move: attempt submit_findings straight away — the gate should BLOCK this.
  return {
    kind: "tool",
    name: submit,
    args: { summary: "initial", findings: [{ title: "F1", severity: "major" }] },
  };
}

const chunk = (obj) => `data: ${JSON.stringify(obj)}\n\n`;

function streamResponse(res, decision) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  const id = "chatcmpl-sim";
  const base = { id, object: "chat.completion.chunk", created: 0, model: "sim-model" };
  res.write(
    chunk({ ...base, choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }),
  );
  if (decision.kind === "text") {
    res.write(
      chunk({
        ...base,
        choices: [{ index: 0, delta: { content: decision.text }, finish_reason: null }],
      }),
    );
    res.write(chunk({ ...base, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }));
  } else {
    res.write(
      chunk({
        ...base,
        choices: [
          {
            index: 0,
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: `call_${Date.now()}`,
                  type: "function",
                  function: { name: decision.name, arguments: JSON.stringify(decision.args) },
                },
              ],
            },
            finish_reason: null,
          },
        ],
      }),
    );
    res.write(chunk({ ...base, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] }));
  }
  res.write("data: [DONE]\n\n");
  res.end();
}

function jsonResponse(res, decision) {
  const message =
    decision.kind === "text"
      ? { role: "assistant", content: decision.text }
      : {
          role: "assistant",
          content: null,
          tool_calls: [
            {
              id: `call_${Date.now()}`,
              type: "function",
              function: { name: decision.name, arguments: JSON.stringify(decision.args) },
            },
          ],
        };
  const body = JSON.stringify({
    id: "chatcmpl-sim",
    object: "chat.completion",
    created: 0,
    model: "sim-model",
    choices: [
      { index: 0, message, finish_reason: decision.kind === "text" ? "stop" : "tool_calls" },
    ],
    usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
  });
  res.writeHead(200, { "content-type": "application/json" });
  res.end(body);
}

const server = createServer((req, res) => {
  if (req.method === "GET" && req.url.startsWith("/v1/models")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        object: "list",
        data: [{ id: "sim-model", object: "model", owned_by: "sim" }],
      }),
    );
    return;
  }
  if (req.method === "POST" && req.url.includes("/chat/completions")) {
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      let payload = {};
      try {
        payload = JSON.parse(raw);
      } catch {}
      const decision = decide(payload.messages, payload.tools);
      logReq({
        ts: new Date().toISOString(),
        stream: !!payload.stream,
        nTools: (payload.tools ?? []).length,
        toolNames: (payload.tools ?? []).map((t) => t?.function?.name ?? t?.name),
        nMessages: (payload.messages ?? []).length,
        decision,
      });
      if (payload.stream) streamResponse(res, decision);
      else jsonResponse(res, decision);
    });
    return;
  }
  res.writeHead(404);
  res.end("not found");
});

server.listen(port, "127.0.0.1", () => {
  process.stderr.write(`[sim-provider] listening on http://127.0.0.1:${port} (log: ${logPath})\n`);
});
