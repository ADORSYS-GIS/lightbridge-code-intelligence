import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import type { Plugin } from "@opencode-ai/plugin";

// Node's built-in test runner (`node --experimental-strip-types --test`) — no vitest/jest, matching
// this repo's zero-runtime-dep, `import type`-only plugin convention (see the logger plugin's tests).

type Hooks = Awaited<ReturnType<Plugin>>;
type ProcessListener = (...args: unknown[]) => void;

/**
 * Run `body` with a fresh SentinelPlugin loaded under `env` (a fresh dir per test, so
 * `LCI_RECORDER_PATH`/`LCI_SENTINEL_MARKER_PATH` never collide across tests). Captures every
 * `process.on(event, ...)` registration the plugin makes so a test can invoke `exit`/`uncaughtException`
 * handlers directly — actually exiting the process would kill the test runner, so this is the only way
 * to exercise those paths. Re-imports the module fresh each call (`?t=<counter>` cache-busts Node's ESM
 * module cache) since the plugin reads `LCI_RECORDER_PATH`/`LCI_SENTINEL_MARKER_PATH` at module-load
 * time, matching the recorder/logger plugins' own env-at-load-time contract.
 */
let importCounter = 0;
async function withSentinel(
  env: Record<string, string | undefined>,
  body: (ctx: {
    hooks: Hooks;
    fireProcessEvent: (event: string, ...args: unknown[]) => void;
    markerPath: string;
    recorderPath: string;
  }) => Promise<void>,
): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "lci-sentinel-test-"));
  const recorderPath = join(dir, "recording.jsonl");
  const markerPath = join(dir, "sentinel.marker.json");
  const saved: Record<string, string | undefined> = {};
  const fullEnv = { ...env, LCI_RECORDER_PATH: recorderPath, LCI_SENTINEL_MARKER_PATH: markerPath };
  for (const [k, v] of Object.entries(fullEnv)) {
    saved[k] = process.env[k];
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  const listeners = new Map<string, ProcessListener[]>();
  const realOn = process.on.bind(process);
  // biome-ignore lint/suspicious/noExplicitAny: matching Node's EventEmitter.on overload set for a spy.
  process.on = ((event: string, listener: ProcessListener): any => {
    const existing = listeners.get(event) ?? [];
    existing.push(listener);
    listeners.set(event, existing);
    return process;
  }) as typeof process.on;
  try {
    importCounter += 1;
    const module = await import(`./index.ts?t=${importCounter}`);
    const SentinelPlugin: Plugin = module.SentinelPlugin;
    const hooks = await SentinelPlugin({
      project: { id: "proj-1" },
      directory: "/w/repo",
      worktree: "/w/repo",
      // biome-ignore lint/suspicious/noExplicitAny: PluginInput carries an opencode client we don't use here.
    } as any);
    await body({
      hooks,
      fireProcessEvent: (event, ...args) => {
        for (const listener of listeners.get(event) ?? []) listener(...args);
      },
      markerPath,
      recorderPath,
    });
  } finally {
    process.on = realOn;
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    rmSync(dir, { recursive: true, force: true });
  }
}

function readMarker(path: string): Record<string, unknown> {
  return JSON.parse(readFileSync(path, "utf8"));
}

function readRecorderLines(path: string): Record<string, unknown>[] {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

test("a clean finish call marks the session terminal — no exit_without_terminal marker written", async () => {
  await withSentinel({}, async ({ hooks, fireProcessEvent, markerPath }) => {
    await hooks["tool.execute.before"]?.(
      { tool: "lightbridge_finish", sessionID: "s1", callID: "c1" } as never,
      {} as never,
    );
    await hooks["tool.execute.after"]?.(
      { tool: "lightbridge_finish", sessionID: "s1", callID: "c1" } as never,
      {} as never,
    );
    fireProcessEvent("exit");
    assert.throws(() => readMarker(markerPath), /ENOENT/);
  });
});

test("exiting without any terminal tool call writes an exit_without_terminal marker", async () => {
  await withSentinel({}, async ({ hooks, fireProcessEvent, markerPath, recorderPath }) => {
    await hooks["tool.execute.before"]?.(
      { tool: "lightbridge_read_file", sessionID: "s1", callID: "c1" } as never,
      {} as never,
    );
    fireProcessEvent("exit");
    const marker = readMarker(markerPath);
    assert.equal(marker.fatalKind, "exit_without_terminal");
    assert.equal(marker.lastToolCall, "lightbridge_read_file");
    assert.equal(marker.sessionID, "s1");
    const lines = readRecorderLines(recorderPath);
    assert.equal(lines.length, 1);
    assert.equal(lines[0]?.kind, "fatal_event");
  });
});

test("an uncaught exception writes a marker and does not throw out of the process", async () => {
  await withSentinel({}, async ({ fireProcessEvent, markerPath }) => {
    fireProcessEvent("uncaughtException", new Error("boom"));
    const marker = readMarker(markerPath);
    assert.equal(marker.fatalKind, "uncaught_exception");
    assert.match(marker.message as string, /boom/);
  });
});

test("an unhandled rejection writes a marker", async () => {
  await withSentinel({}, async ({ fireProcessEvent, markerPath }) => {
    fireProcessEvent("unhandledRejection", new Error("rejected"));
    const marker = readMarker(markerPath);
    assert.equal(marker.fatalKind, "uncaught_exception");
    assert.match(marker.message as string, /rejected/);
  });
});

test("a session.error bus event writes a provider_error marker with its message", async () => {
  await withSentinel({}, async ({ hooks, markerPath }) => {
    await hooks.event?.({
      event: {
        type: "session.error",
        properties: { message: "provider unreachable", sessionID: "s2" },
      } as never,
    });
    const marker = readMarker(markerPath);
    assert.equal(marker.fatalKind, "provider_error");
    assert.equal(marker.message, "provider unreachable");
    assert.equal(marker.sessionID, "s2");
  });
});

test("a malformed session.error event does not throw and still records something", async () => {
  await withSentinel({}, async ({ hooks, markerPath }) => {
    await hooks.event?.({ event: { type: "session.error", properties: undefined } as never });
    const marker = readMarker(markerPath);
    assert.equal(marker.fatalKind, "provider_error");
  });
});

test("only the terminal-tool names configured via LCI_SENTINEL_TERMINAL_TOOLS count as terminal", async () => {
  await withSentinel(
    { LCI_SENTINEL_TERMINAL_TOOLS: "custom_done" },
    async ({ hooks, fireProcessEvent, markerPath }) => {
      await hooks["tool.execute.after"]?.(
        { tool: "lightbridge_finish", sessionID: "s1", callID: "c1" } as never,
        {} as never,
      );
      fireProcessEvent("exit");
      // The default terminal tool name doesn't count once the env override is set — still fatal.
      const marker = readMarker(markerPath);
      assert.equal(marker.fatalKind, "exit_without_terminal");
    },
  );
});
