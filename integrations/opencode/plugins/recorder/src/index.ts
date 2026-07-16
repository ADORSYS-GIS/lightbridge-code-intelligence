import { appendFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import type { Plugin } from "@opencode-ai/plugin";

/**
 * Right-bytes transcript recorder (ADR-0095).
 *
 * Appends one JSONL line per observed event to LCI_RECORDER_PATH. Runs in-process, so it sees
 * every tool call — including subagent-internal ones an ACP client may never be shown. The
 * agent-plane supervisor ships the file to the transcript store (ADR-0034) at task end.
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

  return {
    "tool.execute.before": async (input, output) => {
      record({
        kind: "tool.before",
        tool: input.tool,
        callID: input.callID,
        sessionID: input.sessionID,
        args: output.args,
      });
    },
    "tool.execute.after": async (input, output) => {
      record({
        kind: "tool.after",
        tool: input.tool,
        callID: input.callID,
        sessionID: input.sessionID,
        title: output.title,
        output: output.output,
        metadata: output.metadata,
      });
    },
    event: async ({ event }) => {
      // Reasoning capture (the ADR-0060 requirement) plus session lifecycle markers. Everything
      // else on the bus is intentionally not persisted — tool fidelity comes from the hooks above.
      if (event.type === "message.part.updated") {
        const part = (event.properties as { part?: { type?: string } } | undefined)?.part;
        if (part?.type === "reasoning") {
          record({ kind: "reasoning.part", part });
        }
        return;
      }
      if (event.type === "session.idle" || event.type === "session.error") {
        record({ kind: event.type, properties: event.properties });
      }
    },
  };
};
