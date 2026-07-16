// Mock OpenAI-compatible provider for the RFC-0009 review-host e2e proof.
//
// SCRIPTS a review turn sequence against opencode's `@ai-sdk/openai-compatible` provider, self-adapting
// to the advertised tool names (substring match, so it doesn't hard-code opencode's `lightbridge_`
// prefix): read_file(a.rs) -> add_review_comment(P2 nit) -> finish -> done. That exercises the coverage
// gate (a.rs gets read, so finish is accepted) end to end through the REAL host + gates. Node built-ins
// only; serves /v1/models + /v1/chat/completions (streaming SSE + non-streaming).

import { appendFileSync } from "node:fs";
import { createServer } from "node:http";

const port = Number(process.env.LCI_SIM_PROVIDER_PORT ?? "8899");
const toolsLog = process.env.LCI_SIM_TOOLS_LOG;

function decide(messages, tools) {
  const toolNames = (tools ?? [])
    .map((t) => t?.function?.name ?? t?.name)
    .filter((n) => typeof n === "string");
  // Record every distinct tool opencode advertised, so a test can assert built-ins are disabled.
  if (toolsLog) {
    try {
      appendFileSync(toolsLog, `${JSON.stringify(toolNames)}\n`);
    } catch {}
  }
  const find = (re) => toolNames.find((n) => re.test(n));
  const readFile = find(/read_file/i);
  const addComment = find(/add_review_comment/i);
  const finish = find(/finish/i);

  const transcript = JSON.stringify(messages ?? []);
  const didRead = /READ_OK/.test(transcript);
  const didRecord = /recorded finding/.test(transcript);
  const didFinish = /will finalize/.test(transcript);

  if (didFinish) return { kind: "text", text: "Review complete. DONE." };
  if (!didRead && readFile) return { kind: "tool", name: readFile, args: { path: "a.rs" } };
  if (!didRecord && addComment)
    return {
      kind: "tool",
      name: addComment,
      args: {
        file: "a.rs",
        line: 2,
        priority: "P2",
        title: "minor nit",
        body: "a small style issue",
        evidence: "a.rs:2",
      },
    };
  if (finish) return { kind: "tool", name: finish, args: { summary: "one minor nit; looks good" } };
  return { kind: "text", text: "No review tools advertised. DONE." };
}

const chunk = (obj) => `data: ${JSON.stringify(obj)}\n\n`;

function streamResponse(res, decision) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  const base = { id: "chatcmpl-sim", object: "chat.completion.chunk", created: 0, model: "sim-model" };
  res.write(chunk({ ...base, choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }));
  if (decision.kind === "text") {
    res.write(chunk({ ...base, choices: [{ index: 0, delta: { content: decision.text }, finish_reason: null }] }));
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
  res.writeHead(200, { "content-type": "application/json" });
  res.end(
    JSON.stringify({
      id: "chatcmpl-sim",
      object: "chat.completion",
      created: 0,
      model: "sim-model",
      choices: [{ index: 0, message, finish_reason: decision.kind === "text" ? "stop" : "tool_calls" }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    }),
  );
}

const server = createServer((req, res) => {
  if (req.method === "GET" && req.url.startsWith("/v1/models")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ object: "list", data: [{ id: "sim-model", object: "model", owned_by: "sim" }] }));
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
      if (payload.stream) streamResponse(res, decision);
      else jsonResponse(res, decision);
    });
    return;
  }
  res.writeHead(404);
  res.end("not found");
});

server.listen(port, "127.0.0.1", () => {
  process.stderr.write(`[review-sim-provider] listening on http://127.0.0.1:${port}\n`);
});
