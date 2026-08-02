-- Per-repo review behaviour settings — the OPERATOR-side layer of a three-layer resolution model:
--
--   built-in default  →  repo config file (.lightbridge-code-review.jsonc)  →  THIS TABLE (wins)
--
-- The repo file stays the default source of truth (versioned, reviewable, owned by the repo's own
-- devs); a row here is the operator's escape hatch that needs no repo PR. Mirrors the ADR-0110
-- model-override precedent (migration 0034) in shape and posture.
--
-- Typed nullable columns rather than a key/value table or a JSONB blob: Postgres validates the enums
-- and the debounce range at write time, values decode straight into `Option<bool>` / `Option<String>`,
-- and a NULL means exactly one thing — "not overridden here, fall through to the file/default". A
-- JSONB blob would let a fat-fingered admin persist an unknown key silently; a K/V table would force a
-- stringly-typed value column with no per-key constraint.
--
-- No org-scoped sibling table (unlike 0034's org_model_overrides): these are per-project behavioural
-- choices, not a fleet-wide procurement decision. An org tier is purely additive later — a sibling
-- table plus one precedence step, no data migration.
CREATE TABLE IF NOT EXISTS repo_settings (
    repository_id         BIGINT PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,

    -- Post the "Lightbridge Review" check run / commit status for this repo's reviews.
    check_run_reporting   BOOLEAN,
    -- Run the automatic review when a PR/MR is opened (vs @mention-only).
    review_on_pr_open     BOOLEAN,
    -- Re-review when new commits land on an already-open PR/MR.
    review_on_push        BOOLEAN,
    -- How a burst of pushes is handled:
    --   supersede — cancel the in-flight review, review the newest head
    --   debounce  — wait for a quiet period so a burst collapses into one run
    --   every     — one full review per push
    push_strategy         TEXT CHECK (push_strategy IN ('supersede', 'debounce', 'every')),
    -- Quiet period for `debounce`. Bounded: below ~10s it cannot coalesce anything, above 15 min the
    -- review is too stale to be useful.
    push_debounce_seconds INTEGER CHECK (push_debounce_seconds BETWEEN 10 AND 900),
    -- Scope of ADR-0065 finding suppression on a re-review:
    --   pr     — suppress a finding already reported anywhere on this PR (survives line drift)
    --   commit — suppress only within the same head_sha (the pre-existing behaviour)
    -- Kept configurable as the kill switch for PR-wide suppression's false-positive risk.
    dedup_scope           TEXT CHECK (dedup_scope IN ('pr', 'commit')),

    set_by                TEXT        NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
