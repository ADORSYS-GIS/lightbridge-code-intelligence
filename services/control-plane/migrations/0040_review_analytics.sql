-- Review analytics (ADR-0116 D6): a normalized projection of `reviews.findings`,
-- plus the indexes the windowed, repository-scoped aggregates behind `GET /analytics/*` range over.
--
-- `reviews.findings` stays the verbatim audit record of what the agent said. `review_findings` is a
-- projection of it with the ADR-0032 priority/category fallbacks resolved once, at write time, so
-- "findings by priority for repository X last month" is an indexed join rather than a jsonb explosion
-- plus a per-element CASE repeated in every query.
--
-- Kept in sync by a trigger rather than by each writer: `reviews` is written from two call sites
-- (`upsert_review` at reconciler drain, `insert_review_if_absent` on the silent-clean path), and a
-- projection that depends on each of them remembering drifts the first time a third one appears — the
-- same reasoning 0039 applied to `repositories.last_task_at`.

-- The fallbacks, stated once in SQL. `db::analytics::tests` asserts they agree with
-- `Finding::priority` / `Finding::category` (src/review.rs), which remain the source of truth.
CREATE OR REPLACE FUNCTION review_finding_priority(f jsonb) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE
        WHEN upper(btrim(coalesce(f->>'priority', ''), E' \t\r\n')) IN ('P0', 'P1', 'P2')
            THEN upper(btrim(f->>'priority', E' \t\r\n'))
        WHEN lower(btrim(coalesce(f->>'severity', ''), E' \t\r\n')) IN ('error', 'critical')
            THEN 'P0'
        WHEN lower(btrim(coalesce(f->>'severity', ''), E' \t\r\n')) IN ('warning', 'warn', 'high')
            THEN 'P1'
        ELSE 'P2'
    END
$$;

CREATE OR REPLACE FUNCTION review_finding_category(f jsonb) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT coalesce(nullif(btrim(f->>'category', E' \t\r\n'), ''), 'correctness')
$$;

-- A line number, or NULL for anything that is not one. A cast that raised here would abort the review
-- write the trigger below runs inside of.
CREATE OR REPLACE FUNCTION review_finding_int(v text) RETURNS int
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE WHEN v ~ '^[0-9]{1,9}$' THEN v::int END
$$;

CREATE TABLE IF NOT EXISTS review_findings (
    task_id    uuid NOT NULL REFERENCES reviews (task_id) ON DELETE CASCADE,
    -- 0-based position in `reviews.findings`. A finding carries no id of its own, so its position in
    -- the audit record is its identity in the projection.
    idx        int  NOT NULL,
    file       text,
    line       int,
    start_line int,
    priority   text NOT NULL CHECK (priority IN ('P0', 'P1', 'P2')),
    category   text NOT NULL,
    title      text,
    PRIMARY KEY (task_id, idx)
);

-- The comment → finding lookup the feedback aggregates make on `review_comments.(task_id, file, line)`.
-- `(task_id)` alone is already the primary key's prefix. No separate `(priority)` / `(category)`
-- index: at three and a handful of distinct values the planner would not choose one for a GROUP BY.
CREATE INDEX IF NOT EXISTS review_findings_location_idx ON review_findings (task_id, file, line);

CREATE OR REPLACE FUNCTION project_review_findings(p_task_id uuid, p_findings jsonb) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM review_findings WHERE task_id = p_task_id;
    IF jsonb_typeof(p_findings) IS DISTINCT FROM 'array' THEN
        RETURN;
    END IF;
    INSERT INTO review_findings (task_id, idx, file, line, start_line, priority, category, title)
    SELECT p_task_id,
           (e.ord - 1)::int,
           e.f->>'file',
           review_finding_int(e.f->>'line'),
           review_finding_int(e.f->>'start_line'),
           review_finding_priority(e.f),
           review_finding_category(e.f),
           e.f->>'title'
    FROM jsonb_array_elements(p_findings) WITH ORDINALITY AS e(f, ord);
END
$$;

CREATE OR REPLACE FUNCTION reviews_project_findings() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.findings IS NOT DISTINCT FROM OLD.findings THEN
        RETURN NULL;
    END IF;
    PERFORM project_review_findings(NEW.task_id, NEW.findings);
    RETURN NULL;
EXCEPTION WHEN OTHERS THEN
    -- By the time `reviews` is written the review is already on the forge, and this row is its audit
    -- record: a projection failure must never roll that write back. Loud in the Postgres log instead,
    -- and the analytics undercount rather than the record going missing.
    RAISE WARNING 'review_findings projection failed for task %: %', NEW.task_id, SQLERRM;
    RETURN NULL;
END
$$;

-- Backfill before the trigger exists, so every existing review is projected exactly once.
DO $$
DECLARE
    r record;
BEGIN
    FOR r IN SELECT task_id, findings FROM reviews LOOP
        PERFORM project_review_findings(r.task_id, r.findings);
    END LOOP;
END
$$;

DROP TRIGGER IF EXISTS reviews_project_findings ON reviews;
CREATE TRIGGER reviews_project_findings
    AFTER INSERT OR UPDATE OF findings ON reviews
    FOR EACH ROW EXECUTE FUNCTION reviews_project_findings();

-- Windowed per-repository run aggregates: `repository_id = $1 AND created_at` in the window.
CREATE INDEX IF NOT EXISTS tasks_repository_created_at_idx ON tasks (repository_id, created_at DESC);

-- Review and finding aggregates are windowed on the review's own finalize time.
CREATE INDEX IF NOT EXISTS reviews_created_at_idx ON reviews (created_at);

-- Feedback aggregates are a cohort of the comments POSTED in the window (`db/analytics.rs` explains why
-- not the reaction's reconcile time), so they range over the comment's timestamp. The hop from a
-- comment to its reactions is already served by `review_feedback`'s unique
-- `(platform_comment_id, comment_kind, reactor, reaction)` index.
CREATE INDEX IF NOT EXISTS review_comments_created_at_idx ON review_comments (created_at);
