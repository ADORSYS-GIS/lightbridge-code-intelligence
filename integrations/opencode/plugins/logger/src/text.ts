/**
 * Pure text/JSON helpers for the logger plugin (bounding, code-point counting, tool-result preview).
 *
 * These live in a SIBLING module — NOT in `index.ts` — on purpose. OpenCode's plugin loader treats
 * EVERY export of a configured plugin module as a `Plugin` factory: it invokes each exported value
 * with the plugin input and then reads `.config`/hooks off the result. A plugin module that also
 * exported these helpers would have opencode call `bounded(pluginInput)` → `undefined`, then
 * `undefined.config` → the loader crashes with "plugin config hook failed … undefined is not an
 * object (evaluating '….config')", and on some opencode builds that failure cascades into a
 * `session/new` "directory" service error — i.e. the whole review can't start. Keeping `index.ts` to
 * a SINGLE export (`LoggerPlugin`, mirroring the recorder/gate-interlock plugins) is the contract;
 * anything the plugin or its tests need to share lives here and is imported, never re-exported from
 * the entry module.
 */

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
export function countCodePoints(text: string): number {
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
