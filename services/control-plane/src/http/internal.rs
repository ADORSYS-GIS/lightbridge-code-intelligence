//! Internal runner API — the control-plane side of the runner↔control-plane contract (ADR-0017).
//!
//! The dispatcher launches one Kubernetes Job per task (ADR-0004); that Job runs the agent runner,
//! which has no platform credentials of its own. Per the trust boundary (ADR-0002) the runner calls
//! back here to (a) fetch its task context plus a platform-appropriate clone URL + token, and (b)
//! report status transitions. These routes are **not** OIDC-protected (the caller is a pod, not a
//! user): they authenticate with a shared bearer (`AGENT_RUNNER_TOKEN`) the control plane injects
//! into the Job. Absent that token in this process, the routes fail closed (503) — never open.
//!
//! Platform handling (ADR-0072): GitHub mints a short-lived installation token and sends a plain
//! clone_url (the runner splices `x-access-token:<token>@`). GitLab embeds the token in the
//! clone_url itself (`oauth2:<token>@host`) and sends an empty token field.

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;
use crate::integrations::platform::{CodePlatform, Platform, RepoRef};

/// Authenticates a runner request by comparing its `Authorization: Bearer` token against the
/// configured `AGENT_RUNNER_TOKEN`, in constant time. A unit extractor: presence of the value is
/// the whole proof, so there is nothing to carry.
pub struct RunnerAuth;

/// Rejections for the internal API. 401 for a bad/missing token; 503 when the shared secret is not
/// configured in this process (so the surface is closed rather than unauthenticated).
pub enum RunnerAuthError {
    MissingToken,
    InvalidToken,
    Disabled,
}

impl IntoResponse for RunnerAuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            RunnerAuthError::MissingToken => (StatusCode::UNAUTHORIZED, "missing bearer token"),
            RunnerAuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "invalid runner token"),
            RunnerAuthError::Disabled => {
                (StatusCode::SERVICE_UNAVAILABLE, "runner api not configured")
            }
        };
        (status, msg).into_response()
    }
}

impl FromRequestParts<AppState> for RunnerAuth {
    type Rejection = RunnerAuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, RunnerAuthError> {
        let expected = state
            .runner_token
            .as_ref()
            .ok_or(RunnerAuthError::Disabled)?;
        let presented = bearer_token(parts).ok_or(RunnerAuthError::MissingToken)?;
        // Constant-time compare so a wrong token can't be recovered byte-by-byte via timing.
        if presented.as_bytes().ct_eq(expected.as_bytes()).into() {
            Ok(RunnerAuth)
        } else {
            Err(RunnerAuthError::InvalidToken)
        }
    }
}

fn bearer_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// The runner's view of a task: where the code is, how to fetch it, and what to do.
///
/// `clone_url` + `token` are platform-aware (ADR-0072):
/// - **GitHub**: `clone_url` is the plain HTTPS remote; `token` is a short-lived installation
///   access token (~1h) minted just-in-time. The runner composes the authenticated URL.
/// - **GitLab**: `clone_url` already has the token embedded (`oauth2:<token>@host`); `token`
///   is empty. The runner detects the pre-authenticated URL and passes it through.
#[derive(Serialize)]
pub struct TaskContextResponse {
    pub task_id: Uuid,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub clone_url: String,
    pub token: String,
    pub target_type: String,
    pub target_id: i64,
    pub command: String,
    /// Run kind (ADR-0033): `review` (diff-scoped findings, the default) or `ask` (a conversational
    /// answer posted as a single reply comment). The runner branches on this.
    pub kind: String,
    /// Review tier (ADR-0062): `fast` (automatic `pull_request opened` — SAST + one diff-only LLM turn,
    /// no retrieval) or `deep` (`@mention` — full retrieval, multi-turn). The runner shapes its loop on
    /// this. Defaults to `deep` (the full/safe behavior) for any task that didn't set it.
    pub tier: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// Whether the repo has a reusable semantic index — i.e. a latest indexed snapshot exists
    /// (ADR-0050). The runner skips the full re-index on a review when this is true and reuses that
    /// snapshot + the PR diff (ADR-0025); retrieval pins to the same commit (`task_scope`), so reuse
    /// never lands on zero search hits (the hollow-index trap, run `7c15f9bb`) and a new PR head no
    /// longer forces a full re-index.
    pub repo_indexed: bool,
    /// The agent's own prior review of this target, formatted as a context block (A, #137), present only
    /// for `review`-kind tasks on a target that already has an earlier posted review. The runner injects
    /// it into the prompt so a re-review reconciles with — rather than contradicts — its past output.
    /// `None` for the first review of a target, for `ask`/`index` runs, or if the lookup failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_reviews: Option<String>,
    /// Per-repo feedback memory (M1, ADR-0044): findings a human rejected (👎) on this repo, formatted
    /// as a "don't repeat these" context block. Present only for `review`-kind tasks when the repo has
    /// rejected findings. The runner injects it so the agent stops re-raising known false positives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_memory: Option<String>,
}

/// Custom Debug impl that redacts `clone_url` and `token` — for GitLab the clone URL embeds the
/// API token (`oauth2:<token>@host`), so a `tracing::debug!(?response)` would leak it (ADR-0072).
impl std::fmt::Debug for TaskContextResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskContextResponse")
            .field("task_id", &self.task_id)
            .field("repository_id", &self.repository_id)
            .field("owner", &self.owner)
            .field("name", &self.name)
            .field("default_branch", &self.default_branch)
            .field("clone_url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("target_type", &self.target_type)
            .field("target_id", &self.target_id)
            .field("command", &self.command)
            .field("kind", &self.kind)
            .field("tier", &self.tier)
            .field("base_sha", &self.base_sha)
            .field("head_sha", &self.head_sha)
            .field("repo_indexed", &self.repo_indexed)
            .field("prior_reviews", &self.prior_reviews)
            .field("repo_memory", &self.repo_memory)
            .finish()
    }
}

/// `GET /internal/tasks/{id}` — task context + a freshly-minted installation token for the runner.
pub async fn get_context(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let context = match crate::db::get_task_context(pool, id).await {
        Ok(Some(context)) => context,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "load task context failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };

    // Platform-aware clone URL + token (ADR-0072). GitHub mints a short-lived installation
    // token and sends a plain clone_url (the runner splices `x-access-token:<token>@`).
    // GitLab embeds the token in the clone_url itself (`oauth2:<token>@host`) and sends an
    // empty token field — the runner detects the `@` and passes the URL through unchanged.
    let repo_ref = RepoRef {
        platform: context.platform,
        full_name: format!("{}/{}", context.owner, context.name),
        platform_repo_id: 0,
        installation_id: context.installation_id,
    };

    let (clone_url, token) = match context.platform {
        Platform::GitHub => {
            let Some(app) = state.github.as_ref() else {
                return (StatusCode::SERVICE_UNAVAILABLE, "github app not configured")
                    .into_response();
            };
            let token = match app.installation_token(context.installation_id).await {
                Ok(token) => token,
                Err(error) => {
                    tracing::error!(%error, task_id = %id, "mint installation token failed");
                    return (StatusCode::BAD_GATEWAY, "could not mint installation token")
                        .into_response();
                }
            };
            (app.clone_url(&repo_ref), token)
        }
        Platform::GitLab => {
            let Some(gitlab) = state.gitlab.as_ref() else {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "gitlab client not configured",
                )
                    .into_response();
            };
            // GitlabClient::clone_url() embeds the token (oauth2:<token>@host).
            (gitlab.clone_url(&repo_ref), String::new())
        }
    };

    // Reuse the latest indexed snapshot if the repo has one (ADR-0050): a review skips the full
    // re-index and pins retrieval to that same commit (`task_scope`), so the skip decision and the
    // search scope reference a commit that provably has chunks — no hollow index, and no per-PR
    // re-index just because the PR head isn't indexed. A missing/failed lookup degrades to "not
    // indexed" (fail safe → the runner indexes), so a transient DB hiccup just re-indexes.
    let repo_indexed = crate::db::latest_indexed_commit(pool, context.repository_id)
        .await
        .unwrap_or(None)
        .is_some();

    // Prior-review context (ADR-0040 + ADR-0065): on a re-review, feed the agent ALL its prior reviews of
    // this target so it re-derives-then-reconciles instead of anchoring on a single verdict. Only for
    // `review` kind (an `ask` reply or an `index` run has nothing to reconcile). Best-effort: a lookup
    // error degrades to a blind re-review (the old behavior), never a failed task. The DB returns the
    // reviews newest-first, each carrying its TRUE chronological ordinal (computed by a window function
    // over the full prior set BEFORE the fetch cap, so "review #1" is always the first review ever posted
    // on this target and the labels never shift between runs once a PR exceeds the cap).
    let prior_reviews = if context.kind == "review" {
        match crate::db::all_prior_reviews_for_target(
            pool,
            context.repository_id,
            &context.target_type,
            context.target_id,
            context.id,
        )
        .await
        {
            Ok(rows) if !rows.is_empty() => {
                let priors: Vec<crate::review::PriorReview> = rows
                    .into_iter()
                    .map(|(ordinal, summary, findings)| crate::review::PriorReview {
                        ordinal: ordinal.max(1) as usize,
                        summary,
                        findings,
                    })
                    .collect();
                crate::review::format_prior_reviews(&priors)
            }
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, task_id = %id, "prior-review lookup failed (non-fatal)");
                None
            }
        }
    } else {
        None
    };

    // Feedback memory (M1, ADR-0044): rejected-finding memory for this repo, so the agent doesn't
    // re-raise known false positives. `review` kind only; best-effort (a lookup error degrades to no
    // memory, never a failed task). Cap the list so the prompt stays bounded.
    let repo_memory = if context.kind == "review" {
        match crate::db::rejected_findings_for_repo(pool, context.repository_id, 30).await {
            Ok(rejected) => crate::review::format_repo_memory(&rejected),
            Err(error) => {
                tracing::warn!(%error, task_id = %id, "repo-memory lookup failed (non-fatal)");
                None
            }
        }
    } else {
        None
    };

    Json(TaskContextResponse {
        task_id: context.id,
        repository_id: context.repository_id,
        clone_url,
        owner: context.owner,
        name: context.name,
        default_branch: context.default_branch,
        token,
        target_type: context.target_type,
        target_id: context.target_id,
        command: context.command_text,
        kind: context.kind,
        tier: context.tier,
        base_sha: context.base_sha,
        head_sha: context.head_sha,
        repo_indexed,
        prior_reviews,
        repo_memory,
    })
    .into_response()
}

/// One chunk submitted by the indexer runner.
#[derive(Debug, Deserialize)]
pub struct ChunkInput {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    pub embedding: Vec<f32>,
}

/// Body for `POST /internal/tasks/{id}/chunks`.
#[derive(Debug, Deserialize)]
pub struct ChunkBatch {
    pub commit_sha: String,
    pub chunks: Vec<ChunkInput>,
}

/// Body for `POST /internal/tasks/{id}/transcript` — the agent run transcript (ADR-0034).
#[derive(Debug, Deserialize)]
pub struct TranscriptSubmission {
    pub entries: Vec<crate::db::TranscriptInput>,
}

/// `POST /internal/tasks/{id}/transcript` — store the agent run transcript (ADR-0034). Replaces any
/// prior transcript for the task (a retry re-submits the whole thing).
pub async fn ingest_transcript(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(submission): Json<TranscriptSubmission>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    // Resolve the task first so an unknown id is a clean 404 rather than a foreign-key 500 on insert
    // (mirrors `ingest_chunks`/`ingest_graph`).
    match sqlx::query_scalar::<_, Uuid>("SELECT id FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "load task for transcript failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    }
    match crate::db::replace_transcript(pool, id, &submission.entries).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "storing transcript failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "store error").into_response()
        }
    }
}

/// Body for `POST /internal/tasks/{id}/review/telemetry` — run-level review telemetry submitted at run
/// START (extends ADR-0034/0017/0060). `tools` is the exact set OFFERED to the model this run (per-tier
/// allowlist ADR-0062 + MCP-discovered tools ADR-0066), each `{name, source}`; `config_b64` is the
/// resolved `ReviewConfig` serialized to JSON, **redacted by the runner** (api_key etc. → "[REDACTED]"),
/// then base64-encoded. The control plane stores both verbatim — it does NOT decode or re-redact.
#[derive(Debug, Deserialize)]
pub struct ReviewRunTelemetry {
    pub tools: serde_json::Value,
    pub config_b64: String,
}

/// `POST /internal/tasks/{id}/review/telemetry` — record the offered tools + redacted base64 config for
/// a review run (ADR-0034/0062/0066). Submitted at run start so a crashed/aborted run still has its
/// config recorded. One task = one run, so this UPDATEs the task row in place (latest-run-replace).
/// Indexing runs never call this; their columns stay NULL.
pub async fn record_review_telemetry(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(telemetry): Json<ReviewRunTelemetry>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    // No existence pre-check: unlike the transcript's per-row INSERTs (where an unknown id would be a
    // foreign-key 500), this is a single UPDATE — rows_affected == 0 IS the clean 404 signal, one
    // round-trip total (gemini review on #270).
    match crate::db::record_review_run_telemetry(pool, id, &telemetry.tools, &telemetry.config_b64)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "storing review telemetry failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "store error").into_response()
        }
    }
}

// ── ADR-0087 durable-step journal (the `CheckpointRuntime` replay store) ─────────────────────────
// The agent, running under `CheckpointRuntime`, journals each step's RESULT here through this
// mediated API (it holds no DB credential — ADR-0002/0037). `run_epoch` is resolved server-side from
// the task row: the agent supplies only `(task_id, step_name)`, so it can neither know nor spoof the
// run identity. Additive + prod-neutral: unused while the agent runs `Passthrough` (the default).

/// Body for `POST /internal/tasks/{id}/steps/upsert` — one journaled step result (ADR-0087).
#[derive(Debug, Deserialize)]
pub struct UpsertStepBody {
    /// The stability-tested step name (`llm_turn:{n}` / `tools:{n}` / `tool:{n}:{id}`).
    pub step_name: String,
    /// The step's serialized result — stored verbatim as `jsonb`.
    pub result: serde_json::Value,
    /// Content hash of `result` (ADR-0087 C3), so replay can verify the rehydrated bytes.
    pub content_hash: String,
}

/// Body for `POST /internal/tasks/{id}/steps/fetch`.
#[derive(Debug, Deserialize)]
pub struct FetchStepBody {
    pub step_name: String,
}

/// A journaled step result, returned by `fetch_step` on a hit.
#[derive(Debug, Serialize)]
pub struct StoredStepResponse {
    pub result: serde_json::Value,
    pub content_hash: String,
}

/// `POST /internal/tasks/{id}/steps/upsert` — journal one step result (replay-idempotent on the
/// `(task_id, run_epoch, step_name)` key). 404 if the task is unknown.
pub async fn upsert_step(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpsertStepBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let run_epoch = match crate::db::durable_step_run_epoch(pool, id).await {
        Ok(Some(epoch)) => epoch,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "resolving run_epoch for durable step failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    // Store the result as its serialized text, cast to `jsonb` in-SQL (no extra sqlx feature needed).
    let result_json = body.result.to_string();
    match crate::db::upsert_durable_step(
        pool,
        id,
        run_epoch,
        &body.step_name,
        &result_json,
        &body.content_hash,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, step = %body.step_name, "upserting durable step failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "store error").into_response()
        }
    }
}

/// `POST /internal/tasks/{id}/steps/fetch` — read one journaled step result. 200 with the result on a
/// hit; 404 when the step has not run yet (the replay gap where the loop continues live) or the task
/// is unknown.
pub async fn fetch_step(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<FetchStepBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let run_epoch = match crate::db::durable_step_run_epoch(pool, id).await {
        Ok(Some(epoch)) => epoch,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "resolving run_epoch for durable step failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    match crate::db::fetch_durable_step(pool, id, run_epoch, &body.step_name).await {
        Ok(Some(row)) => {
            // `result::text` round-trips through serde_json; a NULL result (future offload) is `null`.
            let result = row
                .result
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(serde_json::Value::Null);
            Json(StoredStepResponse {
                result,
                content_hash: row.content_hash,
            })
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "step not journaled").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, step = %body.step_name, "fetching durable step failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `POST /internal/tasks/{id}/chunks` — ingest indexed code chunks from the runner.
///
/// The runner submits chunks in batches as it processes files; the control plane writes them to
/// `code_chunks` (pgvector). The task's `repository_id` is read from the DB — the runner cannot
/// supply it (trust boundary, ADR-0002).
pub async fn ingest_chunks(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(batch): Json<ChunkBatch>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };

    let repository_id: Option<i64> =
        match sqlx::query_scalar("SELECT repository_id FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::error!(%error, task_id = %id, "load task for chunk ingest failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
            }
        };

    let Some(repository_id) = repository_id else {
        return (StatusCode::NOT_FOUND, "task not found").into_response();
    };

    if batch.chunks.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let chunks: Vec<crate::db::CodeChunk> = batch
        .chunks
        .into_iter()
        .map(|c| crate::db::CodeChunk {
            file_path: c.file_path,
            language: c.language,
            chunk_type: c.chunk_type,
            symbol_name: c.symbol_name,
            start_line: c.start_line,
            end_line: c.end_line,
            content: c.content,
            embedding: c.embedding,
        })
        .collect();

    match crate::db::upsert_code_chunks(pool, repository_id, &batch.commit_sha, &chunks).await {
        Ok(count) => {
            tracing::info!(task_id = %id, chunk_count = count, "chunks ingested");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            tracing::error!(%error, task_id = %id, "chunk upsert failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "upsert error").into_response()
        }
    }
}

/// One structural-graph node submitted by the runner (a Graphify `graph.json` node).
#[derive(Debug, Deserialize)]
pub struct GraphNodeInput {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge (`contains` / `method` / `calls` / …).
#[derive(Debug, Deserialize)]
pub struct GraphEdgeInput {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Body for `POST /internal/tasks/{id}/graph`.
#[derive(Debug, Deserialize)]
pub struct GraphBatch {
    pub commit_sha: String,
    pub nodes: Vec<GraphNodeInput>,
    pub edges: Vec<GraphEdgeInput>,
}

/// `POST /internal/tasks/{id}/graph` — ingest the structural code graph (Graphify → Neo4j, ADR-0019).
///
/// The runner spawns Graphify, reads its `graph.json`, and POSTs nodes+edges here; the control plane
/// writes them to Neo4j. `repository_id` is read from the DB, not trusted from the caller (ADR-0002).
pub async fn ingest_graph(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(batch): Json<GraphBatch>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let Some(neo4j) = state.neo4j.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "neo4j not configured").into_response();
    };

    let repository_id: Option<i64> =
        match sqlx::query_scalar("SELECT repository_id FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::error!(%error, task_id = %id, "load task for graph ingest failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
            }
        };

    let Some(repository_id) = repository_id else {
        return (StatusCode::NOT_FOUND, "task not found").into_response();
    };

    if batch.nodes.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let nodes: Vec<crate::integrations::neo4j::GraphNode> = batch
        .nodes
        .into_iter()
        .map(|n| crate::integrations::neo4j::GraphNode {
            node_id: n.node_id,
            label: n.label,
            source_file: n.source_file,
            start_line: n.start_line,
        })
        .collect();
    let edges: Vec<crate::integrations::neo4j::GraphEdge> = batch
        .edges
        .into_iter()
        .map(|e| crate::integrations::neo4j::GraphEdge {
            source: e.source,
            target: e.target,
            relation: e.relation,
        })
        .collect();

    match crate::integrations::neo4j::upsert_graph(
        neo4j,
        repository_id,
        &batch.commit_sha,
        &nodes,
        &edges,
    )
    .await
    {
        Ok((n, e)) => {
            tracing::info!(task_id = %id, nodes = n, edges = e, "graph ingested");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            tracing::error!(%error, task_id = %id, "graph upsert failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "upsert error").into_response()
        }
    }
}

/// Fallback retrieval commit for a repo that has **never** been indexed: the head SHA, else the default
/// branch. Used only when [`crate::db::latest_indexed_commit`] is `None` — once any snapshot exists,
/// retrieval pins to *that* (the latest indexed commit), which is the commit that provably has chunks.
fn retrieval_commit(head_sha: Option<&str>, default_branch: &str) -> String {
    head_sha.unwrap_or(default_branch).to_string()
}

/// Resolve a task's `(repository_id, commit_sha)` — the scope every retrieval query is pinned to
/// (ADR-0050). The commit is the repo's **latest indexed snapshot** so a search always references a
/// commit that has chunks (no hollow index); it falls back to the head/default only for a repo with no
/// index yet. Single source of truth with [`get_context`]'s skip decision, which checks the same
/// `latest_indexed_commit`. Returns `None` for an unknown task; the caller never supplies the scope, so
/// a task can only read its own repo.
async fn task_scope(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<(i64, String)>, sqlx::Error> {
    let Some(ctx) = crate::db::get_task_context(pool, id).await? else {
        return Ok(None);
    };
    let commit = match crate::db::latest_indexed_commit(pool, ctx.repository_id).await? {
        Some(c) => c,
        None => retrieval_commit(ctx.head_sha.as_deref(), &ctx.default_branch),
    };
    Ok(Some((ctx.repository_id, commit)))
}

/// Clamp a caller-supplied limit into a sane range (default 10, max 100).
fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(10).clamp(1, 100)
}

/// Body for `POST /internal/tasks/{id}/search` — the query already embedded by the caller (the
/// vector MCP server embeds the text with the runner's embeddings key; the control plane holds none).
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /internal/tasks/{id}/search` — semantic search over the task's pgvector index.
pub async fn search(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SearchRequest>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    if req.embedding.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty embedding").into_response();
    }
    let scope = match task_scope(pool, id).await {
        Ok(Some(scope)) => scope,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "search scope lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    let (repository_id, commit) = scope;
    match crate::db::search_code_chunks(
        pool,
        repository_id,
        &commit,
        &req.embedding,
        clamp_limit(req.limit),
    )
    .await
    {
        Ok(hits) => Json(hits).into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "semantic search failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "search error").into_response()
        }
    }
}

/// Body for `POST /internal/tasks/{id}/graph/query` — a small fixed op set over the Neo4j graph.
#[derive(Debug, Deserialize)]
pub struct GraphQueryRequest {
    /// `find_symbol` (needs `term`) or `get_callers` (needs `node_id`).
    pub op: String,
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /internal/tasks/{id}/graph/query` — structural queries over the task's Neo4j graph.
pub async fn graph_query(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<GraphQueryRequest>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let Some(neo4j) = state.neo4j.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "neo4j not configured").into_response();
    };
    let scope = match task_scope(pool, id).await {
        Ok(Some(scope)) => scope,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "graph-query scope lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    let (repository_id, commit) = scope;
    let limit = clamp_limit(req.limit);

    let result = match req.op.as_str() {
        "find_symbol" => {
            let Some(term) = req.term.as_deref() else {
                return (StatusCode::BAD_REQUEST, "find_symbol requires `term`").into_response();
            };
            crate::integrations::neo4j::find_symbol(neo4j, repository_id, &commit, term, limit)
                .await
        }
        "get_callers" => {
            let Some(node_id) = req.node_id.as_deref() else {
                return (StatusCode::BAD_REQUEST, "get_callers requires `node_id`").into_response();
            };
            crate::integrations::neo4j::get_callers(neo4j, repository_id, &commit, node_id, limit)
                .await
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unsupported op {other:?} (expected: find_symbol | get_callers)"),
            )
                .into_response();
        }
    };

    match result {
        Ok(hits) => Json(hits).into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, op = %req.op, "graph query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "graph query error").into_response()
        }
    }
}

// ── ADR-0066 external-knowledge MCP tools ───────────────────────────────────────────────────────
// A single dynamically-backed mediated tool (`mcp_tools`) — one endpoint discovers whatever tools
// the configured MCP servers (`knowledge_tools.mcp_servers`) currently expose, one endpoint
// dispatches a call to whichever server owns it. Adding a new server (brave-search, context7, or
// anything else) is a config change, not a code change: no per-provider Rust handler, no hardcoded
// tool schema. Available to any tier — gating is purely the normal per-tier `review.tools`
// allowlist, the same mechanism every other mediated tool uses, not a tier check here. The model
// supplies a discovered tool name + arguments, never a URL, so there is no SSRF primitive.

/// How long the control plane waits on an upstream MCP server before giving up.
const KNOWLEDGE_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Every discovered tool's exposed name carries this prefix: `mcp__<server>__<tool>`. Namespaces
/// names across servers (so two servers can't collide) and lets `call_knowledge_tool` route a call
/// back to the right server without a separate lookup table.
const MCP_TOOL_PREFIX: &str = "mcp__";

/// One discovered tool, as returned to the agent-runner to fold into its live tool schema.
#[derive(Debug, Serialize)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// `GET /internal/tasks/{id}/knowledge/tools` — discover every tool every configured MCP server
/// currently exposes. Best-effort per server: one unreachable/misbehaving server is logged and
/// skipped rather than failing the whole discovery (a partial tool set beats none). Not tier-gated
/// (discovery alone performs no provider-billed action); the runner's per-tier allowlist decides
/// whether to call this at all.
pub async fn list_knowledge_tools(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    // Concurrent, not sequential: N configured servers shouldn't cost up to N × the per-server
    // timeout just to discover tools before the review has even started.
    let per_server = state
        .knowledge_tools
        .mcp_servers
        .iter()
        .map(|server| async move {
            let result = crate::mcp_client::list_tools(&server.url, KNOWLEDGE_TOOL_TIMEOUT).await;
            (server, result)
        });
    let results = futures::future::join_all(per_server).await;

    let mut discovered = Vec::new();
    for (server, result) in results {
        match result {
            Ok(tools) => discovered.extend(tools.into_iter().map(|t| DiscoveredTool {
                name: format!("{MCP_TOOL_PREFIX}{}__{}", server.name, t.name),
                description: t.description,
                input_schema: t.input_schema,
            })),
            Err(error) => {
                tracing::warn!(%error, task_id = %id, server = %server.name, "MCP tool discovery failed; skipping this server");
            }
        }
    }
    Json(discovered).into_response()
}

/// Body for `POST /internal/tasks/{id}/knowledge/call`.
#[derive(Debug, Deserialize)]
pub struct KnowledgeToolCallRequest {
    /// The prefixed name from `list_knowledge_tools` (`mcp__<server>__<tool>`).
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// `POST /internal/tasks/{id}/knowledge/call` — dispatch a previously-discovered tool call to its
/// owning MCP server, keyed by the `mcp__<server>__<tool>` prefix.
pub async fn call_knowledge_tool(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<KnowledgeToolCallRequest>,
) -> Response {
    let Some((server_name, tool_name)) = parse_knowledge_tool_name(&req.tool) else {
        crate::http::metrics::knowledge_tool_call("unknown", "invalid_request");
        return (
            StatusCode::BAD_REQUEST,
            format!("not a valid mcp__<server>__<tool> name: {:?}", req.tool),
        )
            .into_response();
    };
    let Some(server) = state
        .knowledge_tools
        .mcp_servers
        .iter()
        .find(|s| s.name == server_name)
    else {
        crate::http::metrics::knowledge_tool_call(server_name, "unknown_tool");
        return (
            StatusCode::NOT_FOUND,
            format!("no configured MCP server named {server_name:?}"),
        )
            .into_response();
    };
    match crate::mcp_client::call_tool(
        &server.url,
        tool_name,
        req.arguments,
        KNOWLEDGE_TOOL_TIMEOUT,
    )
    .await
    {
        Ok(text) => {
            crate::http::metrics::knowledge_tool_call(&server.name, "ok");
            Json(serde_json::json!({ "text": text })).into_response()
        }
        Err(error) => {
            tracing::warn!(%error, task_id = %id, tool = %req.tool, "MCP tool call failed");
            crate::http::metrics::knowledge_tool_call(&server.name, "error");
            (
                StatusCode::BAD_GATEWAY,
                format!("{server_name} upstream error"),
            )
                .into_response()
        }
    }
}

/// Split `mcp__<server>__<tool>` into `(server, tool)`. `server`/`tool` may not themselves contain
/// `__` (the config comment on [`crate::config::McpServerConfig::name`] asks for that), so the
/// first `__` after the prefix is the unambiguous split point.
fn parse_knowledge_tool_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(MCP_TOOL_PREFIX)?.split_once("__")
}

// ── ADR-0037 mediated write actions ─────────────────────────────────────────────────────────────
// The native agent calls these *during* its run; the control plane accumulates them and posts nothing
// until `finalize_review` flushes the buffer as one grouped review (+ a single consolidated reply).
// Per-call diff validation is done runner-side (it holds the diff); the flush re-validates here
// authoritatively via `crate::review::validate`.

/// Default summary for a run that produced no findings (and the empty-run backstop). Persisted to the
/// `reviews` row so prior-review context + the console always have a verdict, even when ADR-0068
/// suppresses the GitHub post (the 👍 reaction is the whole GitHub response).
const DEFAULT_CLEAN_SUMMARY: &str = "No issues found — the change looks good.";

/// GitHub reaction contents for the ADR-0068 verdict: 👍 (`+1`) on a clean pass, 👎 (`-1`) when findings
/// were posted. (GitHub's reaction set has no ❌; 👎 is the agreed stand-in for "changes requested".)
const REACTION_CLEAN: &str = "+1";
const REACTION_FINDINGS: &str = "-1";

/// Body for `POST /internal/tasks/{id}/review/inline` (`add_review_comment`).
#[derive(Debug, Deserialize)]
pub struct InlineActionBody {
    pub file: String,
    pub line: i32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub suggestion: Option<String>,
    pub body: String,
}

/// Body for `POST /internal/tasks/{id}/review/inline/retract` (`retract_finding`, Phase 2 ADR-0043).
#[derive(Debug, Deserialize)]
pub struct RetractInlineBody {
    pub file: String,
    pub line: i32,
}

/// `POST /internal/tasks/{id}/review/inline/retract` — drop a buffered inline finding by `(file, line)`
/// (Phase 2, ADR-0043): the refute pass removing a P0/P1 that didn't hold, before it is posted.
pub async fn retract_inline(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(a): Json<RetractInlineBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::delete_pending_inline(pool, id, &a.file, a.line).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "retracting inline finding failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "retract error").into_response()
        }
    }
}

/// `POST /internal/tasks/{id}/review/inline/clear` — drop ALL buffered inline findings (no body). Used
/// on an `abort` so an incomplete run posts only its note, not its half-baked findings.
pub async fn clear_inline(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::clear_pending_action(pool, id, "inline").await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "clearing inline findings failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "clear error").into_response()
        }
    }
}

/// Body for `POST /internal/tasks/{id}/review/comment` (`add_comment`) and
/// `POST /internal/tasks/{id}/review/summary` (`set_summary`).
#[derive(Debug, Deserialize)]
pub struct TextActionBody {
    pub body: String,
}

/// `POST /internal/tasks/{id}/review/inline` — buffer one inline finding (ADR-0037). Last write wins
/// per `(file, line)`; nothing is posted until [`finalize_review`].
pub async fn add_review_comment(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(a): Json<InlineActionBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::upsert_pending_inline(
        pool,
        id,
        &a.file,
        a.line,
        a.title.as_deref(),
        a.priority.as_deref(),
        a.category.as_deref(),
        a.suggestion.as_deref(),
        &a.body,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "buffering inline finding failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "buffer error").into_response()
        }
    }
}

/// `POST /internal/tasks/{id}/review/comment` — buffer one plain reply (`add_comment`, ADR-0037).
pub async fn add_review_reply(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(a): Json<TextActionBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::add_pending_comment(pool, id, &a.body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "buffering comment failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "buffer error").into_response()
        }
    }
}

/// `POST /internal/tasks/{id}/review/summary` — set the run's summary/verdict (`set_summary`).
pub async fn set_review_summary(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(a): Json<TextActionBody>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::upsert_pending_summary(pool, id, &a.body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "buffering summary failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "buffer error").into_response()
        }
    }
}

/// Whether a finalize run posts a PR review/verdict — inline findings, a verdict summary, or the
/// empty-buffer backstop (a default "no issues" review). This is the ADR-0056 policy gate: when a PR
/// review is going out, the verdict belongs solely in the grouped review, so the agent's buffered
/// `add_comment` narration is dropped. Crucially it is NOT keyed on finding count — a clean review (a
/// summary with zero findings) is still a review (regression: docs PR #224, where add_comment
/// verification narration leaked as a "Lightbridge answer" issue comment). Pure, so the policy is
/// unit-tested independently of the DB/outbox.
fn posts_pr_review(
    target_type: &str,
    has_inline: bool,
    has_summary: bool,
    buffer_empty: bool,
) -> bool {
    target_type == "pull_request" && (has_inline || has_summary || buffer_empty)
}

/// Optional finalize request body: the runner-reported run outcome (ADR-0068). `finished` = the agent
/// called `finish` (a trustworthy verdict); `exhausted` = it ran out of turn budget; `aborted` = it
/// couldn't complete (findings cleared, an honest note buffered as the summary). Absent/unknown (e.g. an
/// older runner mid-rolling-deploy) is treated as "not provably clean".
#[derive(Debug, Default, Deserialize)]
pub struct FinalizeBody {
    #[serde(default)]
    pub outcome: Option<String>,
}

/// How a PR-review finalize responds on GitHub (ADR-0068), derived from the run outcome, the finding
/// count, and the reactions toggle. Pure, so the whole matrix is unit-tested without the DB/outbox.
#[derive(Debug, PartialEq, Eq)]
struct FinalizePolicy {
    /// Suppress the review post entirely — the silent clean pass, where 👍 is the whole response.
    suppress_clean_post: bool,
    /// The verdict reaction to enqueue (`+1` clean / `-1` findings), if any.
    verdict: Option<&'static str>,
    /// 😕 for an aborted run: it posts an honest "couldn't complete" note, not a verdict.
    react_confused: bool,
}

/// ADR-0068 policy: suppress-and-👍 ONLY on an explicitly clean finish (`outcome == "finished"`, zero
/// findings) while reactions are enabled. Everything else fails OPEN to the old visible behavior — an
/// aborted run posts its honest note (+😕, no verdict: its summary is an apology, not a verdict), an
/// exhausted zero-findings run posts its budget note (no verdict: the pass was incomplete, "clean so
/// far" must not read as "clean"), and a missing/unknown outcome posts. With reactions disabled nothing
/// is suppressed (the 👍 could never compensate) and no reaction is enqueued.
fn finalize_policy(
    outcome: Option<&str>,
    has_findings: bool,
    reactions_enabled: bool,
) -> FinalizePolicy {
    let finished = outcome == Some("finished");
    let aborted = outcome == Some("aborted");
    FinalizePolicy {
        suppress_clean_post: finished && !has_findings && reactions_enabled,
        verdict: if !reactions_enabled || aborted {
            None
        } else if has_findings {
            Some(REACTION_FINDINGS)
        } else if finished {
            Some(REACTION_CLEAN)
        } else {
            None // exhausted/unknown with zero findings: incomplete, so no verdict either way
        },
        react_confused: reactions_enabled && aborted,
    }
}

/// The summary that is posted AND persisted for a review (pure, unit-tested). The model's own verdict
/// always wins. Without one, an **all-deduped** run (ADR-0065: every finding it re-derived was already
/// posted on this commit, `deduped_n > 0`, nothing kept) gets a truthful "no NEW findings" note — never
/// [`DEFAULT_CLEAN_SUMMARY`], which would misrepresent (and persist, poisoning later prior-review
/// context) a "found the same issues again" run as clean. A genuinely clean run keeps the default.
fn effective_summary(real_summary: Option<&str>, deduped_n: usize, all_deduped: bool) -> String {
    match real_summary {
        Some(s) => s.to_string(),
        None if all_deduped => {
            format!("No new findings — {deduped_n} prior finding(s) on this commit still stand.")
        }
        None => DEFAULT_CLEAN_SUMMARY.to_string(),
    }
}

/// `POST /internal/tasks/{id}/review/finalize` — flush the accumulated buffer (ADR-0037). Posts the
/// inline findings + summary as **one grouped PR review** (re-validated against the diff here, the
/// authority), consolidates buffered replies into **one** thread comment, records the emergent run
/// kind, and clears the buffer. An **explicitly clean** pass (`outcome: "finished"`, zero findings)
/// posts NO review — the 👍 verdict reaction is the whole GitHub response (ADR-0068) — but still
/// persists the review row; aborted/exhausted/unknown outcomes post as before. The buffer is cleared at
/// the end regardless, so a finished run can't re-post on a stray retry.
pub async fn finalize_review(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> Response {
    // Lenient body parse: the outcome is advisory. An empty body (an older runner) or junk JSON reads
    // as "no outcome" → fail open to posting (never a silent 👍 on an unproven run).
    let outcome: Option<String> = serde_json::from_slice::<FinalizeBody>(&body)
        .ok()
        .and_then(|b| b.outcome);
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let context = match crate::db::get_task_context(pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "load task for finalize failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    let pending = match crate::db::load_pending_review(pool, id).await {
        Ok(p) => p,
        Err(error) => {
            tracing::error!(%error, task_id = %id, "load pending buffer failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    // Platform dispatch (ADR-0072): pick the CodePlatform implementation for this task's platform.
    // GitHub mints an installation token internally; GitLab uses its static PAT. The trait
    // encapsulates auth so this handler stays platform-agnostic.
    let Some(platform) = state.platforms.get(&context.platform) else {
        tracing::error!(
            task_id = %id,
            platform = %context.platform,
            "no platform implementation configured for this task's platform",
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "platform not configured for this task",
        )
            .into_response();
    };
    // serve keeps the App key for READS only (ADR-0059): we mint a token to fetch the PR diff so the
    // review is fully *shaped* here (pre-rendered body + validated inline comments). Nothing is posted —
    // every GitHub write is enqueued to `outbox` and the reconciler delivers it.
    let t = crate::outbox::Target {
        task_id: Some(id),
        platform: context.platform,
        installation_id: context.installation_id,
        owner: &context.owner,
        repo: &context.name,
        egress: &state.egress,
    };

    // Whether this run posts a PR review/verdict at all — findings, a verdict summary, or the
    // empty-buffer backstop. This is the ADR-0056 policy gate for BOTH the reply-drop (step 1) and the
    // review enqueue (step 2), so compute it once up front.
    let has_inline = !pending.inline.is_empty();
    let post_pr_review = posts_pr_review(
        &context.target_type,
        has_inline,
        pending.summary.is_some(),
        pending.is_empty(),
    );

    // 1) Buffered replies → ONE consolidated reply intent. **Policy (ADR-0056):** on a **pull request
    // that is also posting a review**, the verdict belongs solely in the grouped review (step 2) — a
    // separate issue-comment is the duplicate "2× messages" channel, and the agent often buffers
    // progress/verification narration ("still reviewing…", "re-reading each file…") via add_comment.
    // So we DROP the buffered replies whenever a review is posted — gated on `post_pr_review`, NOT on
    // "has inline findings": a CLEAN review (a verdict summary with zero findings) is still a review, and
    // under the old finding-count gate its add_comment narration leaked as a "Lightbridge answer" issue
    // comment on docs PR #224. The reply is kept ONLY when the run posts NO review on the PR — a pure
    // `@mention` *question* whose answer IS the add_comment (no findings, no summary) — or a non-PR
    // (issue) target. On a successful enqueue we drop the rows; a re-finalize re-enqueues idempotently.
    let mut queued_reply = false;
    if !pending.comments.is_empty() {
        if post_pr_review {
            tracing::info!(
                task_id = %id, dropped = pending.comments.len(),
                "PR review: dropping buffered add_comment replies — the review is the only channel (ADR-0056)"
            );
            if let Err(error) = crate::db::clear_pending_action(pool, id, "comment").await {
                tracing::warn!(%error, task_id = %id, "clearing dropped PR replies failed (non-fatal)");
            }
        } else {
            let body = crate::review::render_answer_body(&pending.comments.join("\n\n---\n\n"));
            match crate::outbox::enqueue_reply(
                pool,
                &t,
                context.target_id,
                &body,
                &context.target_type,
            )
            .await
            {
                Ok(_) => {
                    queued_reply = true;
                    let _ = crate::db::clear_pending_action(pool, id, "comment").await;
                }
                Err(error) => {
                    tracing::error!(%error, task_id = %id, "enqueueing reply failed");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "could not queue reply")
                        .into_response();
                }
            }
        }
    }

    // 2) Inline findings + summary → ONE review intent (PR targets only), PLUS the verdict reaction.
    // ADR-0068: only a run with findings enqueues a review; a clean pass suppresses the post (👍 only) but
    // still persists the review row and reacts. `post_pr_review` (computed above) still gates the whole
    // block — a pure @mention question posts neither. (`has_inline` computed above.)
    let mut queued_review = false;
    if post_pr_review {
        let pr = context.target_id;
        let findings: Vec<crate::review::Finding> = pending
            .inline
            .iter()
            .map(|pi| crate::review::Finding {
                file: pi.file.clone(),
                line: pi.line.max(0) as u32,
                // The buffered `add_review_comment` tool call (`InlineActionBody`) doesn't yet carry a
                // `start_line` — extending that mediated-tool contract (agent-runner's
                // `AddReviewCommentArgs` + this receiving DTO) is ADR-0071's companion ticket (#287).
                // `validate()` already handles `start_line: None` as today's single-line path.
                start_line: None,
                priority: pi.priority.clone(),
                category: pi.category.clone(),
                severity: None,
                title: pi.title.clone().unwrap_or_default(),
                body: pi.body.clone(),
                suggestion: pi.suggestion.clone(),
                resources: Vec::new(),
            })
            .collect();

        // Cross-run dedup (ADR-0065, Option B): a re-review must not re-post a finding already sitting on
        // this PR from a prior Lightbridge review. Drop findings whose normalized `(file, line, title)`
        // key matches one already posted — or already QUEUED in the outbox — on the SAME head_sha (line
        // numbers drift across commits, so a key match is only trustworthy within one commit). Sourced
        // from our own persisted `reviews` + pending `outbox` review rows (ADR-0035/0059), not the
        // GitHub API. Best-effort: a lookup error means "nothing posted yet" → no dedup, never a failed
        // finalize. The prompt-side re-derive-then-retract framing (Option C) reduces re-emission
        // upstream; this is the deterministic backstop.
        //
        // `pre_dedup_n` is captured FIRST: the ADR-0068 verdict (👍/👎) must reflect the run's TRUE
        // finding count. A run whose findings were all dedup-suppressed still FOUND them — it is not a
        // clean pass and must never 👍/suppress; only the POSTING is deduped (ADR-0065 composition).
        let pre_dedup_n = findings.len();
        let (findings, deduped_n) = match context.head_sha.as_deref() {
            Some(head) => {
                let posted_keys: std::collections::HashSet<(String, u32, String)> =
                    match crate::db::posted_findings_for_head(
                        pool,
                        context.repository_id,
                        &context.target_type,
                        context.target_id,
                        head,
                        id,
                    )
                    .await
                    {
                        Ok(arrays) => arrays
                            .into_iter()
                            .flat_map(|arr| {
                                serde_json::from_value::<Vec<crate::review::Finding>>(arr)
                                    .unwrap_or_default()
                            })
                            .map(|f| crate::review::dedup_key(&f.file, f.line, &f.title))
                            .collect(),
                        Err(error) => {
                            tracing::warn!(%error, task_id = %id, "posted-findings lookup failed (non-fatal, no dedup)");
                            std::collections::HashSet::new()
                        }
                    };
                crate::review::dedup_against_posted(findings, &posted_keys)
            }
            None => (findings, 0),
        };
        if deduped_n > 0 {
            tracing::info!(
                task_id = %id, deduped_n,
                "re-review dedup: dropped findings already posted on this head_sha (ADR-0065)"
            );
        }
        // The model's `finish` verdict, if it produced one. `None` = an exhausted/clean pass (no
        // verdict) — the FAST body then shows its banner alone, while the DEEP body / stored copy fall
        // back to the default so the verdict is never empty. EXCEPTION (ADR-0065 × ADR-0068): when
        // dedup dropped every finding and the model set no verdict, the stored/posted summary is a
        // truthful "no NEW findings" note — never `DEFAULT_CLEAN_SUMMARY`, which would misrepresent a
        // "found the same issues again" run as clean.
        let real_summary = pending
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let all_deduped = deduped_n > 0 && findings.is_empty();
        let summary = effective_summary(real_summary, deduped_n, all_deduped);

        // The PR-diff fetch is a READ done at produce time (ADR-0059: shaping is the producer's job).
        // Platform-aware (ADR-0072): the trait's `list_changed_files` dispatches to GitHub or GitLab
        // and encapsulates auth internally — no token minting here.
        let repo_ref = RepoRef {
            platform: context.platform,
            full_name: format!("{}/{}", context.owner, context.name),
            platform_repo_id: context.repository_id,
            installation_id: context.installation_id,
        };
        let commentable: std::collections::HashMap<String, std::collections::BTreeSet<u32>> =
            match platform.list_changed_files(&repo_ref, pr).await {
                Ok(files) => files
                    .into_iter()
                    .filter_map(|f| {
                        f.patch
                            .map(|p| (f.path, crate::review::commentable_lines(&p)))
                    })
                    .collect(),
                Err(error) => {
                    tracing::error!(%error, task_id = %id, "fetching PR files failed");
                    return (StatusCode::BAD_GATEWAY, "could not fetch PR files").into_response();
                }
            };

        let in_scope = |f: &crate::review::Finding| {
            let normalized = f.file.replace('\\', "/");
            let trimmed = normalized.trim_start_matches("./").trim_start_matches('/');
            commentable.contains_key(trimmed) || commentable.contains_key(&f.file)
        };
        let label_findings = findings.iter().any(in_scope);
        let label_error = findings.iter().any(|f| f.priority() == "P0" && in_scope(f));
        let findings_json = serde_json::to_value(&findings).unwrap_or_default();

        let validated = crate::review::validate(findings, &commentable);
        // FAST tier (ADR-0062): mark the body as a quick pass — a blockquote banner that names what the
        // pass is and points to the deep review via the App's REAL handle (`state.app_handle`, which only
        // exists control-plane-side; the runner hardcoded the wrong `@lightbridge`). The stored `summary`
        // (re-injected as prior-review context on a later run) stays the verdict/default; only the posted
        // body differs. DEEP keeps the full authoritative review body.
        // On an all-deduped run the FAST banner must not stand alone — the truthful "no NEW findings"
        // note is the whole point of the post, so it rides as the fast body's verdict too.
        let fast_summary = if all_deduped {
            Some(summary.as_str())
        } else {
            real_summary
        };
        let body = if context.tier == "fast" {
            // Platform-aware bot handle (Phase 6): GitLab reviews must name the GitLab bot, not the
            // GitHub App handle, so the "request a deep review" @mention resolves on the right platform.
            let handle = match context.platform {
                crate::integrations::platform::Platform::GitLab => state.gitlab_app_handle.as_str(),
                crate::integrations::platform::Platform::GitHub => state.app_handle.as_str(),
            };
            crate::review::render_fast_body(
                handle,
                fast_summary,
                &validated.deferred,
                &validated.out_of_scope,
            )
        } else {
            crate::review::render_body(&summary, &validated.deferred, &validated.out_of_scope)
        };
        let comments: Vec<crate::outbox::ReviewCommentPayload> = validated
            .inline
            .iter()
            .map(|c| crate::outbox::ReviewCommentPayload {
                path: c.path.clone(),
                line: c.line,
                start_line: c.start_line,
                body: c.body.clone(),
            })
            .collect();
        let (inline_n, deferred_n, out_of_scope_n) = (
            comments.len() as i32,
            validated.deferred.len() as i32,
            validated.out_of_scope.len() as i32,
        );
        // ADR-0068: an EXPLICITLY clean pass (`outcome: "finished"`, zero findings, reactions on) posts
        // NO review — the 👍 reaction is the whole GitHub response. The review row is still persisted
        // (below) so prior-review context + the console keep the verdict. Aborted (honest "couldn't
        // complete" note), exhausted (budget note), and missing/unknown outcomes all still POST — a run
        // that didn't provably finish clean must never masquerade as 👍.
        //
        // ADR-0065 composition: the verdict is computed from the PRE-dedup count — a run whose findings
        // were all already posted (deduped) is NOT clean; it posts the truthful note (shaped above) and
        // reacts 👎, never 👍/suppress. Only the inline COMMENTS are deduped.
        let has_findings = pre_dedup_n > 0;
        let policy = finalize_policy(
            outcome.as_deref(),
            has_findings,
            state.review.reactions_enabled(),
        );

        // ADR-0065 composition, second gate: full silence is only honest when there is NOTHING to
        // reconcile — no prior posted findings on this target. When priors exist and this run re-derived
        // zero findings, the priors were (implicitly or explicitly) retracted, and per the prompt
        // contract the retractions live in the verdict text — so the verdict must POST (👍 still rides
        // from the zero pre-dedup count). Fail open to posting: an errored lookup must never silence a
        // verdict that might carry retractions.
        let suppress_clean = if policy.suppress_clean_post {
            match crate::db::target_has_prior_findings(
                pool,
                context.repository_id,
                &context.target_type,
                context.target_id,
                id,
            )
            .await
            {
                Ok(true) => {
                    tracing::info!(
                        task_id = %id,
                        "clean pass with prior findings on this target: posting the verdict (retraction visibility, ADR-0065)"
                    );
                    false
                }
                Ok(false) => true,
                Err(error) => {
                    tracing::warn!(%error, task_id = %id, "prior-findings check failed (non-fatal): posting instead of suppressing");
                    false
                }
            }
        } else {
            false
        };

        if suppress_clean {
            // Idempotency guard: a `review` intent (any status) or an actually-posted review means this
            // is a re-finalize racing a real review (e.g. crash-after-finalize → requeue, buffer already
            // cleared reads as "clean"). No-op rather than 👍-ing over — or clobbering — the real thing.
            match crate::db::has_review_intent_or_posted_review(pool, id).await {
                Ok(true) => {
                    tracing::info!(
                        task_id = %id,
                        "re-finalize: a review intent/post already exists; skipping the silent-clean path"
                    );
                }
                Ok(false) => {
                    // Persist the verdict FIRST and fail loudly BEFORE the buffer is cleared: this row
                    // feeds later re-review context, and on the silent path nothing else records the run —
                    // a swallowed error here would lose the verdict forever. A 500 makes the runner's
                    // finalize retry re-persist (insert-if-absent + the verdict dedup key are idempotent).
                    if let Err(error) = crate::db::insert_review_if_absent(
                        pool,
                        id,
                        &summary,
                        &body,
                        0,
                        0,
                        0,
                        &findings_json,
                    )
                    .await
                    {
                        tracing::error!(%error, task_id = %id, "persisting silent clean review failed");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "could not persist review",
                        )
                            .into_response();
                    }
                    // The 👍 is the ONLY GitHub response on this path, so its enqueue is fatal too.
                    if let Err(error) = crate::outbox::enqueue_verdict_reaction(
                        pool,
                        &t,
                        context.target_id,
                        REACTION_CLEAN,
                        context.trigger_comment_id,
                        &context.target_type,
                    )
                    .await
                    {
                        tracing::error!(%error, task_id = %id, "enqueueing clean 👍 failed");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "could not queue reaction",
                        )
                            .into_response();
                    }
                    tracing::info!(task_id = %id, "clean pass: no findings → suppressing review post, 👍 only (ADR-0068)");
                }
                Err(error) => {
                    tracing::error!(%error, task_id = %id, "silent-clean idempotency check failed");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
                }
            }
            let _ = crate::db::clear_pending_action(pool, id, "inline").await;
            let _ = crate::db::clear_pending_action(pool, id, "summary").await;
        } else {
            let payload = crate::outbox::ReviewPayload {
                pr,
                body,
                summary,
                comments,
                inline_n,
                deferred_n,
                out_of_scope_n,
                findings_json,
                label_findings,
                label_error,
            };
            match crate::outbox::enqueue_review(pool, &t, &payload).await {
                Ok(_) => {
                    queued_review = true;
                    tracing::info!(task_id = %id, inline = inline_n, deferred = deferred_n, out_of_scope = out_of_scope_n, "review queued for egress");
                    // Drop the inline + summary rows now the intent is durably queued, so a re-finalize
                    // doesn't re-shape (the dedup_key would no-op the re-enqueue anyway).
                    let _ = crate::db::clear_pending_action(pool, id, "inline").await;
                    let _ = crate::db::clear_pending_action(pool, id, "summary").await;
                }
                Err(error) => {
                    tracing::error!(%error, task_id = %id, "enqueueing review failed");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "could not queue review")
                        .into_response();
                }
            }
            // ADR-0068 verdict reaction on the trigger: 👎 for findings (pre-dedup — an all-deduped run
            // reacts 👎 with its truthful note), or 👍 for a clean finish that still posts because prior
            // findings exist on this target (retraction visibility, ADR-0065 composition). Best-effort:
            // the review itself is queued, the reaction is cosmetic.
            if let Some(content) = policy.verdict
                && let Err(error) = crate::outbox::enqueue_verdict_reaction(
                    pool,
                    &t,
                    context.target_id,
                    content,
                    context.trigger_comment_id,
                    &context.target_type,
                )
                .await
            {
                tracing::warn!(%error, task_id = %id, content, "enqueueing verdict reaction failed (non-fatal)");
            }
            // An aborted run posted an apology, not a verdict → 😕 (same dedup key as the failure-path
            // 😕, so a run that also reports `failed` isn't double-reacted).
            if policy.react_confused
                && let Err(error) = crate::outbox::enqueue_reaction(
                    pool,
                    &t,
                    context.target_id,
                    "confused",
                    context.trigger_comment_id,
                    &context.target_type,
                )
                .await
            {
                tracing::warn!(%error, task_id = %id, "enqueueing aborted 😕 failed (non-fatal)");
            }
        }
    }

    // 3) Record the emergent run kind (ADR-0037).
    let kind = match (has_inline, queued_reply) {
        (true, true) => "mixed",
        (true, false) => "review",
        (false, true) => "ask",
        (false, false) => "review", // summary-only or empty → a (clean) review
    };
    let _ = crate::db::set_task_kind(pool, id, kind).await;

    // 4) Purge-on-success (ADR-0087): the run has finalized (review/reply queued for egress), so its
    // durable-step journal is dead weight — drop it now rather than waiting for the TTL sweep. A no-op
    // (0 rows) when the run wasn't journaling (the default `Passthrough` prod path), so it stays
    // prod-neutral. Best-effort: the TTL sweep in the `replay` role is the backstop, so a purge blip
    // never fails a finalize.
    match crate::db::durable_step_run_epoch(pool, id).await {
        Ok(Some(run_epoch)) => {
            if let Err(error) = crate::db::purge_durable_steps(pool, id, run_epoch).await {
                tracing::warn!(%error, task_id = %id, "purge-on-success of durable steps failed (non-fatal; TTL sweep backstops)");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, task_id = %id, "resolving run_epoch for durable-step purge failed (non-fatal)");
        }
    }

    Json(serde_json::json!({ "kind": kind, "review": queued_review, "reply": queued_reply }))
        .into_response()
}

/// The runner's status report. `detail` is optional free text for diagnostics — persisted to the
/// task's `error_detail` (#137) so the console can surface why a run did not post a review.
#[derive(Debug, Deserialize)]
pub struct StatusUpdate {
    pub status: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// `POST /internal/tasks/{id}/status` — apply a runner-reported status transition.
pub async fn set_status(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(update): Json<StatusUpdate>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    if !crate::db::is_runner_reportable_status(&update.status) {
        return (StatusCode::BAD_REQUEST, "unsupported status").into_response();
    }
    if let Some(detail) = &update.detail {
        tracing::info!(task_id = %id, status = %update.status, detail, "runner status report");
    }
    // #137: persist the runner's free-text `detail` (e.g. a failure reason, or a "posted nothing"
    // no-op) so the console can surface why a run did not post a review. Previously this was only
    // logged and dropped ("not persisted yet"), which is why a 14-day audit found 98 of 144 (~68%)
    // "succeeded" PR-review tasks had posted nothing with no recorded reason.
    match crate::db::set_task_status(pool, id, &update.status, update.detail.as_deref()).await {
        Ok(true) => {
            // ADR-0037 idempotency: a runner (re)starting its task clears any buffer left by a prior
            // attempt, so a retry accumulates from empty rather than appending to a partial review.
            if update.status == "running"
                && let Err(error) = crate::db::clear_pending_review(pool, id).await
            {
                tracing::warn!(%error, task_id = %id, "clearing pending buffer on (re)start failed (non-fatal)");
            }
            // A terminal failure gets 😕 + a fallback "review failed, retry" comment on the PR when the
            // review never finalized (ADR-0056), so the author isn't left in silence. Success is
            // acknowledged by the verdict reaction (👍/👎, ADR-0068) in `finalize_review`, so we don't
            // double-react here.
            if matches!(update.status.as_str(), "failed" | "timed_out") {
                let state = state.clone();
                let pool = pool.clone();
                tokio::spawn(async move {
                    handle_review_failure(&state, &pool, id).await;
                });
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "set task status failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "update error").into_response()
        }
    }
}

/// The current task status, for the runner's self-cancel poll.
#[derive(Debug, Serialize)]
pub struct TaskStatusResponse {
    pub status: String,
}

/// `GET /internal/tasks/{id}/status` — the task's current status, so the runner can stop promptly
/// when its task is cancelled (e.g. its PR closed) even if the reaper that would delete the Job is
/// down. Lightweight: no token mint. A missing task is `404` — the runner treats that as "stop" too.
pub async fn get_status(
    _auth: RunnerAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_task_status(pool, id).await {
        Ok(Some(status)) => Json(TaskStatusResponse { status }).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, task_id = %id, "get task status failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// GitHub feedback when a **PR** review task fails terminally (runner-reported `failed`/`timed_out`):
/// **enqueue** a 😕 reaction (gated on the toggle) and the ADR-0056 failure notice. Both ride the
/// egress outbox (ADR-0059) — serve no longer posts — and the reconciler re-checks `has_posted_to_github`
/// before the notice, so a finalize-then-fail stays quiet. The *uncatchable*-kill path (no status report
/// reaches serve) is covered by the reaper enqueueing the same notice (ADR-0057, now via the outbox).
async fn handle_review_failure(state: &AppState, pool: &sqlx::PgPool, id: Uuid) {
    let context = match crate::db::get_task_context(pool, id).await {
        Ok(Some(context)) if context.target_type == "pull_request" => context,
        _ => return,
    };
    let t = crate::outbox::Target {
        task_id: Some(id),
        platform: context.platform,
        installation_id: context.installation_id,
        owner: &context.owner,
        repo: &context.name,
        egress: &state.egress,
    };
    if state.review.reactions_enabled() {
        // ADR-0068: retarget 😕 to the @mention comment when the task was mention-triggered.
        if let Err(error) = crate::outbox::enqueue_reaction(
            pool,
            &t,
            context.target_id,
            "confused",
            context.trigger_comment_id,
            &context.target_type,
        )
        .await
        {
            tracing::warn!(%error, task_id = %id, "enqueueing failure reaction failed (non-fatal)");
        }
    }
    if let Err(error) =
        crate::outbox::enqueue_failure_notice(pool, &t, context.target_id, &context.target_type)
            .await
    {
        tracing::warn!(%error, task_id = %id, "enqueueing failure notice failed (non-fatal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR-0056 reply-drop policy (the docs PR #224 regression). The gate is "a PR review is posted",
    // NOT "there are findings" — a clean review (verdict summary, zero findings) must still suppress the
    // agent's buffered add_comment narration.
    #[test]
    fn posts_pr_review_gates_on_any_review_not_finding_count() {
        // The #224 case: a clean PR review — verdict summary, ZERO inline findings → still a review, so
        // its add_comment narration is dropped.
        assert!(
            posts_pr_review("pull_request", false, true, false),
            "clean PR review (summary, no findings) still posts a review → drop replies"
        );
        // A PR review with findings → review posted.
        assert!(posts_pr_review("pull_request", true, false, false));
        // The empty-buffer backstop on a PR posts a default clean review.
        assert!(posts_pr_review("pull_request", false, false, true));
        // A pure @mention QUESTION on a PR: only an add_comment answer, no findings/summary, buffer not
        // empty → NOT a review → the reply (the answer) is kept.
        assert!(
            !posts_pr_review("pull_request", false, false, false),
            "PR question with only a reply posts no review → keep the answer"
        );
        // A non-PR (issue) target is never a PR review → the reply is the content, kept.
        assert!(!posts_pr_review("issue", true, true, false));
        assert!(!posts_pr_review("issue", false, false, false));
    }

    // ADR-0068 policy matrix. Suppress-and-👍 ONLY on an explicitly clean finish; every other outcome
    // fails open to posting. (❌ has no GitHub reaction; 👎 is the agreed stand-in.)
    #[test]
    fn finalize_policy_suppresses_only_an_explicit_clean_finish() {
        // The one silent case: finished + zero findings + reactions on → suppress, 👍.
        let p = finalize_policy(Some("finished"), false, true);
        assert!(p.suppress_clean_post);
        assert_eq!(p.verdict, Some("+1"));
        assert!(!p.react_confused);

        // Finished with findings → post + 👎.
        let p = finalize_policy(Some("finished"), true, true);
        assert!(!p.suppress_clean_post);
        assert_eq!(p.verdict, Some("-1"));
    }

    #[test]
    fn finalize_policy_aborted_posts_the_note_with_confused_not_a_verdict() {
        // The runner's Aborted path buffers "Couldn't complete this review: …" as the summary and
        // reports succeeded — it must POST that note (never a silent misleading 👍) and react 😕.
        let p = finalize_policy(Some("aborted"), false, true);
        assert!(!p.suppress_clean_post, "an aborted run is not a clean pass");
        assert_eq!(p.verdict, None, "an apology is not a verdict");
        assert!(p.react_confused);
    }

    #[test]
    fn finalize_policy_exhausted_zero_findings_posts_budget_note_without_verdict() {
        // Deep-tier Exhausted sets the "⚠️ Review hit its step budget…" summary; "clean so far" from an
        // incomplete pass must not read as "clean" → post, no verdict reaction either way.
        let p = finalize_policy(Some("exhausted"), false, true);
        assert!(!p.suppress_clean_post);
        assert_eq!(p.verdict, None);
        assert!(!p.react_confused);
        // Exhausted WITH findings still posts them → 👎 (real findings were posted).
        let p = finalize_policy(Some("exhausted"), true, true);
        assert!(!p.suppress_clean_post);
        assert_eq!(p.verdict, Some("-1"));
    }

    #[test]
    fn finalize_policy_missing_or_unknown_outcome_fails_open_to_posting() {
        // An older runner (rolling deploy) sends no outcome → the old visible behavior: post, and no
        // 👍 (the run isn't provably a clean finish).
        let p = finalize_policy(None, false, true);
        assert!(!p.suppress_clean_post);
        assert_eq!(p.verdict, None);
        let p = finalize_policy(Some("gibberish"), false, true);
        assert!(!p.suppress_clean_post);
        assert_eq!(p.verdict, None);
    }

    #[test]
    fn finalize_policy_reactions_disabled_never_suppresses_and_never_reacts() {
        // review.reactions=false: the 👍 could never compensate for a suppressed post, so the clean
        // review posts exactly as before ADR-0068 — and no reaction of any kind is enqueued.
        let p = finalize_policy(Some("finished"), false, false);
        assert!(
            !p.suppress_clean_post,
            "suppression without the compensating 👍 would be total silence"
        );
        assert_eq!(p.verdict, None);
        let p = finalize_policy(Some("finished"), true, false);
        assert_eq!(p.verdict, None);
        let p = finalize_policy(Some("aborted"), false, false);
        assert!(!p.react_confused);
    }

    // ADR-0065 × ADR-0068 composition: the verdict reads the PRE-dedup finding count, the post reads
    // the POST-dedup set. A run whose findings were ALL dedup-suppressed is not clean — it posts a
    // truthful "no NEW findings" note and reacts 👎, never 👍/suppress.
    #[test]
    fn all_deduped_run_posts_truthful_note_with_findings_verdict_not_clean() {
        // `has_findings` fed to the policy is pre-dedup (5 found, 5 dropped): 👎 + no suppression.
        let p = finalize_policy(Some("finished"), true, true);
        assert!(
            !p.suppress_clean_post,
            "an all-deduped run must stay visible"
        );
        assert_eq!(p.verdict, Some(REACTION_FINDINGS), "👎 from the true count");

        // And the body it posts (no model verdict) is the truthful note — never the clean default.
        let s = effective_summary(None, 5, true);
        assert_eq!(
            s,
            "No new findings — 5 prior finding(s) on this commit still stand."
        );
        assert!(
            !s.contains(DEFAULT_CLEAN_SUMMARY),
            "a dedup-suppressed run must never read (or persist) as clean"
        );
    }

    #[test]
    fn effective_summary_keeps_model_verdict_and_clean_default() {
        // The model's own verdict always wins, even on an all-deduped run.
        assert_eq!(
            effective_summary(Some("Both P1s still stand; see prior comments."), 5, true),
            "Both P1s still stand; see prior comments."
        );
        // A genuinely clean run (nothing found, nothing deduped) keeps the default.
        assert_eq!(effective_summary(None, 0, false), DEFAULT_CLEAN_SUMMARY);
        // Partial dedup (some findings survive) also keeps the default when the model set no verdict —
        // the surviving findings carry the review; the summary is not the dedup note.
        assert_eq!(effective_summary(None, 3, false), DEFAULT_CLEAN_SUMMARY);
    }

    // Zero findings re-derived + prior findings exist on the target → the finalize handler's
    // `target_has_prior_findings` gate flips `suppress_clean_post` OFF so the verdict (which carries the
    // retractions per the prompt contract) POSTS — while the 👍 still rides from the zero pre-dedup
    // count. Full silence stays reserved for a clean finish with NO priors. The DB half of the gate is
    // covered by `db::tests::target_has_prior_findings_*`; this pins the policy half.
    #[test]
    fn clean_finish_verdict_is_thumbs_up_whether_posted_or_suppressed() {
        let p = finalize_policy(Some("finished"), false, true);
        assert_eq!(
            p.verdict,
            Some(REACTION_CLEAN),
            "zero pre-dedup findings on a finish → 👍, posted or not"
        );
        assert!(
            p.suppress_clean_post,
            "the policy half still asks for silence; the prior-findings gate decides (handler-side)"
        );
    }

    #[test]
    fn parse_knowledge_tool_name_splits_server_and_tool() {
        assert_eq!(
            parse_knowledge_tool_name("mcp__brave-search__brave_web_search"),
            Some(("brave-search", "brave_web_search"))
        );
        // The tool half itself may contain `__` — split_once takes only the FIRST `__` after the
        // prefix, so everything past it (including further `__`) belongs to the tool name.
        assert_eq!(
            parse_knowledge_tool_name("mcp__context7__resolve-library-id"),
            Some(("context7", "resolve-library-id"))
        );
    }

    #[test]
    fn parse_knowledge_tool_name_rejects_malformed_names() {
        assert_eq!(parse_knowledge_tool_name("brave_web_search"), None); // no mcp__ prefix
        assert_eq!(parse_knowledge_tool_name("mcp__no_double_underscore"), None);
        assert_eq!(parse_knowledge_tool_name("mcp__"), None);
        assert_eq!(parse_knowledge_tool_name(""), None);
    }
}
