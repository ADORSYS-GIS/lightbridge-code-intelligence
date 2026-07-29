//! Per-identity model-override storage (ADR-0110, story #501): `repo_model_overrides` and
//! `org_model_overrides` are one-row-per-scope tables — set/clear/read only, no history. Resolution
//! precedence (repo → org → global default) lives in [`crate::model`], not here; this module is pure
//! persistence, mirroring the CRUD shape `repositories.rs` uses for the approval gate.

use sqlx::PgPool;

/// The repo-scoped model override, if one is set.
pub async fn get_repo_model_override(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT model FROM repo_model_overrides WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_optional(pool)
        .await
}

/// The org (installation)-scoped model override, if one is set.
pub async fn get_org_model_override(
    pool: &PgPool,
    installation_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT model FROM org_model_overrides WHERE installation_id = $1")
        .bind(installation_id)
        .fetch_optional(pool)
        .await
}

/// Set (insert or replace) a repository's model override. `set_by` is the admin's identity, for audit.
pub async fn set_repo_model_override(
    pool: &PgPool,
    repository_id: i64,
    model: &str,
    set_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repo_model_overrides (repository_id, model, set_by, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (repository_id) DO UPDATE \
           SET model = EXCLUDED.model, set_by = EXCLUDED.set_by, updated_at = now()",
    )
    .bind(repository_id)
    .bind(model)
    .bind(set_by)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Clear a repository's model override (reverts resolution to the org/global fallback). Returns
/// `true` if a row existed and was removed.
pub async fn clear_repo_model_override(
    pool: &PgPool,
    repository_id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM repo_model_overrides WHERE repository_id = $1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Set (insert or replace) an org (installation)'s model override.
pub async fn set_org_model_override(
    pool: &PgPool,
    installation_id: i64,
    model: &str,
    set_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO org_model_overrides (installation_id, model, set_by, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (installation_id) DO UPDATE \
           SET model = EXCLUDED.model, set_by = EXCLUDED.set_by, updated_at = now()",
    )
    .bind(installation_id)
    .bind(model)
    .bind(set_by)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Clear an org's model override. Returns `true` if a row existed and was removed.
pub async fn clear_org_model_override(
    pool: &PgPool,
    installation_id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM org_model_overrides WHERE installation_id = $1")
        .bind(installation_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
