use crate::{AppState, jwt::Caller};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
pub struct LightbridgeMcpHandler {
    #[allow(dead_code)]
    state: AppState,
    quota: super::McpQuotaConfig,
    tool_router: ToolRouter<Self>,
}

impl LightbridgeMcpHandler {
    pub fn new(state: AppState, quota: super::McpQuotaConfig) -> Self {
        Self {
            state,
            quota,
            tool_router: Self::tool_router(),
        }
    }
}

pub(crate) fn caller_from_request_context(
    context: &RequestContext<RoleServer>,
) -> std::result::Result<Caller, ErrorData> {
    let parts = context
        .extensions
        .get::<axum::http::request::Parts>()
        .ok_or_else(|| ErrorData::internal_error("missing HTTP request context", None))?;

    let caller = parts
        .extensions
        .get::<Caller>()
        .ok_or_else(|| ErrorData::internal_error("missing caller context", None))?;

    Ok(caller.clone())
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct VectorSearchArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StartReviewArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetReviewStatusArgs {
    pub task_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GraphSearchArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub commit_sha: String,
    pub query_type: String,
    pub term: String,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetRepoSettingsArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListRecentReviewsArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub limit: Option<i64>,
}

#[tool_router]
impl LightbridgeMcpHandler {
    #[tool(
        name = "vector_search",
        description = "Search vector index across the repository"
    )]
    async fn vector_search_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<VectorSearchArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        Ok(format!(
            "Vector search executed on {}/{}/{} with query: {} (caller: {})",
            args.platform, args.org, args.repo, args.query, caller.claims.sub
        ))
    }

    #[tool(
        name = "start_review",
        description = "Start a deep code review on a pull request"
    )]
    async fn start_review_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<StartReviewArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("Database not available", None))?;

        let recent =
            crate::db::count_recent_mcp_runs(pool, &caller.claims.sub, self.quota.window_secs)
                .await
                .map_err(|e| {
                    ErrorData::internal_error("Database error", Some(e.to_string().into()))
                })?;

        if recent >= self.quota.max {
            return Err(ErrorData::invalid_params(
                "Per-identity deep-run quota exceeded",
                None,
            ));
        }

        super::tools::start_review(
            pool,
            &args.platform,
            &args.org,
            &args.repo,
            args.pr_number,
            &args.head_sha,
            args.prompt,
            &caller.claims.sub,
        )
        .await
    }

    #[tool(
        name = "get_review_status",
        description = "Get the status and findings of a review task"
    )]
    async fn get_review_status_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetReviewStatusArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("Database not available", None))?;

        let task_id = Uuid::parse_str(&args.task_id)
            .map_err(|_| ErrorData::invalid_params("Invalid task UUID format", None))?;

        let result = super::tools::get_review_status(pool, task_id).await?;
        Ok(result.to_string())
    }

    #[tool(
        name = "graph_search",
        description = "Query the structural code graph (find_symbol or get_callers)"
    )]
    async fn graph_search_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<GraphSearchArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("Database not available", None))?;

        let result = super::tools::graph_search(
            self.state.neo4j.as_ref(),
            pool,
            &args.platform,
            &args.org,
            &args.repo,
            &args.commit_sha,
            &args.query_type,
            &args.term,
            args.limit.unwrap_or(50),
        )
        .await?;
        Ok(result.to_string())
    }

    #[tool(
        name = "get_repository_settings",
        description = "Get the settings and presets for a repository"
    )]
    async fn get_repository_settings_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetRepoSettingsArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("Database not available", None))?;

        let result =
            super::tools::get_repository_settings(pool, &args.platform, &args.org, &args.repo)
                .await?;
        Ok(result.to_string())
    }

    #[tool(
        name = "list_recent_reviews",
        description = "List recent reviews for a repository"
    )]
    async fn list_recent_reviews_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListRecentReviewsArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        caller.require("repo:read").map_err(|_| {
            ErrorData::invalid_params("Missing required permission: repo:read", None)
        })?;

        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("Database not available", None))?;

        let result = super::tools::list_recent_reviews(
            pool,
            &args.platform,
            &args.org,
            &args.repo,
            args.limit.unwrap_or(10),
        )
        .await?;
        Ok(result.to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LightbridgeMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("MCP interface for Lightbridge Code Intelligence")
    }
}
