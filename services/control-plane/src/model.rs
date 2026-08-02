//! Per-identity (repo/org) model-override resolution + allowlist validation (ADR-0110, story #501).
//!
//! The follow-up ADR-0038's amendment promised: `repo_model_overrides` → `org_model_overrides` →
//! the global `LLM_MODEL` default the runner already falls back to — the same three-tier shape
//! [`crate::preset`] resolves presets with, reused deliberately. Deliberately scoped to repo + org
//! only (no per-user tier — this schema has no user-identity concept today, only
//! `repositories.installation_id`, the closest thing to an org key).
//!
//! Unlike preset resolution, this needs **no platform fetch** — both tables are first-party control-
//! plane state, so resolution is a plain DB read, safe to call from every task-creation call site
//! (including the A2A role, which holds no forge credentials at all).

use sqlx::PgPool;

/// Resolve the model override a new task should run under: `repo_model_overrides` (if set) → else
/// `org_model_overrides` keyed by the repo's `installation_id` (if set) → else `None` (the preset's
/// own configured model applies unchanged). Never fails a task creation: a lookup error degrades to
/// `None`, same as [`crate::settings::resolve_preset_and_settings`]'s error handling.
pub async fn resolve_model_override(
    pool: &PgPool,
    repository_id: i64,
    installation_id: i64,
) -> Option<String> {
    match crate::db::get_repo_model_override(pool, repository_id).await {
        Ok(Some(model)) => return Some(model),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                %error, repository_id,
                "repo model override lookup failed; falling back to the org/global default"
            );
        }
    }
    match crate::db::get_org_model_override(pool, installation_id).await {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                %error, installation_id,
                "org model override lookup failed; falling back to the global default"
            );
            None
        }
    }
}

/// Validate a model id against the operator-curated allowlist (ADR-0110's named write-time
/// prerequisite). `Err` names the allowlist so the caller can surface a clear 4xx — never a silent
/// downgrade or a broken review discovered only at run time. An **empty** allowlist means no
/// `model:` block has been configured at all: fail-closed (reject every model), not fail-open, since
/// an unenforced allowlist reintroduces exactly the "typo breaks every review" risk ADR-0038 was
/// written to prevent.
pub fn validate_model_allowlist(allowlist: &[String], model: &str) -> Result<(), String> {
    if allowlist.is_empty() {
        return Err(
            "no model allowlist is configured; an operator must set control-plane.json's \
             model.allowlist before any model override can be written"
                .to_string(),
        );
    }
    if allowlist.iter().any(|allowed| allowed == model) {
        return Ok(());
    }
    Err(format!(
        "model {model:?} is not in the configured allowlist: [{}]",
        allowlist.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::integrations::platform::Platform;

    #[test]
    fn empty_allowlist_rejects_every_model_fail_closed() {
        let err = validate_model_allowlist(&[], "gpt-5").unwrap_err();
        assert!(err.contains("no model allowlist is configured"));
    }

    #[test]
    fn model_in_the_allowlist_is_accepted() {
        let allowlist = vec!["gpt-5".to_string(), "claude-opus-5".to_string()];
        assert!(validate_model_allowlist(&allowlist, "claude-opus-5").is_ok());
    }

    #[test]
    fn model_outside_the_allowlist_is_rejected_naming_the_allowlist() {
        let allowlist = vec!["gpt-5".to_string(), "claude-opus-5".to_string()];
        let err = validate_model_allowlist(&allowlist, "gpt-4-typo").unwrap_err();
        assert!(err.contains("gpt-4-typo"));
        assert!(err.contains("gpt-5"));
        assert!(err.contains("claude-opus-5"));
    }

    #[sqlx::test]
    async fn neither_override_set_resolves_to_none(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            1,
            "octo",
            "repo",
            "main",
            Some(42),
        )
        .await
        .unwrap();
        assert_eq!(resolve_model_override(&pool, repo_id, 42).await, None);
    }

    #[sqlx::test]
    async fn org_only_override_applies_when_no_repo_override_is_set(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            2,
            "octo",
            "repo2",
            "main",
            Some(43),
        )
        .await
        .unwrap();
        crate::db::set_org_model_override(&pool, 43, "org-model", "admin@example.com")
            .await
            .unwrap();
        assert_eq!(
            resolve_model_override(&pool, repo_id, 43).await,
            Some("org-model".to_string())
        );
    }

    #[sqlx::test]
    async fn repo_override_wins_over_org_override(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            3,
            "octo",
            "repo3",
            "main",
            Some(44),
        )
        .await
        .unwrap();
        crate::db::set_org_model_override(&pool, 44, "org-model", "admin@example.com")
            .await
            .unwrap();
        crate::db::set_repo_model_override(&pool, repo_id, "repo-model", "admin@example.com")
            .await
            .unwrap();
        assert_eq!(
            resolve_model_override(&pool, repo_id, 44).await,
            Some("repo-model".to_string())
        );
    }

    #[sqlx::test]
    async fn clearing_the_repo_override_falls_back_to_the_org_override(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            4,
            "octo",
            "repo4",
            "main",
            Some(45),
        )
        .await
        .unwrap();
        crate::db::set_org_model_override(&pool, 45, "org-model", "admin@example.com")
            .await
            .unwrap();
        crate::db::set_repo_model_override(&pool, repo_id, "repo-model", "admin@example.com")
            .await
            .unwrap();
        let cleared = crate::db::clear_repo_model_override(&pool, repo_id)
            .await
            .unwrap();
        assert!(cleared);
        assert_eq!(
            resolve_model_override(&pool, repo_id, 45).await,
            Some("org-model".to_string())
        );
    }
}
