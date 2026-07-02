-- ADR-0068: reaction-driven review lifecycle. An `@mention` review is triggered by a specific issue
-- comment; record that comment's GitHub id so the lifecycle reactions (👀 work-started, 👍 clean, 👎
-- findings, 😕 failure) can target the triggering COMMENT rather than the PR body. Nullable: the
-- automatic `pull_request opened` review has no trigger comment and keeps reacting on the PR itself.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS trigger_comment_id BIGINT;
