//! Lightbridge control plane.
//!
//! The trust boundary of the system: it verifies GitHub webhooks, owns task/persistence
//! lifecycle, and acts as an OAuth2 **resource server** — validating OIDC access tokens (Keycloak
//! in dev) on protected routes. It does not issue tokens or store credentials; identity comes from
//! the validated JWT claims (see ADR-0014). Persistence is Postgres via hand-written SQLx
//! (cratestack codegen deferred — ADR-0005).
//!
//! # Module map
//!
//! Three concern groups (each its own submodule directory) sit on a few foundational modules:
//!
//! - [`http`] — the HTTP surface (`serve` role): [`webhook`](http::webhook) (GitHub webhooks, HMAC +
//!   delivery-id dedup), [`internal`](http::internal) (runner bootstrap/results API),
//!   [`admin`](http::admin) (dashboard admin), [`metrics`](http::metrics) (Prometheus `/metrics`).
//!   Routing + auth middleware live in this file.
//! - [`queue`] — queue & dispatch (`dispatcher` role): [`dispatcher`](queue::dispatcher) (claim +
//!   launch one Job per task), [`reaper`](queue::reaper) (Job GC + data-purge reconciler),
//!   [`lifecycle`](queue::lifecycle) (task state machine), [`tasks`](queue::tasks) (queue persistence).
//! - [`integrations`] — external systems the control plane owns credentials for:
//!   [`github`](integrations::github) (App auth + token mint + review write-back),
//!   [`k8s`](integrations::k8s) (Job manifests), [`neo4j`](integrations::neo4j) (graph writes).
//!
//! Foundational modules at the crate root: [`config`] (typed config load), [`db`] (pool +
//! migrations), [`jwt`] (OIDC token validation), [`types`] (shared domain types), [`review`]
//! (review validation).

mod a2a;
mod http;
mod integrations;
mod queue;

// Foundational modules the groups above build on.
mod config;
mod db;
mod jwt;
mod mcp_client;
mod outbox;
mod review;
mod runner_token;
mod types;

// Global allocator. The release images are static-musl (ADR-0080); musl's built-in malloc regresses
// sharply under this server's multithreaded allocation load, so we route every allocation through
// mimalloc, which restores glibc-class throughput. Harmless on glibc builds too.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;

use jwt::JwtValidator;
// Bring the grouped modules into scope under their bare names. `crate::` is required to
// disambiguate from the extern `http` / `metrics` crates pulled in by axum.
use crate::http::{admin, internal, metrics, webhook};
use crate::integrations::{github, gitlab, k8s, neo4j};
use crate::queue::{dispatcher, tasks};

#[derive(Clone)]
pub struct AppState {
    /// Secret used to verify GitHub webhook signatures (`X-Hub-Signature-256`).
    pub github_webhook_secret: Arc<String>,
    /// In-memory delivery-id dedup set — the fallback when no database is configured (dev). With a
    /// pool, the webhook dedups on the `webhook_deliveries` PRIMARY KEY instead.
    pub seen_deliveries: Arc<Mutex<HashSet<String>>>,
    /// OIDC token validator for protected routes. `None` when `OIDC_ISSUER` is unset (fails closed).
    pub jwt: Option<Arc<JwtValidator>>,
    /// Postgres pool. `None` when `DATABASE_URL` is unset (dev) or the connection failed.
    pub db: Option<PgPool>,
    /// Dev-only opt-in (`ALLOW_NO_DB=1`) to run without a database. Without it, a pod that has no
    /// `DATABASE_URL` fails readiness instead of silently dedup'ing in process memory — which would
    /// reintroduce the multi-replica duplicate-task bug (RFC-0001 Phase 0).
    pub allow_no_db: bool,
    /// GitHub App auth (App JWT → installation tokens). `None` when the App env is unset.
    pub github: Option<github::GithubApp>,
    /// File-configured GitLab projects. `None` when `gitlab.enabled` is false in
    /// `control-plane.json`. There is intentionally no `GITLAB_*` env fallback.
    pub gitlab: Option<gitlab::GitlabRegistry>,
    /// Platform dispatch table (ADR-0072): maps each configured `Platform` to its `CodePlatform`
    /// implementation. Built once at startup from the configured GitHub App + GitLab client; shared
    /// by the HTTP handlers (e.g. `finalize_review`) and the reconciler so both pick the right
    /// implementation per task. Empty when no platform is configured (the reconciler bails in that
    /// case; HTTP handlers return 503).
    pub platforms: std::collections::HashMap<
        integrations::platform::Platform,
        Arc<dyn integrations::platform::CodePlatform>,
    >,
    /// Mints/verifies per-task tokens for the internal runner API (`RUNNER_TOKEN_SIGNING_KEY`,
    /// ADR-0092). `None` disables those routes (they fail closed with 503) — the dispatcher mints a
    /// fresh, task-scoped token with the same signer and injects it into each agent Job's
    /// `AGENT_RUNNER_TOKEN` env var; the signing key itself never leaves this process (see
    /// internal.rs / integrations/k8s.rs / ADR-0017).
    pub runner_token_signer: Option<Arc<runner_token::RunnerTokenSigner>>,
    /// Neo4j (Bolt) handle for the structural code graph (ADR-0019). `None` when `NEO4J_URI` is
    /// unset — the graph-ingest route then fails closed (503). Held here so the untrusted Job never
    /// gets Neo4j creds (it POSTs the graph through the internal API instead).
    pub neo4j: Option<Arc<neo4rs::Graph>>,
    /// Prometheus render handle backing `/metrics` (scraped by Alloy for the Operations dashboard).
    pub metrics: PrometheusHandle,
    /// Review-feedback config (PR reactions + outcome labels) from the file config's `review` section
    /// (else defaults). Held here so the webhook + internal handlers can react/label.
    pub review: Arc<config::ReviewSection>,
    /// Configured external-knowledge MCP servers (ADR-0066), dynamically discovered + called as the
    /// `mcp_tools` mediated tool. Empty by default — no servers means no tools discovered, a safe
    /// degrade. Available to any tier; gated purely by the runner's per-tier `review.tools` allowlist.
    pub knowledge_tools: Arc<config::KnowledgeToolsSection>,
    /// The GitHub App's handle (e.g. `lightbridge-assistant`), from `GITHUB_APP_HANDLE`. A PR comment
    /// whose body starts with `@<handle>` triggers a re-review (the first review is automatic on PR
    /// open). Default `lightbridge-assistant`.
    pub app_handle: Arc<String>,
    /// Dotted claim path the caller's **permissions** list is read from (ADR-0023), from
    /// `PERMISSIONS_CLAIM`. Default `permissions`. Endpoints authorize on permissions, not roles.
    pub permissions_claim: Arc<String>,
}

impl AppState {
    async fn from_env() -> anyhow::Result<Self> {
        let metrics = metrics::install();
        // The file config is optional (ConfigMap-mounted): `review` drives PR feedback, `embeddings`
        // guards the vector column's dimension.
        let file_config = config::load_file_config().unwrap_or_else(|error| {
            tracing::error!(%error, "invalid control-plane config file; using defaults");
            None
        });
        let review = file_config
            .as_ref()
            .map(|f| f.review.clone())
            .unwrap_or_default();
        let embeddings = file_config
            .as_ref()
            .map(|f| f.embeddings.clone())
            .unwrap_or_default();
        let knowledge_tools = file_config
            .as_ref()
            .map(|f| f.knowledge_tools.clone())
            .unwrap_or_default();
        let gitlab_config = file_config
            .as_ref()
            .map(|f| f.gitlab.clone())
            .unwrap_or_default();
        let db = db::connect_from_env().await?;
        // Embedding-dimension safety (ADR-0018): if configured and the live column differs, either
        // reindex-from-scratch (when allowed) or fail loud — never silently mismatch.
        if let (Some(pool), Some(dimension)) = (db.as_ref(), embeddings.dimension) {
            db::reconcile_embedding_dimension(
                pool,
                dimension,
                embeddings.allow_reindex_on_dim_change,
            )
            .await?;
        }
        let neo4j = match neo4j::connect_from_env().await {
            Ok(handle) => handle.map(Arc::new),
            // A graph store outage shouldn't stop the control plane from serving everything else;
            // the graph-ingest route fails closed (503) when this is None.
            Err(error) => {
                tracing::error!(%error, "neo4j connection failed; graph ingestion disabled");
                None
            }
        };
        let allow_no_db = env_flag("ALLOW_NO_DB");
        if db.is_none() {
            if allow_no_db {
                tracing::warn!(
                    "running WITHOUT a database (ALLOW_NO_DB): in-memory dedup, single-replica only — dev use"
                );
            } else {
                tracing::error!(
                    "DATABASE_URL is not set and ALLOW_NO_DB is unset: the pod will fail readiness. \
                     Set DATABASE_URL in production, or ALLOW_NO_DB=1 for local dev."
                );
            }
        }
        let github = github::GithubApp::from_env();
        // A bad GitLab config must not take down GitHub in the same process — degrade GitLab only.
        let gitlab = match gitlab::GitlabRegistry::from_config(&gitlab_config) {
            Ok(registry) => registry,
            Err(error) => {
                tracing::error!(
                    %error,
                    "invalid GitLab config in control-plane.json; GitLab integration disabled"
                );
                None
            }
        };
        // Build the platform dispatch table (ADR-0072) once, from the configured implementations.
        // Shared by the HTTP handlers and the reconciler so both pick the right implementation per task.
        let mut platforms: std::collections::HashMap<
            integrations::platform::Platform,
            Arc<dyn integrations::platform::CodePlatform>,
        > = std::collections::HashMap::new();
        if let Some(app) = github.clone() {
            platforms.insert(
                integrations::platform::Platform::GitHub,
                Arc::new(app) as Arc<dyn integrations::platform::CodePlatform>,
            );
        }
        if let Some(registry) = gitlab.clone() {
            platforms.insert(
                integrations::platform::Platform::GitLab,
                Arc::new(gitlab::GitlabPlatformRouter::new(registry))
                    as Arc<dyn integrations::platform::CodePlatform>,
            );
        }

        Ok(Self {
            github_webhook_secret: Arc::new(
                std::env::var("GITHUB_WEBHOOK_SECRET").unwrap_or_default(),
            ),
            seen_deliveries: Arc::new(Mutex::new(HashSet::new())),
            jwt: jwt::from_env(),
            db,
            allow_no_db,
            github,
            gitlab,
            platforms,
            runner_token_signer: runner_token::RunnerTokenSigner::from_env().map(Arc::new),
            neo4j,
            metrics,
            review: Arc::new(review),
            knowledge_tools: Arc::new(knowledge_tools),
            app_handle: Arc::new(
                std::env::var("GITHUB_APP_HANDLE")
                    .ok()
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| "lightbridge-assistant".to_string()),
            ),
            permissions_claim: Arc::new(
                std::env::var("PERMISSIONS_CLAIM")
                    .ok()
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| "permissions".to_string()),
            ),
        })
    }
}

/// A boolean env flag that is true for `1`/`true`/`yes` (case-insensitive), false otherwise.
fn env_flag(name: &str) -> bool {
    env_flag_value(std::env::var(name).ok().as_deref())
}

fn env_flag_value(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or_default().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// The database dimension of readiness. With a pool, readiness requires a successful ping; without
/// one (no `DATABASE_URL`), the pod is ready only under the dev opt-in — otherwise a misconfigured
/// production pod would silently run per-replica in-memory dedup yet report ready. The three
/// outcomes map to distinct `/readyz` messages so a database outage isn't mistaken for missing
/// config.
#[derive(Debug, PartialEq, Eq)]
enum DbReadiness {
    Ready,
    /// Pool present but the database did not answer (outage / network).
    Unreachable,
    /// No `DATABASE_URL` and no dev opt-in — fail closed.
    NotConfigured,
}

fn db_readiness(has_pool: bool, ping_ok: bool, allow_no_db: bool) -> DbReadiness {
    match (has_pool, ping_ok, allow_no_db) {
        (true, true, _) => DbReadiness::Ready,
        (true, false, _) => DbReadiness::Unreachable,
        (false, _, true) => DbReadiness::Ready,
        (false, _, false) => DbReadiness::NotConfigured,
    }
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics_endpoint))
        // Unified webhook route — detects the platform (GitHub/GitLab) from headers.
        .route(
            "/webhook",
            post(webhook::webhook_router).layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)),
        )
        // Legacy GitHub webhook route — forwards to the unified handler. Kept during the
        // transition so existing webhook configurations don't break.
        .route(
            "/github/webhook",
            post(webhook::github_webhook_legacy).layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)),
        )
        .route("/me", get(jwt::me))
        .route("/tasks", get(tasks::list))
        .route("/tasks/{id}", get(tasks::get))
        .route("/tasks/{id}/review", get(tasks::get_review))
        .route("/tasks/{id}/transcript", get(tasks::get_transcript))
        .route("/tasks/{id}/feedback", get(tasks::get_feedback))
        .route("/tasks/{id}/cancel", post(tasks::cancel))
        .route("/repositories", get(tasks::list_repositories))
        // Deployment/config read for the web console (GitLab base URL, etc.) — no `GITLAB_URL` env.
        .route("/config", get(http::config::deployment_config))
        // Admin API (approval gate, Epic #75) — gated by the `Admin` extractor (admin realm role).
        .route("/admin/repositories", get(admin::list_repositories))
        .route("/admin/repositories/{id}/approve", post(admin::approve))
        .route("/admin/repositories/{id}/deny", post(admin::deny))
        // Internal runner API (shared-bearer, not OIDC) — the agent Job's lifecycle callbacks.
        .route("/internal/tasks/{id}", get(internal::get_context))
        .route(
            "/internal/tasks/{id}/status",
            post(internal::set_status).get(internal::get_status),
        )
        // Chunk batches can be large: 32 chunks × 4096-dim embeddings as JSON ~1.6 MB plus
        // content. Raise the body limit to 16 MiB on this route only.
        .route(
            "/internal/tasks/{id}/chunks",
            post(internal::ingest_chunks).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        // The structural graph (lci-codegraph → Neo4j, ADR-0086). A whole-repo graph can be large,
        // so raise the body limit here too.
        .route(
            "/internal/tasks/{id}/graph",
            post(internal::ingest_graph).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        // Retrieval for the MCP servers (slice 4): semantic search (pgvector) + structural queries
        // (Neo4j), each scoped server-side to the task's repo snapshot.
        .route("/internal/tasks/{id}/search", post(internal::search))
        .route(
            "/internal/tasks/{id}/graph/query",
            post(internal::graph_query),
        )
        // External-knowledge MCP tools (ADR-0066): dynamically discover + call whatever's in
        // `knowledge_tools.mcp_servers` — no per-provider route. Available to any tier; gated purely
        // by the runner's per-tier `review.tools` allowlist, like every other mediated tool.
        .route(
            "/internal/tasks/{id}/knowledge/tools",
            get(internal::list_knowledge_tools),
        )
        .route(
            "/internal/tasks/{id}/knowledge/call",
            post(internal::call_knowledge_tool),
        )
        // The agent run transcript (ADR-0034): the runner submits it at run end.
        .route(
            "/internal/tasks/{id}/transcript",
            post(internal::ingest_transcript),
        )
        // Run-level review telemetry (ADR-0034/0062/0066): the runner submits the offered tool set +
        // the redacted, base64-encoded resolved config at run START (a crashed run still records it).
        .route(
            "/internal/tasks/{id}/review/telemetry",
            post(internal::record_review_telemetry),
        )
        // ADR-0087 durable-step journal: the agent, running under `CheckpointRuntime` (opt-in),
        // journals each step's result and fetches it back on replay. `run_epoch` is resolved
        // server-side from the task row. Additive — unused on the default `Passthrough` path.
        .route(
            "/internal/tasks/{id}/steps/upsert",
            post(internal::upsert_step),
        )
        .route(
            "/internal/tasks/{id}/steps/fetch",
            post(internal::fetch_step),
        )
        // ADR-0037 mediated write actions: the native agent buffers findings/replies/summary, then
        // flushes them as one grouped review on finalize.
        .route(
            "/internal/tasks/{id}/review/inline",
            post(internal::add_review_comment),
        )
        .route(
            "/internal/tasks/{id}/review/inline/retract",
            post(internal::retract_inline),
        )
        .route(
            "/internal/tasks/{id}/review/inline/clear",
            post(internal::clear_inline),
        )
        .route(
            "/internal/tasks/{id}/review/comment",
            post(internal::add_review_reply),
        )
        .route(
            "/internal/tasks/{id}/review/summary",
            post(internal::set_review_summary),
        )
        .route(
            "/internal/tasks/{id}/review/finalize",
            post(internal::finalize_review),
        )
        // ADR-0088 open mode: the sandboxed open agent proposes a PR (branch + metadata) via this
        // mediated endpoint; the control plane offloads the branch and enqueues a dedup'd `pr_open`
        // intent for the egress plane to push. A branch patch can be large, so raise the body limit.
        // DORMANT: no trigger creates an open task, so nothing calls this in prod yet.
        .route(
            "/internal/tasks/{id}/propose-pr",
            post(internal::propose_pr).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .layer(axum::middleware::from_fn(track_http_metrics))
        .with_state(state)
}

async fn liveness() -> &'static str {
    "ok"
}

/// Prometheus text exposition for Alloy to scrape.
async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

/// Middleware: record request count + latency by method, matched route, and status. Uses the
/// matched route (not the raw path) to keep label cardinality bounded.
///
/// Method and status are resolved to `&'static str` via a match so label allocation per request is
/// limited to a single `path.to_string()` inside `metrics::http_request`.
async fn track_http_metrics(req: Request, next: Next) -> Response {
    let method = match *req.method() {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::PATCH => "PATCH",
        axum::http::Method::HEAD => "HEAD",
        axum::http::Method::OPTIONS => "OPTIONS",
        axum::http::Method::CONNECT => "CONNECT",
        axum::http::Method::TRACE => "TRACE",
        _ => "UNKNOWN",
    };
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let start = Instant::now();
    let response = next.run(req).await;
    let status = match response.status().as_u16() {
        200 => "200",
        201 => "201",
        204 => "204",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        422 => "422",
        500 => "500",
        503 => "503",
        _ => "other",
    };
    let elapsed = start.elapsed().as_secs_f64();
    metrics::http_request(method, &path, status, elapsed);
    response
}

/// Readiness fails closed when required configuration is missing or a dependency is unreachable, so
/// a misconfigured pod is not handed traffic it would only reject: no webhook platform configured,
/// missing OIDC issuer / unreachable JWKS, or a configured-but-unreachable database.
async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    // At least one webhook platform must be configured. GitHub still uses an env secret; GitLab is
    // configured only through `control-plane.json`.
    let github_configured = !state.github_webhook_secret.is_empty();
    let gitlab_configured = state
        .gitlab
        .as_ref()
        .is_some_and(gitlab::GitlabRegistry::is_configured);
    if !github_configured && !gitlab_configured {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no webhook platform configured (set GITHUB_WEBHOOK_SECRET or configure gitlab.projects[] in control-plane.json)",
        );
    }
    match &state.jwt {
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "OIDC_ISSUER not configured",
            );
        }
        Some(validator) => {
            if validator.warm().await.is_err() {
                return (StatusCode::SERVICE_UNAVAILABLE, "oidc jwks unavailable");
            }
        }
    }
    // A configured database must be reachable. With no database, we report ready only when the dev
    // opt-in (`ALLOW_NO_DB`) is set — otherwise a misconfigured prod pod would silently dedup in
    // process memory across replicas. The two failure modes get distinct messages so an outage is
    // not mistaken for missing configuration.
    let ping_ok = match &state.db {
        Some(pool) => db::ping(pool).await.is_ok(),
        None => false,
    };
    match db_readiness(state.db.is_some(), ping_ok, state.allow_no_db) {
        DbReadiness::Ready => {}
        DbReadiness::Unreachable => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "database connection failed",
            );
        }
        DbReadiness::NotConfigured => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "database not configured (set DATABASE_URL; ALLOW_NO_DB=1 for dev only)",
            );
        }
    }
    (StatusCode::OK, "ok")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pin the process-default rustls crypto provider before anything builds a TLS client — including
    // the OTel OTLP exporter's own client, set up below. This workspace links *two* providers — `ring`
    // (via sqlx's `tls-rustls-ring-webpki`) and `aws-lc-rs` (pulled transitively by `rmcp` → reqwest
    // 0.13 → rustls-platform-verifier) — and with both present rustls 0.23 can't auto-select a default
    // and panics on the first handshake ("Could not automatically determine the process-level
    // CryptoProvider"). That bit the `dispatcher` role at startup when it builds its kube client.
    // Installing one explicitly makes selection deterministic regardless of which provider features the
    // dependency graph enables. Shared with `agent-runner` via `lci-observability` (ticket #246) so
    // there's one audited call site instead of two independently-maintained copies.
    lci_observability::install_default_crypto_provider();

    // One binary, several roles (RFC-0001): `serve` (HTTP) and `dispatcher` (queue consumer),
    // selected by the first CLI arg or `CONTROL_PLANE_ROLE`. Deployed as separate Deployments off
    // the same image so they scale independently. `scheduler` arrives in Phase 2. `a2a` (RFC-0006 /
    // #299) serves the A2A agent surface (review skill, polling) — an ingress face holding no forge
    // credentials. `notifier` (RFC-0006 Phase 3 / ADR-0079) delivers A2A push-notification webhooks —
    // the first untrusted-destination egress role, isolated so its restrictive NetworkPolicy is
    // enforceable. All roles share the `ring` crypto provider installed above.
    let role = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("CONTROL_PLANE_ROLE").ok())
        .unwrap_or_else(|| "serve".to_string());

    // Tracing/OTel init (ticket #246), after role resolution so `service.name` is role-aware
    // (`control-plane-serve`, `control-plane-dispatcher`, ...) — Tempo's service list otherwise can't
    // tell the roles apart. Still before anything that could emit a log line worth correlating.
    let otel_guard = lci_observability::init_tracing(&format!("control-plane-{role}"));

    // `mint-runner-token` is a local-dev/ops utility (ADR-0092), not a long-running role: it prints one
    // token to stdout and exits. It needs only `RUNNER_TOKEN_SIGNING_KEY` from env, so it's handled
    // before `AppState::from_env()` (which would otherwise require a database). Without it, running the
    // agent-runner manually (docs/local-setup.md) has no way to obtain a token now that the internal API
    // rejects anything but a validly-signed, task-scoped one — there is no shared dev bearer anymore.
    if role == "mint-runner-token" {
        let result = mint_runner_token_cli();
        otel_guard.shutdown().await;
        return result;
    }

    let state = AppState::from_env().await?;
    let result = match role.as_str() {
        "serve" => serve(state).await,
        "dispatcher" => run_dispatcher(state).await,
        // `poller` is the legacy alias for `reconciler` (ADR-0058); accept both so the binary and the
        // Deployment's role string can be flipped in either order across the rename rollout.
        "poller" | "reconciler" => run_reconciler(state).await,
        // The A2A ingress face (RFC-0006 / #299): serves the `review` skill over polling.
        "a2a" => a2a::run(state).await,
        // The A2A push-notification webhook-delivery actor (RFC-0006 Phase 3 / ADR-0079).
        "notifier" => run_notifier(state).await,
        // The durable-replay store lifecycle owner (ADR-0087): TTL-sweeps the `durable_step` journal.
        "replay" => run_replay(state).await,
        other => anyhow::bail!(
            "unknown role {other:?} (expected: serve | dispatcher | reconciler | a2a | notifier | replay | mint-runner-token [| poller])"
        ),
    };
    // Flush any still-batched spans before exit — covers every role's graceful (SIGTERM-triggered)
    // shutdown path. A SIGKILL loses at most one batch-export interval, not the whole trace, since the
    // batch processor also flushes periodically on its own; that residual gap is an accepted tradeoff,
    // not engineered around.
    otel_guard.shutdown().await;
    result
}

/// `control-plane mint-runner-token <task-id>` — mint one per-task runner token from
/// `RUNNER_TOKEN_SIGNING_KEY` and print it to stdout, for exercising the internal API by hand
/// (docs/local-setup.md) or debugging a stuck task in prod. `<task-id>` may also come from `TASK_ID`.
/// TTL matches the dispatcher's own default (`AGENT_JOB_DEADLINE_SECONDS`, else 3600s).
fn mint_runner_token_cli() -> anyhow::Result<()> {
    use anyhow::Context;

    let signer = runner_token::RunnerTokenSigner::from_env()
        .ok_or_else(|| anyhow::anyhow!("RUNNER_TOKEN_SIGNING_KEY is not set"))?;
    let task_id: uuid::Uuid = std::env::args()
        .nth(2)
        .or_else(|| std::env::var("TASK_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("usage: control-plane mint-runner-token <task-id>"))?
        .parse()
        .context("task id is not a valid UUID")?;
    let deadline_seconds = std::env::var("AGENT_JOB_DEADLINE_SECONDS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(3600);
    let token = signer.mint(task_id, deadline_seconds)?;
    println!("{token}");
    Ok(())
}

/// The reconciler role (ADR-0058): a single replica that owns **all platform egress** — it drains
/// the `outbox` and posts each intent (ADR-0059) — and reads 👍/👎 reactions back into
/// `review_feedback` (ADR-0035). Requires a database and at least one platform implementation; run
/// as ONE replica so egress isn't double-posted and reactions aren't double-polled.
///
/// Platform dispatch: the reconciler receives a `HashMap<Platform, Arc<dyn CodePlatform>>` mapping
/// each configured platform to its implementation. Both GitHub (`GithubApp`) and GitLab
/// (`GitlabClient`) are wired; an outbox row whose `platform` has no registered implementation is
/// backed off with a clear error rather than crashing the loop.
async fn run_reconciler(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("reconciler requires DATABASE_URL"))?;
    // The platform dispatch table is built once at startup in `AppState::from_env` and shared by the
    // HTTP handlers and the reconciler (ADR-0072). Clone it out of the shared state.
    let platforms = state.platforms.clone();
    if platforms.is_empty() {
        anyhow::bail!(
            "reconciler requires at least one platform implementation \
             (set GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY for GitHub, \
             or configure gitlab.projects[] in control-plane.json for GitLab)"
        );
    }
    spawn_metrics_server(state.metrics.clone());
    // Env: prefer RECONCILER_*; fall back to the legacy POLLER_* so the rename doesn't require a
    // simultaneous values change.
    let env_u64 = |a: &str, b: &str, default: u64| {
        std::env::var(a)
            .or_else(|_| std::env::var(b))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let interval = std::time::Duration::from_secs(env_u64(
        "RECONCILER_INTERVAL_SECS",
        "POLLER_INTERVAL_SECS",
        300,
    ));
    let within_days = env_u64("RECONCILER_WINDOW_DAYS", "POLLER_WINDOW_DAYS", 14) as i32;
    // A window of 0 (or negative) makes the feedback poll's "completed within the last N days" bound
    // empty, silently disabling it. Clamp to 1 and say so (#216 review).
    let within_days = if within_days < 1 {
        tracing::warn!(
            within_days,
            "RECONCILER_WINDOW_DAYS < 1 would disable the feedback poll; clamping to 1"
        );
        1
    } else {
        within_days
    };
    queue::reconciler::run(pool, platforms, state.review.clone(), interval, within_days).await
}

/// The `replay` role (ADR-0087): owns the `durable_step` journal's lifecycle. Runs the periodic TTL
/// sweep (`DURABLE_STEP_RETENTION`, validated `> 0` at load — a non-positive value is rejected here,
/// not clamped, since it would sweep in-flight resume state). Purge-on-success lives on the finalize
/// write path; this role is the backstop for abandoned/failed runs. Requires a database; run as a
/// singleton (the sweep is idempotent, so extra replicas are harmless but pointless).
async fn run_replay(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("replay role requires DATABASE_URL"))?;
    let retention =
        queue::replay::retention_from_env().map_err(|reason| anyhow::anyhow!(reason))?;
    // Headless role: stand up the metrics-only listener like the dispatcher/reconciler.
    spawn_metrics_server(state.metrics.clone());
    queue::replay::run(pool, retention).await
}

/// The HTTP control surface (webhook ingress, `/tasks`, health, OIDC-protected routes).
async fn serve(state: AppState) -> anyhow::Result<()> {
    // Bind the raw string so hostnames (e.g. `localhost:8080`) resolve via `ToSocketAddrs`,
    // not only literal IP addresses.
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "control-plane listening");
    axum::serve(listener, app(state)).await?;
    Ok(())
}

/// The dispatcher role: consume the task queue and create one Kubernetes Job per task. Requires a
/// database (it is the queue) and a reachable cluster.
async fn run_dispatcher(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("dispatcher requires DATABASE_URL"))?;
    // The dispatcher has no main HTTP server, so stand up a tiny one just for /metrics (+ health)
    // so Alloy can scrape it like the serve pods.
    spawn_metrics_server(state.metrics.clone());
    // Optional file config (ConfigMap-mounted); file-when-present-else-env for the agent-Job knobs
    // and dispatcher timings.
    let file_config = config::load_file_config()?;
    let launcher = k8s::KubeLauncher::resolve(file_config.as_ref().map(|f| &f.agent)).await?;
    let dispatcher_config =
        dispatcher::DispatcherConfig::from_file(file_config.as_ref().map(|f| &f.dispatcher));
    // The pod name is a natural, unique lease owner; fall back to a generic label off-cluster.
    let owner = std::env::var("HOSTNAME").unwrap_or_else(|_| "dispatcher".to_string());
    // Pass Neo4j so the dispatcher's loop can run the durable purge reconciler (graph + pg cleanup).
    dispatcher::run(
        pool,
        launcher,
        owner,
        dispatcher_config,
        state.neo4j.clone(),
        state.review.clone(),
    )
    .await
}

/// The `notifier` role (RFC-0006 Phase 3 / ADR-0079 §4): the SSRF-guarded webhook-delivery actor. It
/// turns the `a2a_task_events` log into ordered, at-least-once webhook POSTs to caller-registered URLs
/// — the control plane's first outbound egress to untrusted, arbitrary-internet destinations. Requires
/// a database (the queue + the config store); holds no forge credentials.
/// Parse the operator's extra SSRF deny-CIDRs for push delivery from `A2A_PUSH_DENIED_CIDRS`
/// (comma-separated — the cluster Service/Pod CIDRs, ADR-0079 §2 / slice 3). Unset → empty (the fixed
/// ranges still block loopback/RFC1918/metadata/…). A malformed entry is a hard startup error
/// (fail-closed: refuse to run with a gap in the SSRF policy rather than deliver silently).
fn push_denied_cidrs_from_env() -> anyhow::Result<Vec<ipnet::IpNet>> {
    match std::env::var("A2A_PUSH_DENIED_CIDRS") {
        Ok(v) => parse_denied_cidrs(&v),
        Err(_) => Ok(Vec::new()),
    }
}

/// Parse a comma-separated CIDR list; blanks are skipped, a malformed entry is an error (fail-closed).
fn parse_denied_cidrs(raw: &str) -> anyhow::Result<Vec<ipnet::IpNet>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<ipnet::IpNet>()
                .map_err(|e| anyhow::anyhow!("invalid CIDR {s:?} in A2A_PUSH_DENIED_CIDRS: {e}"))
        })
        .collect()
}

async fn run_notifier(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("notifier requires DATABASE_URL"))?;
    // Headless role: stand up the metrics-only listener like the dispatcher/reconciler.
    spawn_metrics_server(state.metrics.clone());
    // The token-decryption key (ADR-0079 §3). `None` when unconfigured: a config that carries a token
    // then delivers WITHOUT auth (warned), never plaintext and never a panic; a tokenless config is
    // unaffected. Reuses the same fail-soft loader as the `a2a` role so both interpret the env identically.
    let key = a2a::push_token_key_from_env();
    // The SSRF policy enforced at every delivery (DNS-rebinding/TOCTOU re-validation, ADR-0079 §2).
    // The fixed blocked ranges (loopback/RFC1918/link-local+metadata/ULA/…) always apply; on top the
    // operator supplies the cluster Service/Pod CIDRs via `A2A_PUSH_DENIED_CIDRS` (the app-level half of
    // the defence-in-depth; the deny-internal NetworkPolicy is the network-level half). A cluster whose
    // internal ranges sit outside RFC1918 depends on this being set.
    let denied = push_denied_cidrs_from_env()?;
    let policy = if denied.is_empty() {
        tracing::warn!(
            "notifier: A2A_PUSH_DENIED_CIDRS is unset — only the fixed SSRF ranges apply. A non-RFC1918 \
             cluster CIDR would be reachable; set it (+ the deny-internal NetworkPolicy) before enabling \
             push in such a cluster."
        );
        a2a::ssrf::SsrfPolicy::default()
    } else {
        tracing::info!(
            count = denied.len(),
            "notifier: SSRF policy loaded operator extra deny-CIDRs"
        );
        a2a::ssrf::SsrfPolicy::with_extra_denied(denied)
    };
    let sender = std::sync::Arc::new(queue::notifier::ReqwestWebhookSender::new())
        as std::sync::Arc<dyn queue::notifier::WebhookSender>;
    let cfg = queue::notifier::NotifierConfig::from_env();
    // The pod name is a natural, unique lease owner; fall back off-cluster.
    let owner = std::env::var("HOSTNAME").unwrap_or_else(|_| "notifier".to_string());
    queue::notifier::run(pool, sender, key, policy, owner, cfg).await
}

/// Serve `/metrics` (+ `/healthz`) on `METRICS_ADDR` for roles without a main HTTP server.
fn spawn_metrics_server(handle: PrometheusHandle) {
    let addr = std::env::var("METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
    tokio::spawn(async move {
        let router = Router::new().route("/healthz", get(liveness)).route(
            "/metrics",
            get(move || {
                let handle = handle.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                        handle.render(),
                    )
                }
            }),
        );
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                tracing::info!(addr = %addr, "dispatcher metrics listening");
                if let Err(error) = axum::serve(listener, router).await {
                    tracing::error!(%error, "metrics server stopped");
                }
            }
            Err(error) => tracing::error!(%error, addr = %addr, "failed to bind metrics server"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_denied_cidrs_accepts_a_list_and_rejects_a_bad_entry() {
        // A valid v4 + v6 list parses; blanks/whitespace are ignored.
        let ok = parse_denied_cidrs(" 10.42.0.0/16 , 10.43.0.0/16 ,, fd00::/8 ").unwrap();
        assert_eq!(ok.len(), 3);
        assert!(ok.contains(&"10.42.0.0/16".parse().unwrap()));
        assert!(ok.contains(&"fd00::/8".parse().unwrap()));
        // Empty / unset → no extra CIDRs (the fixed ranges still apply).
        assert!(parse_denied_cidrs("").unwrap().is_empty());
        assert!(parse_denied_cidrs("  ,  ").unwrap().is_empty());
        // A malformed entry is a hard error — the notifier fails closed rather than run with a gap.
        assert!(parse_denied_cidrs("10.42.0.0/16,not-a-cidr").is_err());
        assert!(parse_denied_cidrs("10.42.0.0/99").is_err());
    }

    // The workspace links two rustls crypto providers (`ring` via sqlx, `aws-lc-rs` via rmcp's
    // reqwest 0.13), so rustls can't auto-pick a process default and panics on the first TLS
    // handshake — which crashed the dispatcher role at startup. `main` installs `ring` explicitly;
    // this guards that the provider is actually linked and installable, and that a process default
    // ends up set. `install_default` is process-global and once-only, so tolerate a benign `Err`
    // (another test in this binary may have installed it first) and assert the end state instead.
    #[test]
    fn a_default_crypto_provider_is_installed() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "no process-default rustls CryptoProvider — the dispatcher would panic on first TLS use"
        );
    }

    #[test]
    fn with_pool_readiness_follows_the_ping() {
        // A configured database must actually answer; a failed ping is reported as Unreachable
        // (distinct from missing config), and the dev opt-in never overrides it.
        assert_eq!(db_readiness(true, true, false), DbReadiness::Ready);
        assert_eq!(db_readiness(true, false, false), DbReadiness::Unreachable);
        assert_eq!(db_readiness(true, false, true), DbReadiness::Unreachable);
    }

    #[test]
    fn without_pool_requires_the_dev_opt_in() {
        // No DATABASE_URL: ready only when ALLOW_NO_DB is set — otherwise NotConfigured (fail
        // closed) so a misconfigured prod pod is never handed traffic it would dedup only in memory.
        assert_eq!(
            db_readiness(false, false, false),
            DbReadiness::NotConfigured
        );
        assert_eq!(db_readiness(false, true, false), DbReadiness::NotConfigured);
        assert_eq!(db_readiness(false, false, true), DbReadiness::Ready);
    }

    #[test]
    fn env_flag_parses_truthy_values_only() {
        for truthy in ["1", "true", "TRUE", "Yes"] {
            assert!(env_flag_value(Some(truthy)), "{truthy} should be truthy");
        }
        for falsy in ["0", "false", "no", "", "off"] {
            assert!(!env_flag_value(Some(falsy)), "{falsy} should be falsy");
        }
        assert!(!env_flag_value(None));
    }
}
