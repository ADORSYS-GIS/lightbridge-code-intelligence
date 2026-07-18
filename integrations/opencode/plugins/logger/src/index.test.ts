import assert from "node:assert/strict";
import test from "node:test";
import type { Plugin } from "@opencode-ai/plugin";
import { LoggerPlugin } from "./index.ts";
import { bounded, resultText } from "./text.ts";

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
// `id` defaults to a stable value so a single-part test drives one tracked part; `end: true` on
// `time` supplies the completion marker that flushes that part immediately (see the streaming model).
type PartInput = {
  type: string;
  text?: unknown;
  id?: string;
  synthetic?: boolean;
  ignored?: boolean;
  done?: boolean;
};
function partEvent(input: PartInput): {
  event: Parameters<NonNullable<Hooks["event"]>>[0]["event"];
} {
  const { done, ...rest } = input;
  const part: Record<string, unknown> = { id: "p1", ...rest };
  // A reasoning part always carries a `time`; a text part only once it completes. `done` sets `end`.
  if (done || input.type === "reasoning") {
    part.time = { start: 1, ...(done ? { end: 2 } : {}) };
  }
  return {
    event: { type: "message.part.updated", properties: { part } } as unknown as Parameters<
      NonNullable<Hooks["event"]>
    >[0]["event"],
  };
}

// A bare bus event (session lifecycle, etc.) with arbitrary properties.
function busEvent(
  type: string,
  properties: Record<string, unknown> = {},
): { event: Parameters<NonNullable<Hooks["event"]>>[0]["event"] } {
  return {
    event: { type, properties } as unknown as Parameters<NonNullable<Hooks["event"]>>[0]["event"],
  };
}

const find = (lines: LogLine[], message: string) => lines.filter((l) => l.message === message);

test("text part → an info agent.content line at info level", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "text", text: "The diff looks correct.", done: true }));
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 1);
    assert.equal(content[0]?.level, "info");
    assert.equal(content[0]?.text, "The diff looks correct.");
    assert.equal(content[0]?.chars, "The diff looks correct.".length);
  });
});

test("reasoning part → a debug agent.reasoning line, suppressed at info", async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines }) => {
    await hooks.event?.(
      partEvent({ type: "reasoning", text: "Let me check bounds first.", done: true }),
    );
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
    await hooks.event?.(partEvent({ type: "text", text: "", id: "a", done: true }));
    await hooks.event?.(partEvent({ type: "text", text: "   \n\t ", id: "b", done: true }));
    await hooks.event?.(partEvent({ type: "reasoning", text: "", id: "c", done: true }));
    assert.equal(find(lines(), "agent.content").length, 0);
    assert.equal(find(lines(), "agent.reasoning").length, 0);
  });
});

test("oversized content is truncated to the cap with a marker", async () => {
  await withLogger(
    { LCI_LOG_LEVEL: "info", LCI_LOG_CONTENT_CHARS: "10" },
    async ({ hooks, lines }) => {
      await hooks.event?.(partEvent({ type: "text", text: "0123456789ABCDEFGHIJ", done: true }));
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
  // Exactly at the cap (code points, not UTF-16 units) returns whole — no marker.
  assert.equal(bounded("😀😀😀", 3), "😀😀😀");
  // Defensive: a non-string (number/object) returns undefined rather than throwing on .trim().
  // biome-ignore lint/suspicious/noExplicitAny: exercising the defensive non-string branch.
  assert.equal(bounded(123 as any, 10), undefined);
  // biome-ignore lint/suspicious/noExplicitAny: exercising the defensive non-string branch.
  assert.equal(bounded({ a: 1 } as any, 10), undefined);
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

// --- streaming de-duplication (finding 1) ---------------------------------------------------------

test("streaming: many updates for one part id → exactly ONE agent.content with the final text", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    // Three re-fires of the same part id with growing accumulated text; only the last completes.
    await hooks.event?.(partEvent({ type: "text", id: "t1", text: "Hel" }));
    await hooks.event?.(partEvent({ type: "text", id: "t1", text: "Hello wo" }));
    await hooks.event?.(partEvent({ type: "text", id: "t1", text: "Hello world", done: true }));
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 1);
    assert.equal(content[0]?.text, "Hello world");
    assert.equal(content[0]?.chars, "Hello world".length);
  });
});

test("streaming: a part with no completion marker is flushed exactly once on session.idle", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "text", id: "t1", text: "part one" }));
    await hooks.event?.(partEvent({ type: "text", id: "t1", text: "part one, final" }));
    // No `done` marker — nothing emitted yet.
    assert.equal(find(lines(), "agent.content").length, 0);
    await hooks.event?.(busEvent("session.idle"));
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 1);
    assert.equal(content[0]?.text, "part one, final");
  });
});

test("streaming: two distinct part ids in one cycle → two lines", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "text", id: "a", text: "first answer" }));
    await hooks.event?.(partEvent({ type: "text", id: "b", text: "second answer" }));
    await hooks.event?.(busEvent("session.idle"));
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 2);
    assert.deepEqual(content.map((l) => l.text).sort(), ["first answer", "second answer"]);
  });
});

test("streaming: no double-emit across completion re-fire, idle flush, and dispose", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    // "done" completes via its marker, then re-fires again in the SAME cycle (emittedParts guards it
    // from re-emitting). "open" never completes and rides the idle flush.
    await hooks.event?.(partEvent({ type: "text", id: "done", text: "completed", done: true }));
    await hooks.event?.(partEvent({ type: "text", id: "done", text: "completed", done: true }));
    await hooks.event?.(partEvent({ type: "text", id: "open", text: "still open" }));
    await hooks.event?.(busEvent("session.idle")); // flushes "open" once
    await hooks.dispose?.(); // pending already drained — nothing to re-emit
    const content = find(lines(), "agent.content");
    assert.equal(content.length, 2);
    assert.deepEqual(content.map((l) => l.text).sort(), ["completed", "still open"]);
  });
});

// --- robustness & correctness fixes ---------------------------------------------------------------

test("LCI_LOG_LEVEL set to a prototype key (toString) falls back to the info threshold", async () => {
  await withLogger({ LCI_LOG_LEVEL: "toString" }, async ({ hooks, lines }) => {
    // At the info threshold: content (info) is emitted, reasoning (debug) is suppressed. Under the
    // old `in` check `threshold` would be a function and every level comparison would misbehave.
    await hooks.event?.(partEvent({ type: "text", id: "t", text: "visible", done: true }));
    await hooks.event?.(partEvent({ type: "reasoning", id: "r", text: "hidden", done: true }));
    assert.equal(find(lines(), "agent.content").length, 1);
    assert.equal(find(lines(), "agent.reasoning").length, 0);
  });
});

test('boundedArgs: a no-arg tool logs no `args` field (never the string "undefined")', async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines }) => {
    await hooks["tool.execute.before"]?.(
      { tool: "list_dir", sessionID: "s1", callID: "c1" },
      // biome-ignore lint/suspicious/noExplicitAny: a tool invoked with no args.
      { args: undefined } as any,
    );
    const [line] = find(lines(), "tool.start");
    assert.ok(line);
    assert.equal(line?.args, undefined);
    assert.ok(!("args" in (line ?? {})));
  });
});

test("synthetic / ignored text parts are never logged as agent.content", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(
      partEvent({ type: "text", id: "s", text: "injected", synthetic: true, done: true }),
    );
    await hooks.event?.(
      partEvent({ type: "text", id: "i", text: "compacted", ignored: true, done: true }),
    );
    await hooks.event?.(busEvent("session.idle"));
    assert.equal(find(lines(), "agent.content").length, 0);
  });
});

// --- unknown part.type visibility (#463 F4, #411 silent-drop shape) ------------------------------

test("unknown part.type → exactly one debug agent.part.unknown line; known types emit none", async () => {
  await withLogger({ LCI_LOG_LEVEL: "debug" }, async ({ hooks, lines }) => {
    // A future/renamed shape (e.g. `reasoning-delta`) re-fires per streaming delta with the SAME id;
    // it must surface EXACTLY ONCE (on first sighting — an unknown shape has no known completion
    // marker, so we can't wait), carrying the unrecognized type + a bounded length — not the text.
    await hooks.event?.(partEvent({ type: "reasoning-delta", id: "u1", text: "chain of" }));
    await hooks.event?.(partEvent({ type: "reasoning-delta", id: "u1", text: "chain of thought" }));
    // Known types (text/reasoning) must NOT be reported as unknown.
    await hooks.event?.(partEvent({ type: "text", id: "t", text: "answer", done: true }));
    await hooks.event?.(partEvent({ type: "reasoning", id: "r", text: "thinking", done: true }));

    const unknown = find(lines(), "agent.part.unknown");
    assert.equal(unknown.length, 1);
    assert.equal(unknown[0]?.level, "debug");
    assert.equal(unknown[0]?.partType, "reasoning-delta");
    // Length is the first-sighting snapshot ("chain of"), a magnitude signal, not the final text.
    assert.equal(unknown[0]?.chars, "chain of".length);
    // The raw text is never placed on the line — only the type + a length.
    assert.ok(!("text" in (unknown[0] ?? {})));
    // The known parts still logged on their own channels, not as unknown.
    assert.equal(find(lines(), "agent.content").length, 1);
    assert.equal(find(lines(), "agent.reasoning").length, 1);
  });
});

test("unknown part.type is suppressed below debug (info level)", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    await hooks.event?.(partEvent({ type: "reasoning-delta", id: "u1", text: "hidden at info" }));
    assert.equal(find(lines(), "agent.part.unknown").length, 0);
  });
});

test("a non-string part.text neither throws into the loop nor emits a line", async () => {
  await withLogger({ LCI_LOG_LEVEL: "info" }, async ({ hooks, lines }) => {
    // Malformed wire shape: `text` is a number. The guard + defensive bounded() must swallow it.
    await hooks.event?.(partEvent({ type: "text", id: "bad", text: 42, done: true }));
    await hooks.dispose?.();
    assert.equal(find(lines(), "agent.content").length, 0);
  });
});
