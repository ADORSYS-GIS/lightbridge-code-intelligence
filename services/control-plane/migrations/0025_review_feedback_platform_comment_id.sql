-- ADR-0071 follow-up: migration 0024 renamed `review_comments.github_comment_id` →
-- `platform_comment_id` but missed `review_feedback.github_comment_id`. The application code
-- (db.rs `rejected_findings_for_repo`, `get_feedback`, `record_feedback`) queries
-- `f.platform_comment_id`, so without this rename every feedback lookup fails with
-- `column f.platform_comment_id does not exist`.
--
-- Also rename the UNIQUE constraint that still carries the old `github_comment_id` name so
-- future `ON CONFLICT (platform_comment_id, comment_kind, reactor, reaction)` clauses match.
--
-- Idempotent: uses a DO block to check whether the old column still exists before renaming,
-- so re-running this migration (e.g. after a manual partial apply) is safe.

BEGIN;

-- 1. review_feedback: rename github_comment_id → platform_comment_id (if not already renamed).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'review_feedback' AND column_name = 'github_comment_id'
    ) THEN
        ALTER TABLE review_feedback RENAME COLUMN github_comment_id TO platform_comment_id;
    END IF;
END $$;

-- 2. Drop the old-named UNIQUE constraint and recreate with the new column name.
ALTER TABLE review_feedback
    DROP CONSTRAINT IF EXISTS review_feedback_github_comment_id_comment_kind_reactor_reac_key;
CREATE UNIQUE INDEX IF NOT EXISTS review_feedback_platform_comment_id_comment_kind_reactor_reac_key
    ON review_feedback (platform_comment_id, comment_kind, reactor, reaction);

COMMIT;