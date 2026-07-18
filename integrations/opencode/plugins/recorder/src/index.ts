import { appendFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import type { Plugin } from "@opencode-ai/plugin";

/**
 * Right-bytes transcript recorder (ADR-0095).
 *
 * Appends one JSONL line per observed event to LCI_RECORDER_PATH. Runs in-process, so it sees
 * every tool call — including subagent-internal ones an ACP client may never be shown. The
 * agent-plane supervisor reads the file per-cycle to drive the review quality gates (coverage
 * accounting); it is not a transcript store (the DB transcript was retired — epic #459).
 *
 * This plugin must never fail the loop: recording errors are reported on stderr and swallowed.
 */

const recorderPath =
  process.env.LCI_RECORDER_PATH ?? join(process.cwd(), ".lightbridge", "recording.jsonl");

function record(line: Record<string, unknown>): void {
  try {
    appendFileSync(recorderPath, `${JSON.stringify({ ts: new Date().toISOString(), ...line })}\n`);
  } catch (error) {
    console.error(`[lci-recorder] failed to append to ${recorderPath}:`, error);
  }
}

export const RecorderPlugin: Plugin = async () => {
  try {
    mkdirSync(dirname(recorderPath), { recursive: true });
  } catch (error) {
    console.error(`[lci-recorder] failed to create ${dirname(recorderPath)}:`, error);
  }
  record({ kind: "recorder.start", pid: process.pid });

  // A hook must NEVER throw into the agent loop (the recorder's contract). record() already guards
  // its own I/O; `guard` additionally contains any error from the hook body (e.g. unexpected field
  // shapes) so a malformed event can never take the loop down.
  const guard = (fn: () => void): void => {
    try {
      fn();
    } catch (error) {
      console.error("[lci-recorder] hook error (swallowed):", error);
    }
  };

  return {
    "tool.execute.before": async (input, output) => {
      guard(() =>
        record({
          kind: "tool.before",
          tool: input?.tool,
          callID: input?.callID,
          sessionID: input?.sessionID,
          args: output?.args,
        }),
      );
    },
    "tool.execute.after": async (input, output) => {
      // Record the FULL output verbatim (right-bytes). The plugin's TS type declares
      // {title, output, metadata}, but that only holds for BUILT-IN tools — MCP tools (the mediated
      // Lightbridge tools: submit_findings/propose_pr/…) deliver {content: [{type,text}], isError}
      // at runtime (verified against 1.18.2 via the sim). Cherry-picking the typed fields silently
      // dropped every mediated tool's RESULT; recording the whole object captures either shape.
      guard(() =>
        record({
          kind: "tool.after",
          tool: input?.tool,
          callID: input?.callID,
          sessionID: input?.sessionID,
          result: output,
        }),
      );
    },
    event: async ({ event }) => {
      // Session lifecycle markers only. Model parts (reasoning/content) are the observability
      // surface's job (Loki via the logger plugin, epic #459) — the recorder persists tool fidelity
      // for the quality gates, not the model's prose. Everything else on the bus is not recorded.
      guard(() => {
        if (event?.type === "session.idle" || event?.type === "session.error") {
          record({ kind: event.type, properties: event.properties });
        }
      });
    },
  };
};
