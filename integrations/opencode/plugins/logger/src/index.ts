import type { Plugin } from "@opencode-ai/plugin";

/**
 * Structured JSON operational logger (RFC-0009 / ADR-0095 sibling of the recorder).
 *
 * Emits winston-shaped JSON lines — `{level, message, timestamp, ...meta}` — one per line, the same
 * output a `winston` Console transport with `format.json()` produces. We deliberately DO NOT depend
 * on winston: every Lightbridge OpenCode plugin is `import type`-only so the agent image stays
 * hermetic (no node_modules at runtime — the sandbox is egress-denied and can't npm-install at
 * startup; see ../../Dockerfile). For "JSON logs to the pod log pipeline" winston's transports and
 * rotation add nothing over one JSON line, so the shape is winston-compatible and the dependency is
 * dropped. If real winston is ever wanted OUTSIDE the sandbox, this file is the single swap point.
 *
 * ⚠️ Writes to STDERR, never stdout. In embedded stdio ACP mode OpenCode's STDOUT carries the
 * JSON-RPC protocol stream the agent-plane supervisor parses; a log line on stdout would corrupt it
 * (this is why OpenCode's own `--print-logs` goes to stderr). stderr is captured by the pod → Loki
 * pipeline just the same.
 *
 * This is the OPERATIONAL log (lifecycle, coarse tool timing, errors, permission decisions) — NOT
 * the transcript. Full tool args/results + reasoning are the recorder's job (right-bytes JSONL to
 * the ADR-0034 store); the two do not overlap.
 *
 * Config (rendered per task by the supervisor):
 *   LCI_LOG_LEVEL    error | warn | info | debug   (default: info)
 *   LCI_LOG_SERVICE  service label on every line   (default: lci-opencode)
 */

type Level = "error" | "warn" | "info" | "debug";
const LEVELS: Record<Level, number> = { error: 0, warn: 1, info: 2, debug: 3 };

const envLevel = process.env.LCI_LOG_LEVEL as Level | undefined;
const threshold = envLevel && envLevel in LEVELS ? LEVELS[envLevel] : LEVELS.info;
const service = process.env.LCI_LOG_SERVICE ?? "lci-opencode";

export const LoggerPlugin: Plugin = async ({ project, directory, worktree }) => {
  const base = {
    service,
    projectID: project?.id,
    worktree,
    directory,
  };

  const log = (level: Level, message: string, meta?: Record<string, unknown>): void => {
    if (LEVELS[level] > threshold) return;
    try {
      process.stderr.write(
        `${JSON.stringify({ level, message, timestamp: new Date().toISOString(), ...base, ...meta })}\n`,
      );
    } catch (error) {
      // A logger must never take the loop down; last-resort console, then swallow.
      console.error("[lci-logger] emit failed:", error);
    }
  };

  // callID → start time, for tool durations. tool.execute.after fires per call, so this stays small.
  const started = new Map<string, number>();

  // Note: never put a `level`/`message`/`timestamp` key in meta — it would clobber the winston
  // top-level field of the same name (meta is spread last).
  log("info", "opencode plugin logger started", {
    pid: process.pid,
    configuredLevel: envLevel ?? "info",
  });

  return {
    "tool.execute.before": async (input) => {
      started.set(input.callID, Date.now());
      log("debug", "tool.start", {
        tool: input.tool,
        callID: input.callID,
        sessionID: input.sessionID,
      });
    },
    "tool.execute.after": async (input, output) => {
      const startedAt = started.get(input.callID);
      started.delete(input.callID);
      log("info", "tool.done", {
        tool: input.tool,
        callID: input.callID,
        sessionID: input.sessionID,
        durationMs: startedAt === undefined ? undefined : Date.now() - startedAt,
        // ok reflects the MCP result flag ({content,isError} at runtime for mediated tools); `title`
        // is populated for built-in tools. The full output is the recorder's territory, not this log.
        ok: (output as { isError?: boolean }).isError !== true,
        title: output.title,
      });
    },
    "permission.ask": async (input, output) => {
      // Security-relevant: what the agent asked to do and how policy answered.
      log("info", "permission.ask", {
        type: (input as { type?: string }).type,
        pattern: (input as { pattern?: string }).pattern,
        status: output.status,
      });
    },
    event: async ({ event }) => {
      const type = event.type;
      if (type === "session.error") {
        log("error", "session.error", { properties: event.properties });
      } else if (type.startsWith("session.")) {
        log("info", type, { properties: event.properties });
      }
      // Everything else on the bus (message parts, tool state) is covered by the hooks above and the
      // recorder — logging it here would just duplicate noise.
    },
    dispose: async () => {
      log("info", "opencode plugin logger disposing");
    },
  };
};
