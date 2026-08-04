use crate::{
    db,
    model::resolve_model_override,
    preset::EntryPoint,
    settings::resolve_repo_settings_db_only,
};
use rmcp::ErrorData;
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use crate::integrations::platform::Platform;
use std::str::FromStr;
use uuid::Uuid;

pub async fn start_review(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    pr_number: i64,
    head_sha: &str,
    prompt: Option<String>,
    caller_id: &str,
) -> Result<String, ErrorData> {
    let platform_enum = Platform::from_str(platform).map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let repo = repo.ok_or_else(|| ErrorData::invalid_params("Repository not found or not connected", None))?;

    if repo.status != "approved" {
        return Err(ErrorData::invalid_params("Repository is not approved", None));
    }

    let installation_id = repo.installation_id.ok_or_else(|| {
        ErrorData::invalid_params("Repository is not fully provisioned (no installation)", None)
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

    db::record_delivery(pool, platform_enum, &delivery_id, "mcp.review", &provenance)
        .await
        .map_err(|e| ErrorData::internal_error("Failed to record delivery", Some(e.to_string().into())))?;

    let underlying = match db::create_task(pool, &new_task).await {
        Ok(Some(id)) => id,
        Ok(None) => db::find_task_id_by_idempotency(pool, &new_task)
            .await
            .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
            .ok_or_else(|| ErrorData::internal_error("Failed to dedup task", None))?,
        Err(e) => return Err(ErrorData::internal_error("Failed to create task", Some(e.to_string().into()))),
    };

    Ok(underlying.to_string())
}

pub async fn get_review_status(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Value, ErrorData> {
    let task = db::get_task(pool, task_id)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Task not found", None))?;

    let review = db::get_review(pool, task_id)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let mut response = json!({
        "status": task.status,
        "repo_owner": task.repo_owner,
        "repo_name": task.repo_name,
        "target_id": task.target_id,
        "created_at": task.created_at,
    });

    if let Some(r) = review {
        response["review_url"] = json!(r.review_url);
        response["summary"] = json!(r.summary);
        response["findings"] = json!(r.findings);
    }

    Ok(response)
}

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
) -> Result<Value, ErrorData> {
    let graph = neo4j.ok_or_else(|| ErrorData::internal_error("Neo4j graph not configured", None))?;

    let platform_enum = Platform::from_str(platform).map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Repository not found", None))?;

    let results = match query_type {
        "find_symbol" => {
            let nodes = crate::integrations::neo4j::find_symbol(graph, repo.id, commit_sha, term, limit)
                .await
                .map_err(|e| ErrorData::internal_error("Graph query failed", Some(e.to_string().into())))?;
            json!(nodes)
        }
        "get_callers" => {
            // wait: get_callers signature might be different. Let's pass the term as a string for now, assuming it finds the node by term or id.
            // Actually, get_callers in neo4j.rs takes `node_id: &str`.
            let edges = crate::integrations::neo4j::get_callers(graph, repo.id, commit_sha, term, limit)
                .await
                .map_err(|e| ErrorData::internal_error("Graph query failed", Some(e.to_string().into())))?;
            json!(edges)
        }
        _ => return Err(ErrorData::invalid_params("Invalid query_type. Use 'find_symbol' or 'get_callers'", None)),
    };

    Ok(results)
}

pub async fn get_repository_settings(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
) -> Result<Value, ErrorData> {
    let platform_enum = Platform::from_str(platform).map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await
        .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?
        .ok_or_else(|| ErrorData::invalid_params("Repository not found", None))?;

    let settings = resolve_repo_settings_db_only(pool, repo.id).await;

    Ok(json!({
        "check_run_reporting": settings.check_run_reporting.value,
        "review_on_pr_open": settings.review_on_pr_open.value,
        "review_on_push": settings.review_on_push.value,
    }))
}

pub async fn list_recent_reviews(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    limit: i64,
) -> Result<Value, ErrorData> {
    let platform_enum = Platform::from_str(platform).map_err(|_| ErrorData::invalid_params("Invalid platform", None))?;
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
        "#
    )
    .bind(repo.id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ErrorData::internal_error("Database error", Some(e.to_string().into())))?;

    let recent = rows.into_iter().map(|r| {
        json!({
            "task_id": r.try_get::<Uuid, _>("id").ok(),
            "pr_number": r.try_get::<i64, _>("target_id").ok(),
            "status": r.try_get::<String, _>("status").ok(),
            "created_at": r.try_get::<String, _>("created_at").ok(),
        })
    }).collect::<Vec<_>>();

    Ok(json!(recent))
}
