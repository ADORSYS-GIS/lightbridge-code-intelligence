-- One index for the windowed, optionally repository-scoped feedback aggregates behind
-- `GET /analytics/feedback`.
--
-- Those aggregates are a cohort of the comments POSTED in the window, not of the reactions
-- reconciled in it (`db/analytics.rs` explains why), so they range over `review_comments.created_at`
-- and the hop from a comment to its reactions is already served by `review_feedback`'s unique
-- `(platform_comment_id, comment_kind, reactor, reaction)` index. Without this index the cohort is a
-- sequential scan of every comment ever posted; `db::analytics::tests` fails if the planner stops
-- using it.
CREATE INDEX IF NOT EXISTS review_comments_created_at_idx ON review_comments (created_at);
