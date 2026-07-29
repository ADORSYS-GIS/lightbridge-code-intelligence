import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import type { Plugin } from "@opencode-ai/plugin";

/**
 * Fatal-situation sentinel (ADR-0106).
 *
 * Today a fatal OpenCode session (crash, hang, an uncaught error, a silent exit) is only ever
 * INFERRED control-plane-side from a telemetry timeout — conflating "still working" with "already
 * dead." This plugin observes the session's own lifecycle directly and reports a structured cause
 * instead: a `fatal_event` line appended to the same JSONL the recorder plugin writes (ADR-0095,
 * `LCI_RECORDER_PATH`) — one more line shape, not a new file — plus a marker file
 * (`LCI_SENTINEL_MARKER_PATH`) holding the LAST fatal event observed, which the agent-runner host
 * checks after the session ends.
 *
 * Three kinds, each mapped from a distinct OpenCode-visible signal:
 *   - `provider_error`       — an OpenCode `session.error` bus event (a provider/transport failure
 *                               OpenCode itself surfaced).
 *   - `uncaught_exception`   — a Node `uncaughtException`/`unhandledRejection` in this process.
 *   - `exit_without_terminal` — the process exits having never seen a terminal tool call
 *                               (`lightbridge_finish`/`lightbridge_abort`) complete.
 *
 * `exit_without_terminal` is diagnostic ONLY, not inherently fatal: a normal budget-exhausted review
 * ALSO exits without ever calling finish/abort (the Rust-side driver just stops prompting once its
 * turn/cycle budget is spent — the model is never asked to call a terminal tool for that). The
 * plugin has no visibility into "was this the driver's own budget decision" — only the agent-runner
 * host does (it already computes `ReviewOutcome::Exhausted` for exactly this case). So this plugin
 * always records what it saw; the HOST decides what's actually alarming — see
 * `services/agent-runner/src/review/opencode.rs`'s marker-consumption comment for the split: an
 * `exit_without_terminal` marker only gets folded into a hard failure when the loop ALSO returned a
 * transport `Err` (i.e. never reached a clean Finished/Exhausted/Aborted resolution at all);
 * `provider_error`/`uncaught_exception` are logged as a warning regardless of the resolution, since
 * those are never a normal outcome even within an otherwise-successful run.
 *
 * This plugin is diagnostic only (ADR-0106) — no retry/recovery logic, and (like `recorder`) it must
 * never throw into the agent loop itself: every hook body is guarded.
 */

const recorderPath =
  process.env.LCI_RECORDER_PATH ?? join(process.cwd(), ".lightbridge", "recording.jsonl");
const markerPath =
  process.env.LCI_SENTINEL_MARKER_PATH ?? join(dirname(recorderPath), "sentinel.marker.json");
const terminalTools = (
  process.env.LCI_SENTINEL_TERMINAL_TOOLS ?? "lightbridge_finish,lightbridge_abort"
)
  .split(",")
  .map((name) => name.trim())
  .filter((name) => name.length > 0);

export type FatalKind = "provider_error" | "uncaught_exception" | "exit_without_terminal";

export type FatalEvent = {
  kind: "fatal_event";
  fatalKind: FatalKind;
  message: string;
  lastToolCall: string | null;
  sessionID: string | null;
};

function appendRecorderLine(line: Record<string, unknown>): void {
  try {
    mkdirSync(dirname(recorderPath), { recursive: true });
    appendFileSync(recorderPath, `${JSON.stringify({ ts: new Date().toISOString(), ...line })}\n`);
  } catch (error) {
    console.error(`[lci-sentinel] failed to append to ${recorderPath}:`, error);
  }
}

function writeMarker(event: FatalEvent): void {
  try {
    mkdirSync(dirname(markerPath), { recursive: true });
    writeFileSync(markerPath, JSON.stringify(event));
  } catch (error) {
    console.error(`[lci-sentinel] failed to write marker ${markerPath}:`, error);
  }
}

function record(
  fatalKind: FatalKind,
  message: string,
  lastToolCall: string | null,
  sessionID: string | null,
): void {
  const event: FatalEvent = { kind: "fatal_event", fatalKind, message, lastToolCall, sessionID };
  appendRecorderLine(event);
  writeMarker(event);
}

export const SentinelPlugin: Plugin = async () => {
  let lastToolCall: string | null = null;
  let lastSessionID: string | null = null;
  let terminalSeen = false;

  const guard = (fn: () => void): void => {
    try {
      fn();
    } catch (error) {
      console.error("[lci-sentinel] hook error (swallowed):", error);
    }
  };

  // Best-effort: a hook that throws OUT of the loop is exactly what this plugin exists to catch, so
  // its own installation must not itself be fragile. `process.on` is synchronous and never throws for
  // a well-formed listener, but wrap it anyway — consistent with every other guarded body here.
  guard(() => {
    process.on("uncaughtException", (error) => {
      record(
        "uncaught_exception",
        error instanceof Error ? (error.stack ?? error.message) : String(error),
        lastToolCall,
        lastSessionID,
      );
    });
    process.on("unhandledRejection", (reason) => {
      record(
        "uncaught_exception",
        reason instanceof Error ? (reason.stack ?? reason.message) : String(reason),
        lastToolCall,
        lastSessionID,
      );
    });
    process.on("exit", () => {
      if (!terminalSeen) {
        // Synchronous only — an `exit` handler cannot do async I/O, so this reuses the same
        // synchronous writeFileSync/appendFileSync path every other record() call already uses.
        record(
          "exit_without_terminal",
          `process exited without a terminal tool call (${terminalTools.join(", ")}) completing`,
          lastToolCall,
          lastSessionID,
        );
      }
    });
  });

  return {
    "tool.execute.before": async (input) => {
      guard(() => {
        if (input?.tool) lastToolCall = input.tool;
        if (input?.sessionID) lastSessionID = input.sessionID;
      });
    },
    "tool.execute.after": async (input) => {
      guard(() => {
        if (input?.tool && terminalTools.includes(input.tool)) {
          terminalSeen = true;
        }
      });
    },
    event: async ({ event }) => {
      guard(() => {
        if (event?.type === "session.error") {
          const properties = event.properties as
            | { message?: unknown; sessionID?: unknown }
            | undefined;
          record(
            "provider_error",
            typeof properties?.message === "string"
              ? properties.message
              : JSON.stringify(properties ?? {}),
            lastToolCall,
            typeof properties?.sessionID === "string" ? properties.sessionID : lastSessionID,
          );
        }
      });
    },
  };
};
