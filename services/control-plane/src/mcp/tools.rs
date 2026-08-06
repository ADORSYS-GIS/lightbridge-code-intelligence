use crate::integrations::neo4j::SymbolHit;
use crate::integrations::platform::Platform;
use crate::{
    db, model::resolve_model_override, preset::EntryPoint, settings::resolve_repo_settings_db_only,
};
use rmcp::{ErrorData, schemars};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

/// `start_review`'s result: the caller MUST capture `task_id` to poll `get_review_status` — that's
/// the only way an external client (which has no other view into this system's internal IDs) learns
/// it. Spelled out in `message` too, since a bare UUID with no field name gives an LLM client nothing
/// to recognize it by.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StartReviewResult {
    /// `uuid::Uuid` has no `JsonSchema` impl in this workspace's dependency set — stringified,
    /// matching how every tool's task-id *input* args already take it (e.g. `GetReviewStatusArgs`).
    pub task_id: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetReviewStatusResult {
    pub status: String,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub target_id: i64,
    pub created_at: String,
    pub review_url: Option<String>,
    pub summary: Option<String>,
    /// Structured findings as posted (severity/file/line/body per finding) — shape is the review
    /// pipeline's own, not re-typed here, since it already varies by finding kind.
    pub findings: Option<Value>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GraphSearchResult {
    pub query_type: String,
    pub results: Vec<SymbolHit>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetRepositorySettingsResult {
    pub check_run_reporting: bool,
    pub review_on_pr_open: bool,
    pub review_on_push: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecentReviewEntry {
    pub task_id: Option<String>,
    pub pr_number: Option<i64>,
    pub status: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListRecentReviewsResult {
    pub reviews: Vec<RecentReviewEntry>,
}

#[allow(clippy::too_many_arguments)]
pub async fn start_review(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    pr_number: i64,
    head_sha: &str,
    prompt: Option<String>,
    caller_id: &str,
    quota_max: i64,
    quota_window_secs: i64,
) -> Result<StartReviewResult, ErrorData> {
    let platform_enum = Platform::from_str(platform)
        .map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let repo = repo
        .ok_or_else(|| ErrorData::invalid_params("Repository not found or not connected", None))?;

    if repo.status != "approved" {
        return Err(ErrorData::invalid_params(
            "Repository is not approved",
            None,
        ));
    }

    let installation_id = repo.installation_id.ok_or_else(|| {
        ErrorData::invalid_params(
            "Repository is not fully provisioned (no installation)",
            None,
        )
    })?;

    let model_override = resolve_model_override(pool, repo.id, installation_id).await;
    let settings = resolve_repo_settings_db_only(pool, repo.id).await;

    // Use a unique delivery ID for MCP triggered reviews
    let task_uuid = Uuid::now_v7();
    let delivery_id = format!("mcp:{}", task_uuid);

    let new_task = db::NewTask {
        repository_id: repo.id,
        installation_id,
        webhook_delivery_id: delivery_id.clone(),
        target_type: "pull_request".to_string(),
        target_id: pr_number,
        command_text: prompt.unwrap_or_default(),
        base_sha: None,
        head_sha: Some(head_sha.to_string()),
        run_epoch: 0,
        preset: EntryPoint::Mcp.platform_default_preset().to_string(),
        entry_point: EntryPoint::Mcp.as_str().to_string(),
        trigger_comment_id: None,
        trace_context: None,
        model_override,
        check_runs_enabled: settings.check_run_reporting.value,
        run_after_secs: None,
    };

    let provenance = json!({
        "source": "mcp",
        "caller": caller_id,
        "repo": format!("{}/{}", org, repo_name),
        "pr": pr_number,
    });

    // Atomically check the per-identity quota AND record the delivery in one step (db::tasks::
    // reserve_mcp_run_slot) — a separate count-then-insert would race under concurrent calls from the
    // same caller.
    let reserved = db::reserve_mcp_run_slot(
        pool,
        caller_id,
        quota_window_secs,
        quota_max,
        platform_enum,
        &delivery_id,
        &provenance,
    )
    .await
    .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    if !reserved {
        return Err(ErrorData::invalid_params(
            "Per-identity deep-run quota exceeded",
            None,
        ));
    }

    let underlying = match db::create_task(pool, &new_task).await {
        Ok(Some(id)) => id,
        Ok(None) => db::find_task_id_by_idempotency(pool, &new_task)
            .await
            .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
            .ok_or_else(|| ErrorData::internal_error("Failed to dedup task", None))?,
        Err(e) => {
            return Err(ErrorData::internal_error(
                "Failed to create task",
                Some(e.to_string().into()),
            ));
        }
    };

    Ok(StartReviewResult {
        task_id: underlying.to_string(),
        status: "queued".to_string(),
        message: "Poll get_review_status with this task_id to check progress.".to_string(),
    })
}

pub async fn get_review_status(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<GetReviewStatusResult, ErrorData> {
    let task = db::get_task(pool, task_id)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Task not found", None))?;

    let review = db::get_review(pool, task_id)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let created_at = task
        .created_at
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| {
            ErrorData::internal_error("Timestamp formatting error", Some(e.to_string().into()))
        })?;

    Ok(GetReviewStatusResult {
        status: task.status,
        repo_owner: task.repo_owner,
        repo_name: task.repo_name,
        target_id: task.target_id,
        created_at,
        review_url: review.as_ref().and_then(|r| r.review_url.clone()),
        summary: review.as_ref().map(|r| r.summary.clone()),
        findings: review.map(|r| r.findings),
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn graph_search(
    neo4j: Option<&Arc<neo4rs::Graph>>,
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    commit_sha: &str,
    query_type: &str,
    term: &str,
    limit: i64,
) -> Result<GraphSearchResult, ErrorData> {
    let graph =
        neo4j.ok_or_else(|| ErrorData::internal_error("Neo4j graph not configured", None))?;

    let platform_enum = Platform::from_str(platform)
        .map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Repository not found", None))?;

    let results = match query_type {
        "find_symbol" => {
            crate::integrations::neo4j::find_symbol(graph, repo.id, commit_sha, term, limit)
                .await
                .map_err(|e| {
                    ErrorData::internal_error("Graph query failed", Some(e.to_string().into()))
                })?
        }
        "get_callers" => {
            crate::integrations::neo4j::get_callers(graph, repo.id, commit_sha, term, limit)
                .await
                .map_err(|e| {
                    ErrorData::internal_error("Graph query failed", Some(e.to_string().into()))
                })?
        }
        _ => {
            return Err(ErrorData::invalid_params(
                "Invalid query_type. Use 'find_symbol' or 'get_callers'",
                None,
            ));
        }
    };

    Ok(GraphSearchResult {
        query_type: query_type.to_string(),
        results,
    })
}

pub async fn get_repository_settings(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
) -> Result<GetRepositorySettingsResult, ErrorData> {
    let platform_enum = Platform::from_str(platform)
        .map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Repository not found", None))?;

    let settings = resolve_repo_settings_db_only(pool, repo.id).await;

    Ok(GetRepositorySettingsResult {
        check_run_reporting: settings.check_run_reporting.value,
        review_on_pr_open: settings.review_on_pr_open.value,
        review_on_push: settings.review_on_push.value,
    })
}

pub async fn list_recent_reviews(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    limit: i64,
) -> Result<ListRecentReviewsResult, ErrorData> {
    let platform_enum = Platform::from_str(platform)
        .map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Repository not found", None))?;

    let rows = sqlx::query(
        r#"
        SELECT id, target_id, status, created_at::text
        FROM tasks
        WHERE repository_id = $1 AND target_type = 'pull_request'
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(repo.id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let reviews = rows
        .into_iter()
        .map(|r| RecentReviewEntry {
            task_id: r.try_get::<Uuid, _>("id").ok().map(|id| id.to_string()),
            pr_number: r.try_get::<i64, _>("target_id").ok(),
            status: r.try_get::<String, _>("status").ok(),
            created_at: r.try_get::<String, _>("created_at").ok(),
        })
        .collect::<Vec<_>>();

    Ok(ListRecentReviewsResult { reviews })
}
