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
 * This is THE observability surface for an OpenCode review (epic #459 / #462). The DB transcript is
 * being removed (#461): on the OpenCode-over-ACP path we do not own the loop, so the recorder →
 * `transcript_from_recorder` reconstruction is a post-hoc record, not resumable execution state.
 * Loki logs are now the single place a review is reconstructed from a live tail (`kubectl logs`),
 * and this plugin is the emitter. It sees every message part + tool call in-process. A whole review
 * — the model's reasoning, its visible content, and each tool's inputs + a bounded output preview —
 * must be legible from these lines alone. (The recorder file still exists but only feeds
 * `cycle_turn_outcome` for coverage accounting; it is NOT an observability dependency here.)
 *
 * Level dial — pick how much of the loop you want in the tail:
 *   info  = the readable narrative: `agent.content` (the model's visible message), `tool.done`
 *           (per-call name/ok/duration), `session.*` lifecycle, and `permission.ask` decisions.
 *   debug = the full forensic overview: everything at info PLUS `agent.reasoning` (chain-of-thought),
 *           `tool.start` carrying the tool INPUT args, and `tool.output` carrying a bounded preview
 *           of each tool's RESULT.
 *
 * Config (rendered per task by the supervisor):
 *   LCI_LOG_LEVEL              error | warn | info | debug   (default: info)
 *   LCI_LOG_SERVICE            service label on every line   (default: lci-opencode)
 *   LCI_LOG_REASONING_CHARS    cap for `agent.reasoning` text   (default: 4000, `0` = unbounded)
 *   LCI_LOG_CONTENT_CHARS      cap for `agent.content` text     (default: 4000, `0` = unbounded)
 *   LCI_LOG_TOOL_ARGS_CHARS    cap for `tool.start` args        (default: 4000, `0` = unbounded)
 *   LCI_LOG_TOOL_OUTPUT_CHARS  cap for `tool.output` preview    (default: 4000, `0` = unbounded)
 * The char defaults mirror the native `REASONING_LOG_CHARS`/`CONTENT_LOG_CHARS` (4000).
 */

type Level = "error" | "warn" | "info" | "debug";
const LEVELS: Record<Level, number> = { error: 0, warn: 1, info: 2, debug: 3 };

const CAP_DEFAULT = 4000;

/** Resolve a char cap from `name`: absent/non-numeric/negative → `fallback`; `0` → unbounded. */
function readCap(name: string, fallback: number): number {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  const parsed = Number.parseInt(raw.trim(), 10);
  return Number.isNaN(parsed) || parsed < 0 ? fallback : parsed;
}

/**
 * The slice of `text` to show on a log line, or `undefined` when it is blank (skip the line — a pure
 * tool-call turn has no prose). `cap === 0` returns the whole string; otherwise the text is bounded
 * to at most `cap` Unicode code points and a `…[+N chars]` marker records how many were dropped.
 * Iterating code points (via the string iterator) never slices a surrogate pair.
 *
 * Defensive by contract: a non-string `text` (any shape) returns `undefined` rather than throwing —
 * this runs on the hot path off untrusted wire shapes and must never take the agent loop down.
 * Allocation-free on the common path: the whole-string `Array.from` is gone. A string's UTF-16
 * `length` is an upper bound on its code-point count, so when `length <= cap` the text provably fits
 * and is returned as-is with zero allocation; only when we actually truncate do we materialize the
 * kept prefix (never the whole string).
 */
export function bounded(text: string | undefined | null, cap: number): string | undefined {
  if (typeof text !== "string" || text.trim() === "") return undefined;
  if (cap === 0) return text;
  if (text.length <= cap) return text; // UTF-16 length ≥ code points ⇒ within cap, no allocation
  const kept: string[] = [];
  let count = 0;
  for (const ch of text) {
    if (count < cap) kept.push(ch); // one entry per code point — never splits a surrogate pair
    count += 1;
  }
  if (count <= cap) return text; // surrogates collapsed the count back under the cap
  return `${kept.join("")}…[+${count - cap} chars]`;
}

/** Count Unicode code points without allocating (the string iterator collapses surrogate pairs). */
function countCodePoints(text: string): number {
  let count = 0;
  for (const _ of text) count += 1;
  return count;
}

/**
 * Extract the human-visible preview text of a tool result, mirroring the native `result_text`
 * (services/review-agent/src/opencode/recorder.rs): join `content[].text` (the MCP shape the
 * mediated review tools return, `{content:[{type,text}],isError}`); else the built-in `output`/
 * `title` string; else a bare string; else JSON. Never throws.
 */
export function resultText(result: unknown): string {
  try {
    if (result !== null && typeof result === "object") {
      const obj = result as Record<string, unknown>;
      const content = obj.content;
      if (Array.isArray(content)) {
        const joined = content
          .map((item) =>
            item !== null && typeof item === "object"
              ? (item as { text?: unknown }).text
              : undefined,
          )
          .filter((t): t is string => typeof t === "string")
          .join("\n");
        if (joined !== "") return joined;
      }
      for (const key of ["output", "title"] as const) {
        const value = obj[key];
        if (typeof value === "string") return value;
      }
    }
    if (typeof result === "string") return result;
    return JSON.stringify(result) ?? String(result);
  } catch {
    return String(result);
  }
}

export const LoggerPlugin: Plugin = async ({ project, directory, worktree }) => {
  // Config is read per factory invocation (once per task process). Keeping it here — rather than at
  // module top-level — means a test can exercise a fresh level/caps per call without re-importing.
  const envLevel = process.env.LCI_LOG_LEVEL as Level | undefined;
  // `Object.hasOwn`, not `envLevel in LEVELS`: `in` walks the prototype chain, so `LCI_LOG_LEVEL`
  // set to `toString`/`valueOf`/… would resolve to an inherited function and make `threshold` a
  // function (never a number) — every level comparison would then misbehave. Own-key only.
  const threshold =
    envLevel !== undefined && Object.hasOwn(LEVELS, envLevel) ? LEVELS[envLevel] : LEVELS.info;
  const service = process.env.LCI_LOG_SERVICE ?? "lci-opencode";
  const reasoningCap = readCap("LCI_LOG_REASONING_CHARS", CAP_DEFAULT);
  const contentCap = readCap("LCI_LOG_CONTENT_CHARS", CAP_DEFAULT);
  const toolArgsCap = readCap("LCI_LOG_TOOL_ARGS_CHARS", CAP_DEFAULT);
  const toolOutputCap = readCap("LCI_LOG_TOOL_OUTPUT_CHARS", CAP_DEFAULT);

  const base = {
    service,
    projectID: project?.id,
    worktree,
    directory,
  };

  const enabled = (level: Level): boolean => LEVELS[level] <= threshold;

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

  // A hook must NEVER throw into the agent loop (the recorder's contract). `log()` guards its own
  // I/O, but the pre-log meta computation (bounding, code-point counts, wire-shape access) can throw
  // on an unexpected event shape; `guard` contains that so a malformed event can't take the loop
  // down. Mirrors the recorder's `guard` — swallow to stderr, never rethrow.
  const guard = (fn: () => void): void => {
    try {
      fn();
    } catch (error) {
      console.error("[lci-logger] hook error (swallowed):", error);
    }
  };

  // Serialize a value to a bounded JSON string for a log line. `undefined` in → `undefined` out (no
  // `args`/`properties` field on the line), never the literal string "undefined". Never throws.
  const boundedJson = (value: unknown, cap: number): string | undefined => {
    if (value === undefined) return undefined;
    let serialized: string;
    try {
      serialized = typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
    } catch {
      serialized = String(value);
    }
    return bounded(serialized, cap);
  };
  const boundedArgs = (args: unknown): string | undefined => boundedJson(args, toolArgsCap);

  // callID → start time, for tool durations. tool.execute.after fires per call, so this stays small.
  const started = new Map<string, number>();

  // Streaming de-duplication of `message.part.updated`.
  //
  // `message.part.updated` is a STREAMING event: on a token-streaming provider it re-fires on every
  // chunk while a text/reasoning part grows, each fire carrying the accumulated `part.text` so far.
  // Logging on every fire would emit hundreds of growing duplicate lines per message (at `info`, the
  // default). Instead we upsert the latest snapshot per part id and emit each part EXACTLY ONCE with
  // its final text — when the part signals completion (`part.time.end`), else on the per-cycle
  // boundary (`session.idle`) or at `dispose`. `emittedParts` guards against a double-emit; both maps
  // are cleared at each flush so the next cycle starts clean.
  type PendingPart = { level: Level; message: string; text: unknown; cap: number };
  const pendingParts = new Map<string, PendingPart>();
  const emittedParts = new Set<string>();

  const emitPart = (id: string, part: PendingPart): void => {
    pendingParts.delete(id);
    if (emittedParts.has(id)) return;
    emittedParts.add(id);
    const text = bounded(typeof part.text === "string" ? part.text : undefined, part.cap);
    if (text !== undefined) {
      log(part.level, part.message, {
        chars: countCodePoints(typeof part.text === "string" ? part.text : ""),
        text,
      });
    }
  };

  const flushParts = (): void => {
    for (const [id, part] of pendingParts) emitPart(id, part);
    pendingParts.clear();
    emittedParts.clear();
  };

  // Note: never put a `level`/`message`/`timestamp` key in meta — it would clobber the winston
  // top-level field of the same name (meta is spread last).
  log("info", "opencode plugin logger started", {
    pid: process.pid,
    configuredLevel: envLevel ?? "info",
  });

  return {
    "tool.execute.before": async (input, output) => {
      guard(() => {
        started.set(input.callID, Date.now());
        // At debug, carry the tool INPUT args (bounded) so a forensic tail shows exactly what the
        // model asked each tool to do — this is the observability surface, not just coarse timing.
        log("debug", "tool.start", {
          tool: input.tool,
          callID: input.callID,
          sessionID: input.sessionID,
          args: enabled("debug") ? boundedArgs(output?.args) : undefined,
        });
      });
    },
    "tool.execute.after": async (input, output) => {
      guard(() => {
        const startedAt = started.get(input.callID);
        started.delete(input.callID);
        log("info", "tool.done", {
          tool: input.tool,
          callID: input.callID,
          sessionID: input.sessionID,
          durationMs: startedAt === undefined ? undefined : Date.now() - startedAt,
          // ok reflects the MCP result flag ({content,isError} at runtime for mediated tools);
          // `title` is populated for built-in tools.
          ok: (output as { isError?: boolean }).isError !== true,
          title: (output as { title?: string }).title,
        });
        // At debug, a bounded preview of the tool's RESULT (either result shape — see resultText).
        if (enabled("debug")) {
          const preview = bounded(resultText(output), toolOutputCap);
          if (preview !== undefined) {
            log("debug", "tool.output", {
              tool: input.tool,
              callID: input.callID,
              sessionID: input.sessionID,
              preview,
            });
          }
        }
      });
    },
    "permission.ask": async (input, output) => {
      guard(() => {
        // Security-relevant: what the agent asked to do and how policy answered.
        log("info", "permission.ask", {
          type: (input as { type?: string }).type,
          pattern: (input as { pattern?: string }).pattern,
          status: output.status,
        });
      });
    },
    event: async ({ event }) => {
      guard(() => {
        const type = event.type;
        if (type === "message.part.updated") {
          // The model's own output, streamed as parts. `text` is its visible answer (info — the
          // review narrative); `reasoning` is its chain-of-thought (debug — the forensic overview).
          // OpenCode 1.18.2 surfaces the visible answer as `part.type === "text"`. This event is
          // STREAMING: track the latest snapshot per part id and emit once (see the maps above).
          const part = (
            event.properties as
              | {
                  part?: {
                    id?: string;
                    type?: string;
                    text?: unknown;
                    synthetic?: boolean;
                    ignored?: boolean;
                    time?: { start?: number; end?: number };
                  };
                }
              | undefined
          )?.part;
          if (part === undefined || typeof part.id !== "string") return;
          if (part.type === "reasoning") {
            if (!enabled("debug")) return; // reasoning is debug-only — don't even track at info
            if (!emittedParts.has(part.id)) {
              pendingParts.set(part.id, {
                level: "debug",
                message: "agent.reasoning",
                text: part.text,
                cap: reasoningCap,
              });
            }
          } else if (part.type === "text") {
            // Synthetic/injected parts aren't the model's genuine visible answer — never log them.
            if (part.synthetic === true || part.ignored === true) return;
            if (!emittedParts.has(part.id)) {
              pendingParts.set(part.id, {
                level: "info",
                message: "agent.content",
                text: part.text,
                cap: contentCap,
              });
            }
          } else {
            return;
          }
          // Emit as soon as the part signals completion; otherwise it flushes on session.idle.
          if (part.time?.end) {
            const pending = pendingParts.get(part.id);
            if (pending !== undefined) emitPart(part.id, pending);
          }
          return;
        }
        if (type === "session.idle") {
          // The per-cycle boundary: guarantee-flush any part that never got a completion marker.
          flushParts();
          log("info", type, { properties: event.properties });
          return;
        }
        if (type === "session.error") {
          // Bound the error payload (stack/properties can be unbounded) — reuse the tool-output cap.
          log("error", "session.error", {
            properties: boundedJson(event.properties, toolOutputCap),
          });
        } else if (type.startsWith("session.")) {
          log("info", type, { properties: event.properties });
        }
        // Other bus events (tool state) are covered by the hooks above.
      });
    },
    dispose: async () => {
      guard(() => {
        flushParts(); // last-chance flush for anything still pending at teardown
        log("info", "opencode plugin logger disposing");
      });
    },
  };
};
