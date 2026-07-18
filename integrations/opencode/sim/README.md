# RFC-0009 offline loop simulation

Drives the **real** opencode binary + our **real** plugins through a full tool-call loop with **no
eaig and no control plane**, and asserts the gate-interlock + recorder behavior. This closes the
model-driven probe items (b/d/e/f) deterministically; only (c) reasoning fidelity needs a real model.

## Run

```
docker build --platform linux/arm64 -f integrations/opencode/Dockerfile -t lightbridge-agent-open:poc integrations/opencode
integrations/opencode/sim/run-sim.sh      # prints PASS/FAIL per assertion, exits non-zero on failure
```

## How it works

- **`mock-provider.mjs`** — an OpenAI-compatible provider (SSE) that opencode's bundled
  `@ai-sdk/openai-compatible` talks to. It is **self-adapting**: it reads the `tools` opencode
  advertises each turn and scripts the gate scenario by substring, so it doesn't hard-code tool
  names. Script: attempt `submit_findings` → (blocked) → `refute_finding` → `submit_findings` → done.
- **`mock-mcp.mjs`** — a stdio MCP named `lightbridge` stubbing `submit_findings`, `refute_finding`,
  `search`; each returns a marker (`SUBMIT_OK`/`REFUTE_OK`) the provider keys off, and logs the call.
- **`opencode.sim.jsonc`** — wires the custom provider, the stdio MCP, and the three real plugins
  (recorder/gate-interlock/logger) from `/plugins`.
- **`run-sim.sh`** — orchestrates it in a root container (adds `node` at runtime — the real image has
  none) and asserts the outcome.

## What it proves (all PASS as of 2026-07-16, opencode 1.18.2)

```
model turns   : submit_findings -> refute_finding -> submit_findings -> <done>
MCP executed  : refute_finding, submit_findings   (the FIRST submit never reached the tool)
PASS  gate-interlock blocked the premature submit_findings
PASS  blocked submit never reached the tool; refute ran first
PASS  refute_finding executed BEFORE the allowed submit_findings
PASS  recorder captured MCP tool RESULTS (right-bytes)
PASS  logger emitted tool.done with durationMs
```

- MCP tool naming is `lightbridge_<tool>` — matches the gate config and the `explore` `tools` denies.
- The gate throws in `tool.execute.before`; opencode feeds the error back to the model, which then
  satisfies the precondition and retries — block-until enforcement (ADR-0095) end-to-end.

## Probe item (c) — reasoning fidelity

Historically this item verified that per-turn reasoning survived the eaig path and reached the
**recorder** as `reasoning.part` entries (via `opencode.eaig.jsonc` + `run-eaig-reasoning.sh`). Epic
#459 retired that mechanism: the recorder no longer records reasoning, and run observability moved to
**Loki logs** (the logger plugin, `LCI_LOG_LEVEL`). The reasoning-reaches-the-logs probe is re-homed
onto the logger plugin under #462/#463; the old recorder-reasoning probe (script + `opencode.eaig.jsonc`)
was removed here with the transcript teardown (#461).

## Bug this harness caught

The recorder originally read `output.title/output/metadata` on `tool.execute.after` (the plugin's
declared TS type). But **MCP tools deliver `{content: [{type,text}], isError}` at runtime**, so the
recorder silently dropped **every mediated tool's result** — a right-bytes failure on exactly the
tools that matter. Fixed to record the full `output` verbatim; the sim now asserts results are kept.
