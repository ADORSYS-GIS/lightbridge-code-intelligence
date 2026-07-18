import assert from "node:assert/strict";
import test from "node:test";
import type { Plugin } from "@opencode-ai/plugin";
import { bounded, LoggerPlugin, resultText } from "./index.ts";

// Node's built-in test runner (`node --experimental-strip-types --test`) — no vitest/jest, matching
// the repo's zero-runtime-dep, `import type`-only plugin convention (the probe runs the same way).

type Hooks = Awaited<ReturnType<Plugin>>;
type LogLine = { level: string; message: string; [k: string]: unknown };

/**
 * Run `body` with a fresh LoggerPlugin under `env`, capturing every stderr line as parsed JSON and
 * counting stdout writes. stdout MUST stay at zero — a single stdout write corrupts the ACP JSON-RPC
 * stream. Env and the write spies are always restored.
 */
async function withLogger(
  env: Record<string, string | undefined>,
  body: (ctx: {
    hooks: Hooks;
    lines: () => LogLine[];
    stdoutWrites: () => number;
  }) => Promise<void>,
): Promise<void> {
  const saved: Record<string, string | undefined> = {};
  for (const [k, v] of Object.entries(env)) {
    saved[k] = process.env[k];
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  const realErr = process.stderr.write.bind(process.stderr);
  const realOut = process.stdout.write.bind(process.stdout);
  const captured: LogLine[] = [];
  let stdoutCount = 0;
  // Node's write() may pass an optional completion callback as the last arg; honor it so nothing hangs.
  // biome-ignore lint/suspicious/noExplicitAny: matching the Node write() overload set for a spy.
  const finish = (rest: any[]): boolean => {
    const cb = rest[rest.length - 1];
    if (typeof cb === "function") cb();
    return true;
  };
  // biome-ignore lint/suspicious/noExplicitAny: matching the Node write() overload set for a spy.
  process.stderr.write = ((chunk: any, ...rest: any[]): boolean => {
    for (const part of String(chunk).split("\n")) {
      if (part.trim() === "") continue;
      try {
        captured.push(JSON.parse(part) as LogLine);
      } catch {
        // non-JSON stderr (e.g. a last-resort console.error) — ignore for assertions
      }
    }
    return finish(rest);
  }) as typeof process.stderr.write;
  // biome-ignore lint/suspicious/noExplicitAny: matching the Node write() overload set for a spy.
  process.stdout.write = ((_chunk: any, ...rest: any[]): boolean => {
    stdoutCount += 1;
    return finish(rest);
  }) as typeof process.stdout.write;
  try {
    const hooks = await LoggerPlugin({
      project: { id: "proj-1" },
      directory: "/w/repo",
      worktree: "/w/repo",
      // biome-ignore lint/suspicious/noExplicitAny: PluginInput carries an opencode client we don't use here.
    } as any);
    await body({
      hooks,
      lines: () => captured.slice(),
      stdoutWrites: () => stdoutCount,
    });
  } finally {
    process.stderr.write = realErr;
    process.stdout.write = realOut;
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

// A `message.part.updated` bus event carrying a single part. Cast through unknown — the real Event is
// a large discriminated union we don't need to reconstruct to drive the one branch under test.
function partEvent(part: { type: string; text?: string }): {
  event: Parameters<NonNullable<Hooks["event"]>>[0]["event"];
} {
  return {
    event: { type: "message.part.updated", properties: { part } } as unknown as Parameters<
      NonNullable<Hooks["event"]>
    >[0]["event"],
  };
}

const find = (lines: LogLine[], message: string) => lines.filter((l) => l.message === message);

test("text part → an info agent.content line at info level", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "text", text: "The diff looks correct." }));
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 1);
    assert.equal(content[0]?.level, "info");
    assert.equal(content[0]?.text, "The diff looks correct.");
    assert.equal(content[0]?.chars, "The diff looks correct.".length);
  });
});

test("reasoning part → a debug agent.reasoning line, suppressed at info", async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "reasoning", text: "Let me check bounds first." }));
    const reasoning = find(lines(), "agent.reasoning");
    assert.equal(reasoning.length, 1);
    assert.equal(reasoning[0]?.level, "debug");
    assert.equal(reasoning[0]?.text, "Let me check bounds first.");
  });
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "reasoning", text: "Hidden at info." }));
    assert.equal(find(lines(), "agent.reasoning").length, 0);
  });
});

test("empty / blank text part emits no line", async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "text", text: "" }));
    await hooks.event?.(partEvent({ type: "text", text: "   \n\t " }));
    await hooks.event?.(partEvent({ type: "reasoning", text: "" }));
    assert.equal(find(lines(), "agent.content").length, 0);
    assert.equal(find(lines(), "agent.reasoning").length, 0);
  });
});

test("oversized content is truncated to the cap with a marker", async () => {
  await withLogger(
    { LCI_LOG_LEVEL: "info", LCI_LOG_CONTENT_CHARS: "10" },
    async ({ hooks, lines }) => {
      await hooks.event?.(partEvent({ type: "text", text: "0123456789ABCDEFGHIJ" }));
      const [line] = find(lines(), "agent.content");
      assert.equal(line?.text, "0123456789…[+10 chars]");
      assert.equal(line?.chars, 20);
    },
  );
});

test("tool.start carries bounded input args at debug only", async () => {
  await withLogger(
    { LCI_LOG_LEVEL: "debug", LCI_LOG_TOOL_ARGS_CHARS: "0" },
    async ({ hooks, lines }) => {
      await hooks["tool.execute.before"]?.(
        { tool: "read_file", sessionID: "s1", callID: "c1" },
        { args: { path: "a.rs", start: 1 } },
      );
      const [line] = find(lines(), "tool.start");
      assert.equal(line?.level, "debug");
      assert.equal(line?.args, JSON.stringify({ path: "a.rs", start: 1 }));
    },
  );
  // At info the whole tool.start line is below threshold — no args leak.
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks["tool.execute.before"]?.(
      { tool: "read_file", sessionID: "s1", callID: "c1" },
      { args: { path: "a.rs" } },
    );
    assert.equal(find(lines(), "tool.start").length, 0);
  });
});

test("tool.done stays info; tool.output preview is debug-only and bounded", async () => {
  await withLogger(
    { LCI_LOG_LEVEL: "debug", LCI_LOG_TOOL_OUTPUT_CHARS: "6" },
    async ({ hooks, lines }) => {
      await hooks["tool.execute.before"]?.(
        { tool: "add_review_comment", sessionID: "s1", callID: "c2" },
        { args: {} },
      );
      await hooks["tool.execute.after"]?.(
        { tool: "add_review_comment", sessionID: "s1", callID: "c2", args: {} },
        // biome-ignore lint/suspicious/noExplicitAny: MCP result shape ({content,isError}) at runtime.
        { content: [{ type: "text", text: "recorded finding at a.rs:2" }], isError: false } as any,
      );
      const done = find(lines(), "tool.done");
      assert.equal(done.length, 1);
      assert.equal(done[0]?.level, "info");
      assert.equal(done[0]?.ok, true);
      const preview = find(lines(), "tool.output");
      assert.equal(preview.length, 1);
      assert.equal(preview[0]?.level, "debug");
      assert.equal(preview[0]?.preview, "record…[+20 chars]");
    },
  );
  // At info, tool.done appears but no tool.output preview.
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks["tool.execute.after"]?.(
      { tool: "read_file", sessionID: "s1", callID: "c3", args: {} },
      // biome-ignore lint/suspicious/noExplicitAny: built-in result shape ({title,output,metadata}).
      { title: "a.rs", output: "fn main() {}", metadata: {} } as any,
    );
    assert.equal(find(lines(), "tool.done").length, 1);
    assert.equal(find(lines(), "tool.output").length, 0);
  });
});

test("NOTHING is ever written to stdout", async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines, stdoutWrites }) => {
    await hooks.event?.(partEvent({ type: "text", text: "answer" }));
    await hooks.event?.(partEvent({ type: "reasoning", text: "thinking" }));
    await hooks["tool.execute.before"]?.(
      { tool: "t", sessionID: "s", callID: "c" },
      { args: { a: 1 } },
    );
    await hooks["tool.execute.after"]?.(
      { tool: "t", sessionID: "s", callID: "c", args: {} },
      // biome-ignore lint/suspicious/noExplicitAny: minimal built-in result shape.
      { title: "t", output: "out", metadata: {} } as any,
    );
    await hooks.event?.({
      // biome-ignore lint/suspicious/noExplicitAny: minimal session lifecycle event.
      event: { type: "session.idle", properties: {} } as any,
    });
    await hooks.dispose?.();
    // Sanity: we DID emit to stderr, and NONE of it went to stdout.
    assert.ok(lines().length > 0);
    assert.equal(stdoutWrites(), 0);
  });
});

test("bounded(): blank → undefined, cap 0 → unbounded, surrogate pairs never split", () => {
  assert.equal(bounded("", 10), undefined);
  assert.equal(bounded("   ", 10), undefined);
  assert.equal(bounded(undefined, 10), undefined);
  assert.equal(bounded("keep it whole", 0), "keep it whole");
  assert.equal(bounded("short", 10), "short");
  // "😀" is a surrogate pair (2 UTF-16 units, 1 code point). Cap 1 keeps the whole emoji intact.
  assert.equal(bounded("😀😀😀", 1), "😀…[+2 chars]");
});

test("resultText(): MCP content join, built-in output/title, string, JSON fallbacks", () => {
  assert.equal(
    resultText({
      content: [
        { type: "text", text: "a" },
        { type: "text", text: "b" },
      ],
    }),
    "a\nb",
  );
  assert.equal(resultText({ output: "built-in out", title: "t" }), "built-in out");
  assert.equal(resultText({ title: "just a title" }), "just a title");
  assert.equal(resultText("bare string"), "bare string");
  assert.equal(resultText({ some: "obj" }), JSON.stringify({ some: "obj" }));
});
