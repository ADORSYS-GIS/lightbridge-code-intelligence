//! # A2A SDK evaluation probe (RFC-0006 / #298) — THROWAWAY
//!
//! This crate is an **evaluation probe**, not production code. It answers the R2
//! source-read gate from [RFC-0006](../../docs/rfc/0006-a2a-agent-surface.md): can the
//! official Rust A2A SDK (`a2a-server-lf` 0.4.0 / `a2a-lf` 0.3.0) back its task store with
//! *our* state (Postgres, later Restate), or must we hand-roll a direct Axum REST+SSE binding?
//!
//! ## Verdict (grounded in the SDK source, read from the registry cache)
//!
//! **USE `a2a-server-lf`, backing it with our own `TaskStore`.** The gate passes:
//!
//! 1. **Task store IS pluggable.** `a2a_server::TaskStore` is a plain public
//!    `#[async_trait] trait TaskStore: Send + Sync + 'static` with four methods
//!    (`create`/`update`/`get`/`list`). `DefaultRequestHandler::new(executor, task_store: impl
//!    TaskStore)` boxes *any* impl into `Arc<dyn TaskStore>` — the in-memory store is merely
//!    the bundled default, not hard-wired. [`ProbeTaskStore`] below is our own impl; the
//!    `get_task_round_trip_through_our_store` test drives a real `GetTask` through
//!    `DefaultRequestHandler` reading from it. A sqlx/Restate impl slots in the same way.
//!    Note the trait exposes `TaskVersion` but `update` takes no *expected* version, so
//!    optimistic-concurrency CAS is the store's own concern (fine — our `run_epoch`
//!    idempotency already owns that), and the handler's write path is upsert (update, then
//!    create-on-not-found). The sibling `PushConfigStore` and the whole `RequestHandler` are
//!    equally public traits, so even the request semantics are replaceable if needed.
//!
//! 2. **0.4.0 covers the v1.0.1 wire shapes we care about.** Verified in source + asserted in
//!    tests: well-known path `/.well-known/agent-card.json`; PascalCase JSON-RPC methods
//!    (`SendMessage`/`GetTask`/`CancelTask`, and it explicitly rejects stale `message.send`/
//!    `tasks.get`); `TASK_STATE_*` SCREAMING_SNAKE enum values; camelCase card with an ordered
//!    `supportedInterfaces[]`; OIDC security scheme (`openIdConnectSecurityScheme`); REST
//!    v1.0.1 paths (`/message:send`, `/tasks/{id}`, `/tasks/{id}/cancel`, …) with 0.x legacy
//!    aliases. **Gaps to note, not blockers:** the crate's protocol constant is `VERSION =
//!    "1.0"` (not `"1.0.1"`), and the REST binding emits `application/json` rather than the
//!    spec's `application/a2a+json` media type and sets no `A2A-Version` response header
//!    (the `A2A-Version` key exists only as an inbound service-param). If a counterparty
//!    enforces the media type we'd wrap the response; otherwise cosmetic.
//!
//! 3. **rustls lineage is compatible with the workspace.** `a2a-server-lf` (default
//!    `rustls-tls`) rides **reqwest 0.13 + rustls 0.23 on aws-lc-rs** (for the push-notification
//!    egress client). That is the *same* generation already in `services/control-plane` — whose
//!    `main.rs` two-provider note (ring via sqlx + aws-lc-rs via rmcp→reqwest 0.13) already pins
//!    `ring` as the process default. Adding this SDK introduces no new rustls major and no new
//!    panic surface beyond what that existing `install_default()` pin covers. For the *server*
//!    role we can additionally select the SDK's `rustls-no-provider` feature and let our `ring`
//!    pin decide, avoiding a second bundled provider entirely.
//!
//! Fallback (a direct ~9-endpoint Axum REST+SSE binding, spec-compliant per §5.2) is therefore
//! **not** needed for Phase 1.
//!
//! ## What is reusable here vs. throwaway
//! [`build_agent_card`], [`build_card_router`] and [`ProbeTaskStore`] are exercised by tests
//! (they mirror what the real `a2a` role would ship). The rest is investigative glue — delete
//! this whole crate once the resulting ADR lands.

use std::collections::HashMap;
use std::sync::Mutex;

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentInterface, AgentSkill, ListTasksRequest,
    ListTasksResponse, OpenIdConnectSecurityScheme, SecurityScheme, Task,
    TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};
use a2a_server::task_store::TaskVersion;
use a2a_server::{StaticAgentCard, TaskStore};
use async_trait::async_trait;

/// The Keycloak OIDC discovery URL is a placeholder for the probe; the real card wires it to our
/// realm (ADR-0014). Kept obviously-fake so nobody mistakes the probe card for a live one.
const OIDC_DISCOVERY_URL: &str =
    "https://keycloak.example.invalid/realms/lightbridge/.well-known/openid-configuration";

/// Build the static agent card the `a2a` role would publish: a `review` + `ask` skill card with
/// an ordered `supportedInterfaces[]` (JSON-RPC preferred, then REST/HTTP+JSON) and an OIDC
/// security scheme placeholder. Reusable — this is the shape the real role serves.
pub fn build_agent_card(base_url: &str) -> AgentCard {
    let mut security_schemes = HashMap::new();
    security_schemes.insert(
        "keycloak-oidc".to_string(),
        SecurityScheme::OpenIdConnect(OpenIdConnectSecurityScheme {
            open_id_connect_url: OIDC_DISCOVERY_URL.to_string(),
            description: Some(
                "Keycloak OIDC; callers are client-credentials service accounts.".to_string(),
            ),
        }),
    );

    AgentCard {
        name: "Lightbridge Code Intelligence".to_string(),
        description: "A2A surface over Lightbridge's review + ask agents.".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // Order matters: the first entry is the preferred transport (spec §5.6.4).
        supported_interfaces: vec![
            AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        capabilities: AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: None,
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        skills: vec![
            AgentSkill {
                id: "review".to_string(),
                name: "Deep PR review".to_string(),
                description: "Request a deep review of a pull/merge request.".to_string(),
                tags: vec!["code-review".to_string(), "ci".to_string()],
                examples: Some(vec!["Review PR #128 in acme/api".to_string()]),
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            },
            AgentSkill {
                id: "ask".to_string(),
                name: "Ask about a repo".to_string(),
                description: "Conversational Q&A grounded in an indexed repository.".to_string(),
                tags: vec!["qa".to_string(), "retrieval".to_string()],
                examples: Some(vec!["Where is auth enforced in acme/api?".to_string()]),
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            },
        ],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: Some(security_schemes),
        security_requirements: None,
        signatures: None,
    }
}

/// An axum router serving [`build_agent_card`] at the well-known path via the SDK's
/// `StaticAgentCard`. Reusable shape; no TLS, no network — drivable in-process for tests.
pub fn build_card_router(base_url: &str) -> axum::Router {
    let producer = std::sync::Arc::new(StaticAgentCard::new(build_agent_card(base_url)));
    a2a_server::agent_card::agent_card_router(producer)
}

/// Our own [`TaskStore`] impl — the crux of the R2 pluggability answer. Backed here by a plain
/// map to stay network-free, but the trait surface it fills (create/update/get/list over
/// `A2AError`) is exactly what a `sqlx`/Restate-backed store maps onto our `tasks` rows. That
/// `DefaultRequestHandler::new(executor, ProbeTaskStore)` compiles and serves reads through it is
/// the proof the SDK is not hard-wired to its in-memory store.
#[derive(Default)]
pub struct ProbeTaskStore {
    tasks: Mutex<HashMap<String, (Task, TaskVersion)>>,
}

impl ProbeTaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Preload a task, mirroring a real store already holding a row the webhook path created.
    pub fn with_task(task: Task) -> Self {
        let store = Self::new();
        store
            .tasks
            .lock()
            .expect("probe store mutex poisoned")
            .insert(task.id.clone(), (task, 1));
        store
    }
}

#[async_trait]
impl TaskStore for ProbeTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut guard = self.tasks.lock().expect("probe store mutex poisoned");
        if guard.contains_key(&task.id) {
            return Err(A2AError::internal("task already exists"));
        }
        let id = task.id.clone();
        guard.insert(id, (task, 1));
        Ok(1)
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut guard = self.tasks.lock().expect("probe store mutex poisoned");
        let entry = guard
            .get_mut(&task.id)
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        entry.1 += 1;
        entry.0 = task;
        Ok(entry.1)
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        Ok(self
            .tasks
            .lock()
            .expect("probe store mutex poisoned")
            .get(task_id)
            .map(|(task, _)| task.clone()))
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let guard = self.tasks.lock().expect("probe store mutex poisoned");
        let tasks: Vec<Task> = guard
            .values()
            .filter(|(task, _)| {
                req.context_id
                    .as_ref()
                    .is_none_or(|ctx| &task.context_id == ctx)
            })
            .filter(|(task, _)| {
                req.status
                    .as_ref()
                    .is_none_or(|state| &task.status.state == state)
            })
            .map(|(task, _)| task.clone())
            .collect();
        let total = tasks.len() as i32;
        Ok(ListTasksResponse {
            tasks,
            next_page_token: String::new(),
            page_size: total,
            total_size: total,
        })
    }
}
