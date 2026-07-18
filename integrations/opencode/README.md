# OpenCode integration (RFC-0009 PoC)

Everything Lightbridge ships *into* OpenCode when it hosts an agent loop
([RFC-0009](../../docs/rfc/0009-opencode-acp-agent-host.md),
[ADR-0094](../../docs/adr/0094-opencode-acp-open-mode-host.md),
[ADR-0095](../../docs/adr/0095-opencode-plugins-recording-and-gates.md)).

| Path | What |
|---|---|
| [`config/`](config/) | Per-mode OpenCode config: primary agent + subagents, permissions, MCP servers, plugin list. Rendered per task by the agent-plane supervisor (env placeholders filled in). |
| [`plugins/recorder/`](plugins/recorder/) | `@lightbridge/opencode-plugin-recorder` — right-bytes JSONL of every tool call (args + results, subagent-internal included) + session lifecycle, feeding the review quality gates. (Reasoning is no longer recorded — run observability moved to Loki logs, epic #459.) |
| [`plugins/gate-interlock/`](plugins/gate-interlock/) | `@lightbridge/opencode-plugin-gate-interlock` — blocks the terminal tool (`submit_findings` / `propose_pr`) until gate preconditions (refute pass, …) have mechanically happened in the session. |
| [`plugins/logger/`](plugins/logger/) | `@lightbridge/opencode-plugin-logger` — **the run-observability surface** for an OpenCode review ([ADR-0100](../../docs/adr/0100-retire-db-transcript-logs-as-observability.md), epic #459): per turn it emits `agent.reasoning`, `agent.content`, `agent.part.unknown` and `tool.done` at **info**, plus `tool.start` (tool input args) and `tool.output` (a bounded result preview) at **debug** — bounded + de-duped per completed part, dialed by `LCI_LOG_LEVEL`. winston-shaped JSON to **stderr** (stdout is the ACP channel), zero-dependency. A whole review is legible from these lines alone; the recorder (above) still exists but only feeds coverage accounting, not observability. |
| [`probe/`](probe/) | The RFC-0009 **Phase-0 fidelity probe** — a scripted ACP client + minimal MCP server that answers checklist items (a)–(f) against a pinned OpenCode version. Run it before adopting, and again on every OpenCode bump. |

## Invariants (do not relax in config)

- OpenCode runs **inside** the ADR-0088 sandbox pod with exactly the credentials the native loop
  held: LLM-gateway key + task-scoped runner token. No forge creds, no DB, no cluster identity.
- Every side effect leaves through a **mediated Lightbridge MCP tool** (`submit_findings`,
  `propose_pr`, …) — `webfetch` stays `deny`, and there is no authenticated git remote to push to.
- **Enforcement never depends on `experimental.*` hooks** — the gate-interlock blocks on the
  stable `tool.execute.before`; experimental hooks may only *steer*.
- OpenCode is **version-pinned**; a version bump re-runs the probe before it ships.
