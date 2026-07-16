import type { Plugin } from "@opencode-ai/plugin";

/**
 * Gate interlock (ADR-0095): block-until enforcement on the stable hook.
 *
 * Per-session state tracks which gate-relevant tools have actually executed;
 * `tool.execute.before` on the terminal tool THROWS until the preconditions hold. The thrown
 * message is the steering channel — it names exactly what is missing. The model cannot game a
 * tool that refuses to execute (the property `TurnFilter::force_names()` enforced in the native
 * loop, in the shape the plugin contract can express — see ADR-0095 for force-now vs block-until).
 *
 * Configuration is rendered per task/mode by the agent-plane supervisor:
 *   LCI_GATE_TERMINAL_TOOL   the tool to hold back (default: lightbridge_submit_findings)
 *   LCI_GATE_REQUIRED_TOOLS  comma-separated tools that must each have run first
 *   LCI_GATE_MIN_CALLS       minimum completed calls per required tool (default: 1)
 */

const terminalTool = process.env.LCI_GATE_TERMINAL_TOOL ?? "lightbridge_submit_findings";
const requiredTools = (process.env.LCI_GATE_REQUIRED_TOOLS ?? "lightbridge_refute_finding")
  .split(",")
  .map((name) => name.trim())
  .filter((name) => name.length > 0);
const minCalls = Math.max(1, Number(process.env.LCI_GATE_MIN_CALLS ?? "1") || 1);

export const GateInterlockPlugin: Plugin = async () => {
  const completedCalls = new Map<string, Map<string, number>>();

  return {
    "tool.execute.after": async (input) => {
      let counts = completedCalls.get(input.sessionID);
      if (!counts) {
        counts = new Map();
        completedCalls.set(input.sessionID, counts);
      }
      counts.set(input.tool, (counts.get(input.tool) ?? 0) + 1);
    },
    "tool.execute.before": async (input) => {
      if (input.tool !== terminalTool) return;
      const counts = completedCalls.get(input.sessionID);
      const missing = requiredTools.filter((tool) => (counts?.get(tool) ?? 0) < minCalls);
      if (missing.length === 0) return;
      throw new Error(
        `Gate interlock: ${terminalTool} is blocked until every gate precondition has run. ` +
          `Still missing: ${missing
            .map((tool) => `${tool} (needs ${minCalls - (counts?.get(tool) ?? 0)} more call(s))`)
            .join(", ")}. ` +
          `Complete the missing tool call(s), then call ${terminalTool} again.`,
      );
    },
  };
};
