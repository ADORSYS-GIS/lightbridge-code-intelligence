//! Per-repo review-behaviour override storage (migration 0036): `repo_settings` is a one-row-per-repo
//! table — set/clear/read only, no history. Three-layer resolution (built-in default → repo config
//! file → this table) lives in [`crate::settings`], not here; this module is pure persistence,
//! mirroring [`super::model_overrides`]'s shape for the ADR-0110 model override.
//!
//! Every column is nullable and a NULL means exactly one thing: "not overridden here, fall through to
//! the file/default". So `set_repo_settings` takes an `Option<Option<T>>` per field — `None` leaves
//! the column untouched, `Some(None)` clears it, `Some(Some(v))` sets it.

use sqlx::PgPool;

/// A repo's stored overrides. Every field is `None` when that setting is not overridden.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoSettingsRow {
    pub check_run_reporting: Option<bool>,
    pub review_on_pr_open: Option<bool>,
    pub review_on_push: Option<bool>,
    pub push_strategy: Option<String>,
    pub push_debounce_seconds: Option<i32>,
    pub dedup_scope: Option<String>,
}

/// A repo's stored overrides, or `None` when the repo has no settings row at all. Callers treat both
/// "no row" and "row with all-NULL columns" identically — the resolution layer falls through either
/// way — but they are kept distinct here so the admin API can tell an operator whether a row exists.
pub async fn get_repo_settings(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<RepoSettingsRow>, sqlx::Error> {
    sqlx::query_as::<_, RepoSettingsRow>(
        "SELECT check_run_reporting, review_on_pr_open, review_on_push, push_strategy, \
                push_debounce_seconds, dedup_scope \
         FROM repo_settings WHERE repository_id = $1",
    )
    .bind(repository_id)
    .fetch_optional(pool)
    .await
}

/// The per-field patch applied by [`set_repo_settings`]. `None` leaves the stored value alone;
/// `Some(None)` clears it (back to file/default); `Some(Some(v))` sets it.
#[derive(Debug, Clone, Default)]
pub struct RepoSettingsPatch {
    pub check_run_reporting: Option<Option<bool>>,
    pub review_on_pr_open: Option<Option<bool>>,
    pub review_on_push: Option<Option<bool>>,
    pub push_strategy: Option<Option<String>>,
    pub push_debounce_seconds: Option<Option<i32>>,
    pub dedup_scope: Option<Option<String>>,
}

impl RepoSettingsPatch {
    /// Whether this patch would change anything. The admin handler rejects an empty patch rather than
    /// silently writing a no-op row (mirroring `set_preset`'s empty-body 400).
    pub fn is_empty(&self) -> bool {
        self.check_run_reporting.is_none()
            && self.review_on_pr_open.is_none()
            && self.review_on_push.is_none()
            && self.push_strategy.is_none()
            && self.push_debounce_seconds.is_none()
            && self.dedup_scope.is_none()
    }
}

/// Apply a patch to a repo's overrides, inserting the row if absent. `set_by` is the admin's identity,
/// for audit.
///
/// Each column uses `COALESCE($n, column)`-style upsert semantics driven by a companion "touch this
/// field" boolean, so a field the caller didn't mention keeps its stored value while a field the
/// caller explicitly cleared goes back to NULL. Doing this in one statement (rather than read-then-
/// write) keeps it atomic against a concurrent admin write.
pub async fn set_repo_settings(
    pool: &PgPool,
    repository_id: i64,
    patch: &RepoSettingsPatch,
    set_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repo_settings ( \
             repository_id, check_run_reporting, review_on_pr_open, review_on_push, \
             push_strategy, push_debounce_seconds, dedup_scope, set_by \
         ) VALUES ($1, $2, $4, $6, $8, $10, $12, $14) \
         ON CONFLICT (repository_id) DO UPDATE SET \
             check_run_reporting = CASE WHEN $3  THEN $2  ELSE repo_settings.check_run_reporting END, \
             review_on_pr_open   = CASE WHEN $5  THEN $4  ELSE repo_settings.review_on_pr_open END, \
             review_on_push      = CASE WHEN $7  THEN $6  ELSE repo_settings.review_on_push END, \
             push_strategy       = CASE WHEN $9  THEN $8  ELSE repo_settings.push_strategy END, \
             push_debounce_seconds = CASE WHEN $11 THEN $10 ELSE repo_settings.push_debounce_seconds END, \
             dedup_scope         = CASE WHEN $13 THEN $12 ELSE repo_settings.dedup_scope END, \
             set_by = $14, updated_at = now()",
    )
    .bind(repository_id)
    .bind(patch.check_run_reporting.flatten())
    .bind(patch.check_run_reporting.is_some())
    .bind(patch.review_on_pr_open.flatten())
    .bind(patch.review_on_pr_open.is_some())
    .bind(patch.review_on_push.flatten())
    .bind(patch.review_on_push.is_some())
    .bind(patch.push_strategy.clone().flatten())
    .bind(patch.push_strategy.is_some())
    .bind(patch.push_debounce_seconds.flatten())
    .bind(patch.push_debounce_seconds.is_some())
    .bind(patch.dedup_scope.clone().flatten())
    .bind(patch.dedup_scope.is_some())
    .bind(set_by)
    .execute(pool)
    .await
    .map(|_| ())
}
