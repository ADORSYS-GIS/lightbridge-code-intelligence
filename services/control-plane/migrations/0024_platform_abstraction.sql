-- Platform abstraction: rename GitHub-specific columns and tables to platform-agnostic names.
-- All existing rows get platform = 'github' (the default), so existing behaviour is unchanged.
--
-- This migration is a pure rename + add-default-column refactor. No data is lost, no behaviour
-- changes. The application code is updated in the same deploy to use the new column names.

BEGIN;

-- 1. repositories: add platform, rename github_repo_id → platform_repo_id
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE repositories RENAME COLUMN github_repo_id TO platform_repo_id;

-- The old UNIQUE constraint on github_repo_id became a UNIQUE on platform_repo_id after the
-- rename. A GitHub repo ID and a GitLab project ID could collide numerically, so we need a
-- composite unique on (platform, platform_repo_id).
DROP INDEX IF EXISTS repositories_platform_repo_id_key;
CREATE UNIQUE INDEX IF NOT EXISTS repositories_platform_repo_id_unique
    ON repositories (platform, platform_repo_id);

-- 2. tasks: rename github_delivery_id → webhook_delivery_id
-- (installation_id stays — it holds the GitHub installation ID or the GitLab project ID)
ALTER TABLE tasks RENAME COLUMN github_delivery_id TO webhook_delivery_id;

-- 3. github_deliveries → webhook_deliveries
ALTER TABLE github_deliveries RENAME TO webhook_deliveries;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';

-- 4. github_outbox → outbox
ALTER TABLE github_outbox RENAME TO outbox;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
-- Rename github_id → platform_ref_id (the ID of the posted resource on the platform)
ALTER TABLE outbox RENAME COLUMN github_id TO platform_ref_id;

-- 5. reviews: rename github_review_id → platform_review_id
ALTER TABLE reviews RENAME COLUMN github_review_id TO platform_review_id;

-- 6. review_comments: rename github_comment_id → platform_comment_id
ALTER TABLE review_comments RENAME COLUMN github_comment_id TO platform_comment_id;

-- 7. review_feedback: add platform
ALTER TABLE review_feedback ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';

COMMIT;