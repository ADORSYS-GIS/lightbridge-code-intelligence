-- Run-level review telemetry (extends ADR-0034/0017/0060 — NOT a new decision). For every REVIEW run
-- (fast + deep tiers), the runner records at run START:
--   * `run_tools`      — the exact tool set OFFERED to the model this run: the per-tier allowlist
--                        (ADR-0062) resolved together with the MCP-discovered external-knowledge tools
--                        (ADR-0066). A JSON array of `{ name, source }` where source is `builtin|mcp`.
--   * `run_config_b64` — the resolved `ReviewConfig` serialized to JSON, **REDACTED** (api_key + any
--                        credential-shaped passthrough → "[REDACTED]"), then base64-encoded, so a run's
--                        exact configuration (model, budgets, per-tier tools, the ~23KB system_prompt,
--                        `extra`, `stream`, resilience) is auditable later. Base64 is ENCODING, not
--                        encryption — the redaction happens BEFORE the encode (see the runner).
-- Both nullable and columns on `tasks` (one task = one run, so latest-run-replace is inherent — a retry
-- overwrites in place, matching how the transcript is replaced per run). INDEXING runs never submit, so
-- they leave both NULL.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS run_tools      JSONB,
    ADD COLUMN IF NOT EXISTS run_config_b64 TEXT;
