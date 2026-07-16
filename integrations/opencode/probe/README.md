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

## Offline validation — 2026-07-16 (no provider, no control plane)

Run against the real 1.18.2 binary in the image via `opencode agent list` / `opencode run` +
bind-mounts (no eaig, no MCP). Caught real bugs and validated the load-bearing mechanics:

- **⚠️ BUG (fixed): the config was invalid.** opencode's schema rejects `"//"` comment KEYS
  ("Unrecognized key: //"). Config is now `opencode.jsonc` with real `//` line comments.
- **Config accepted; our agents load** — `opencode agent list` shows `lci-open (primary)` and
  `explore (subagent)` beside the built-ins.
- **Permission/tools semantics confirmed empirically** — agent-level `permission`/`tools` deep-merge
  over and OVERRIDE the built-in agents (last-match-wins): `explore`'s `edit`/`bash`/`webfetch` denies
  win over the built-in `explore`'s *allows*, and the `tools` map resolves into real
  `lightbridge_propose_pr deny` entries. (Found + fixed a gap: built-in `explore` also had
  `websearch: allow` → now denied.)
- **⚠️ Plugin loading resolved.** Bare package-name `plugin` entries do NOT resolve in-image;
  **absolute paths** and **`.opencode/plugin/*.ts` auto-dir** both work. Capstone in the built image:
  all three plugins load from `/opt` absolute paths, the **logger + recorder init hooks fire**
  (recorder wrote `recorder.start`, logger printed its startup line), zero resolve/import errors.
- **The event bus reaches our plugins** — the logger's `event` hook emitted `session.updated` lines
  from real opencode events, so hooks fire on live events (partial evidence for probe item (d)/(f);
  full tool-call capture still needs the mock-provider harness below).

**Loop simulation — 2026-07-16 (offline, `../sim/`):** the mock-provider + mock-MCP harness now
drives the full tool-call loop and closes the model-driven items **(b) tool fidelity, (d) subagent/
tool visibility, (e) interlock, (f) recorder completeness** — all PASS (`sim/run-sim.sh`):
gate-interlock blocked a premature `submit_findings` (it never reached the tool), forced
`refute_finding` first, then allowed the retry; recorder captured tool args + **results**; logger
emitted `tool.done` with `durationMs`. MCP tool naming is `lightbridge_<tool>` (matches the gate +
`explore` denies). **Caught + fixed a recorder right-bytes bug**: MCP results arrive as
`{content,isError}` at runtime, not the typed `{title,output,metadata}` — the recorder now records
the full output. Only **(c) reasoning fidelity** still needs a real reasoning model (eaig).

## Evidence record

| Date | OpenCode version | (a) | (b) | (c) | (d) | (e) | (f) | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-07-16 | 1.18.2 | ⚠️ | — | — | — | — | — | ACP `initialize` handshake ✓ (vendored-release image); client MCP is http/sse-only (rerun (a) with an HTTP MCP); (b)–(f) need eaig provider creds in-container |
