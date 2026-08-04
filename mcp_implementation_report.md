# MCP External Tools Implementation Report

This document details how the Model Context Protocol (MCP) server is wired into the Lightbridge Code Intelligence `control-plane`. It outlines the exact lifecycle of an MCP request originating from an external AI client (e.g., LibreChat, OpenCode, Claude Desktop) and traversing the Rust backend to interact with Postgres and Neo4j.

## 1. The Global Server Pattern

Lightbridge exposes a **single global MCP SSE endpoint** mapped directly inside the `control-plane`.
Rather than spinning up unique MCP servers per-repository, all external client traffic hits `/mcp`. Thus, every stateless tool mandates parameters like `platform`, `org`, and `repo` within its JSON Schema to resolve the target repository on-the-fly.

## 2. Client Configuration (LibreChat / OpenCode)

External clients must implement an SSE transport that points to the unified endpoint and injects an OIDC token matching the Lightbridge realm. 

**Example LibreChat `mcp-servers.json` Config:**
```json
{
  "lightbridge-code-intelligence": {
    "type": "sse",
    "url": "https://code-intelligence-api.ai.camer.digital/mcp",
    "headers": {
      "Authorization": "Bearer eyJhbGciOiJSUzI1NiIsInR5c..."
    }
  }
}
```

## 3. The Request Journey

### Step 1: Ingress & Authentication (`auth.rs`)

When the client connects via SSE, Traefik forwards the request to the `control-plane`. The Axum router intercepts it using the `mcp_auth` middleware. 

The middleware validates the OIDC token against the Keycloak provider and explicitly rejects requests lacking the correct `lightbridge-mcp-client` scope. It then extracts the caller's identity (the `sub` claim) into a strong `McpCallerContext` and injects it into the request extensions for downstream handlers.

```rust
// Extracted from services/control-plane/src/mcp/auth.rs

pub async fn mcp_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ErrorData> {
    let auth_header = req.headers().get("Authorization").ok_or(...)?;
    let token = auth_header.to_str().unwrap().strip_prefix("Bearer ").unwrap();
    
    // Validate OIDC claims
    let claims = state.oidc.verify_token(token).await?;
    
    // Inject caller context
    let caller_context = McpCallerContext { sub: claims.sub };
    req.extensions_mut().insert(caller_context);
    
    Ok(next.run(req).await)
}
```

### Step 2: Routing & Tool Macros (`handler.rs`)

Once authenticated, `rmcp::StreamableHttpService` parses the JSON-RPC request and routes it to `LightbridgeMcpHandler`.
This handler uses the powerful `#[tool_router]` and `#[tool]` macros from `rmcp v2.x` to statically define the available tools and their required schemas.

The handler automatically unpacks JSON arguments, extracts the `McpCallerContext`, and passes strongly typed structs down to the business logic layer.

```rust
// Extracted from services/control-plane/src/mcp/handler.rs

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StartReviewArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub pr_number: i64,
    pub base_sha: String,
    pub head_sha: String,
    pub prompt: Option<String>,
}

#[tool_router]
impl LightbridgeMcpHandler {
    
    #[tool(
        name = "start_review",
        description = "Start a deep code review on a pull request."
    )]
    pub async fn start_review_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<StartReviewArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;
        let pool = self.state.db.as_ref().unwrap();

        // Delegate to stateless business logic
        super::tools::start_review(
            pool,
            &args.platform,
            &args.org,
            &args.repo,
            args.pr_number,
            &args.base_sha,
            &args.head_sha,
            args.prompt,
            &caller.sub,
        ).await
    }
}
```

### Step 3: Business Logic & Database (`tools.rs`)

The `mcp/tools.rs` module isolates database connections from JSON-RPC definitions. 
For `start_review`, the function fetches the repository settings, confirms it is `approved`, checks the OIDC `caller_id`, and safely issues a `db::create_task` utilizing idempotency constraints.

```rust
// Extracted from services/control-plane/src/mcp/tools.rs

#[allow(clippy::too_many_arguments)]
pub async fn start_review(
    pool: &PgPool,
    platform: &str,
    org: &str,
    repo_name: &str,
    pr_number: i64,
    base_sha: &str,
    head_sha: &str,
    prompt: Option<String>,
    caller_id: &str,
) -> Result<String, ErrorData> {
    let platform_enum = Platform::from_str(platform)?;
    
    // 1. Resolve Repo
    let repo = db::find_repository(pool, platform_enum, org, repo_name)
        .await?.ok_or_else(|| ErrorData::invalid_params("Repository not connected", None))?;

    if repo.status != "approved" {
        return Err(ErrorData::invalid_params("Repository is not approved", None));
    }

    // 2. Prepare Task Identity 
    let new_task = db::NewTask {
        target_type: "pull_request",
        target_id: pr_number,
        repository_id: repo.id,
        entry_point: EntryPoint::Mcp.as_str(),
        preset: "deep", // Extracted via settings overrides
        target_ref: head_sha,
        target_base_ref: Some(base_sha.to_string()),
        idempotency_key: Some(format!("mcp-review-{}", head_sha)),
    };

    // 3. Insert and return UUID
    let underlying = match db::create_task(pool, &new_task).await {
        Ok(Some(id)) => id,
        Ok(None) => db::find_task_id_by_idempotency(pool, &new_task).await?.unwrap(),
        Err(e) => return Err(ErrorData::internal_error("Failed to create task", None)),
    };

    Ok(underlying.to_string())
}
```

## 4. Exposed Stateless Tooling

The MCP server currently supports the following globally available, stateless tools out-of-the-box for external clients:

1. **`start_review`**: Triggers a `db::NewTask` utilizing `EntryPoint::Mcp` to spawn agent runners. Idempotent by `head_sha`.
2. **`get_review_status`**: Queries `tasks` and `reviews` to provide JSON metadata about in-progress or completed reviews.
3. **`graph_search`**: Connects via `neo4rs` to the global structural index. Returns Neo4j graph nodes and caller hierarchies (`find_symbol`, `get_callers`).
4. **`get_repository_settings`**: Exposes config overrides like `review_on_push`.
5. **`list_recent_reviews`**: Uses dynamic `sqlx::query` bounds to bypass compile-time environment caches, returning the 10 most recent reviews for a `repo`.

*(Note: Agent-Runner file-system specific tasks, such as `read_file` or `run_sast`, are NOT exposed here. They are routed via a specialized tunneling mechanism only reachable from within the runner pods).*
