# OpenCode integration (RFC-0009 PoC)

Everything Lightbridge ships *into* OpenCode when it hosts an agent loop
([RFC-0009](../../docs/rfc/0009-opencode-acp-agent-host.md),
[ADR-0094](../../docs/adr/0094-opencode-acp-open-mode-host.md),
[ADR-0095](../../docs/adr/0095-opencode-plugins-recording-and-gates.md)).

| Path | What |
|---|---|
| [`config/`](config/) | Per-mode OpenCode config: primary agent + subagents, permissions, MCP servers, plugin list. Rendered per task by the agent-plane supervisor (env placeholders filled in). |
| [`plugins/recorder/`](plugins/recorder/) | `@lightbridge/opencode-plugin-recorder` — right-bytes JSONL transcript of every tool call (args + results, subagent-internal included) and every reasoning part. |
| [`plugins/gate-interlock/`](plugins/gate-interlock/) | `@lightbridge/opencode-plugin-gate-interlock` — blocks the terminal tool (`submit_findings` / `propose_pr`) until gate preconditions (refute pass, …) have mechanically happened in the session. |
| [`plugins/logger/`](plugins/logger/) | `@lightbridge/opencode-plugin-logger` — operational logs (lifecycle, coarse tool timing, errors, permission decisions) as winston-shaped JSON to **stderr** (stdout is the ACP channel). Zero-dependency, not the transcript — that's the recorder. |
| [`probe/`](probe/) | The RFC-0009 **Phase-0 fidelity probe** — a scripted ACP client + minimal MCP server that answers checklist items (a)–(f) against a pinned OpenCode version. Run it before adopting, and again on every OpenCode bump. |

## Invariants (do not relax in config)

- OpenCode runs **inside** the ADR-0088 sandbox pod with exactly the credentials the native loop
  held: LLM-gateway key + task-scoped runner token. No forge creds, no DB, no cluster identity.
- Every side effect leaves through a **mediated Lightbridge MCP tool** (`submit_findings`,
  `propose_pr`, …) — `webfetch` stays `deny`, and there is no authenticated git remote to push to.
- **Enforcement never depends on `experimental.*` hooks** — the gate-interlock blocks on the
  stable `tool.execute.before`; experimental hooks may only *steer*.
- OpenCode is **version-pinned**; a version bump re-runs the probe before it ships.
