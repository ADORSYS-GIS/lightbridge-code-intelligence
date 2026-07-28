-- Named review presets (ADR-0103) replace the hardcoded fast/deep tier model. `tier` (0021,
-- "fast"|"deep" only) is renamed to `preset` — same column, same semantics, but now an arbitrary
-- operator-configured name resolved per repo config (ADR-0030's `preset`/`entry_points`), not one of
-- two hardcoded literals. Hard cutover, no compat column (this repo's convention) — every read/write
-- call site is updated in the same change.
ALTER TABLE tasks RENAME COLUMN tier TO preset;

-- `entry_point` records WHICH trigger created the task (`pr_open`, `mention`, `a2a`) — kept separate
-- from `preset` because preset names are now arbitrary/operator-defined, so a decision like "post the
-- quick-pass banner vs the full truncation note" can no longer key off a preset name (there is no
-- guarantee an operator's custom preset is even named "fast"). Default `mention` for the same reason
-- 0021 defaulted `tier` to `deep`: the safe/full behavior for any pre-existing row or non-review task.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS entry_point TEXT NOT NULL DEFAULT 'mention';
