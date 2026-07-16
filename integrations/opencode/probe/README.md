# RFC-0009 Phase-0 probe

Answers the [RFC-0009](../../../docs/rfc/0009-opencode-acp-agent-host.md) checklist against a
**pinned** OpenCode version, with hard evidence. Run it before adopting, and again on **every**
OpenCode bump — the plugin hook contract and the ACP surface are the load-bearing dependencies.

## Checklist

| Item | Question | How it's answered |
|---|---|---|
| (a) | Does OpenCode honor MCP servers passed by the ACP client at `session/new`? | Automated: the probe passes a throwaway MCP server; a live `probe_echo` call writes a marker file. If FAIL, per-task MCP servers must ride the rendered config instead — a config decision, not a blocker. |
| (b) | Do ACP tool-call updates carry raw input/output (right-bytes)? | Automated: counts `rawInput`/`rawOutput` on `tool_call`/`tool_call_update` updates. |
| (c) | Does reasoning surface as `agent_thought_chunk` through the eaig path? | Automated count; a zero needs a manual check of the model/provider reasoning config before it counts as FAIL. |
| (d) | Are **subagent-internal** tool calls visible to the recorder? | Manual: run a session whose prompt delegates to a subagent, with the recorder plugin loaded; the recorder JSONL must contain the subagent's tool calls. |
| (e) | Does the gate-interlock actually block and does the loop recover? | Manual: load the gate-interlock with `LCI_GATE_REQUIRED_TOOLS` set to a tool the prompt is told to skip; the terminal call must fail with the steering error, and a follow-up satisfying the gate must succeed — no wedge. |
| (f) | Is recorder JSONL ⊇ ACP-visible information? | Manual: diff the recorder file from (d)/(e) against `probe-output.jsonl`. |

**Per RFC-0009: a failing (b), (c) or (e) fails the RFC's gate.** A failing (a) merely picks the
config-side wiring.

## Running

Requires Node ≥ 22.6 (`--experimental-strip-types`) and an `opencode` binary on PATH (or
`OPENCODE_BIN=/path/to/opencode`), authenticated against the LLM provider under test — for the
real verdict, the eaig gateway path, not a vendor-direct key.

```
$ pnpm install
$ OPENCODE_BIN=opencode pnpm --filter @lightbridge/opencode-probe probe /path/to/some/repo
```

The automated verdicts print as PASS/FAIL/UNKNOWN; the full wire log lands in
`probe-output.jsonl`. For (d)–(f), load the plugins into the target repo's OpenCode config (e.g.
symlink `../plugins/{recorder,gate-interlock}/src/index.ts` into the target's
`.opencode/plugins/`, or list the packages in its `opencode.json` `plugin` array — whichever
mechanics work is itself a probe finding to record here).

## Findings — 2026-07-16 (image `lightbridge-agent-open:poc`, opencode **v1.18.2**)

First real run against the pinned binary in the [`../Dockerfile`](../Dockerfile) image (the vendored
`opencode-linux-*-musl` v1.18.2 release on our own Alpine base). Not a full probe (no provider creds
wired, so no model turn yet), but the ACP surface is validated:

- **`opencode acp` speaks ACP; the `initialize` handshake matches this probe's request shape** —
  the harness is correct against the real binary. Response `agentInfo.version` = `1.18.2`.
- **⚠️ Two version lines exist — pick the release, not the container tag.** The release tarball
  reports `--version` = `1.18.2` (matches npm `@opencode-ai/plugin`); the `ghcr.io/anomalyco/opencode`
  *container image* (tag `1.0.196`) reports `1.0.196` — a separate build line. The Dockerfile vendors
  the **release** so the runtime aligns with the plugin line.
- **⚠️ MCP transport: `agentCapabilities.mcpCapabilities` = `{http:true, sse:true}` — no `stdio`.**
  Client-passed MCP servers over ACP `session/new` are accepted as **http/sse only**. So item (a)
  must be run with an **HTTP** MCP server; the current stdio `probe-mcp-server.ts` would report a
  FAIL that means "stdio unsupported over ACP", **not** "client MCP unsupported" — uninformative.
  This matches production: [`../config/opencode.json`](../config/opencode.json) already declares the
  mediated Lightbridge MCP as `type: remote` (http). Stdio MCP, if ever needed, goes via opencode's
  own config, not over ACP.
- **`agentCapabilities.loadSession: true`** — session load/resume exists (noted against the
  replay-drop discussion: restart-on-failure is a choice, not a missing capability).
- **`session/prompt` needs a provider** (`authMethods: [opencode-login]`) — the model-driven
  verdicts (b) reasoning/(c) fidelity require the eaig gateway wired into the container. That is the
  next run's prerequisite.

## Evidence record

| Date | OpenCode version | (a) | (b) | (c) | (d) | (e) | (f) | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-07-16 | 1.18.2 | ⚠️ | — | — | — | — | — | ACP `initialize` handshake ✓ (vendored-release image); client MCP is http/sse-only (rerun (a) with an HTTP MCP); (b)–(f) need eaig provider creds in-container |
