-- Per-identity (repo/org) model selection (ADR-0110, story #501) — the follow-up ADR-0038's
-- amendment promised. Deliberately scoped to repo + org, no per-user tier: this schema has no
-- user-identity concept today (only `installation_id` on `repositories`, the closest thing to an
-- org key), so per-user precedence is out of scope until a real user table exists (ADR-0110).

-- One row per repo; absent = inherit from the org override (or the global `LLM_MODEL` default).
CREATE TABLE repo_model_overrides (
    repository_id BIGINT PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    set_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per installation (org). Not FK'd to `repositories` (an installation can own zero or many
-- repos, and may be recorded before any repo row exists — mirrors `tasks.installation_id`, which is
-- also a bare BIGINT with no FK for the same reason).
CREATE TABLE org_model_overrides (
    installation_id BIGINT PRIMARY KEY,
    model TEXT NOT NULL,
    set_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The model override resolved at task-creation time (repo -> org -> global fallback chain,
-- `preset.rs`'s same three-tier shape), applied by the runner as the FINAL override on the
-- preset-resolved `ReviewConfig.model` — never touching tools/gates/budgets. NULL = no override,
-- the preset's own configured model applies unchanged.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS model_override TEXT;
