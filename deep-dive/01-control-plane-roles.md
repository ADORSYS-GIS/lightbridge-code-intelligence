# 1. One binary, many roles

This chapter is about `services/control-plane/src/main.rs` — the single Rust binary that becomes a
different long-running process depending on how it's started. There is no `control-plane-serve`
crate and a separate `control-plane-dispatcher` crate; it is one `main.rs`, one `AppState`, and a
`match` on a string.

## 1.1 The trust-boundary framing, straight from the file's own doc comment

Before any role logic, `main.rs` states what this service *is*, in its module-level doc comment
(`services/control-plane/src/main.rs:1-7`):

```rust
//! Lightbridge control plane.
//!
//! The trust boundary of the system: it verifies GitHub webhooks, owns task/persistence
//! lifecycle, and acts as an OAuth2 **resource server** — validating OIDC access tokens (Keycloak
//! in dev) on protected routes. It does not issue tokens or store credentials; identity comes from
//! the validated JWT claims (see ADR-0014). Persistence is Postgres via hand-written SQLx
//! (cratestack codegen deferred — ADR-0005).
```

Three separate claims are packed into that paragraph, and each shows up concretely later in this
file:

1. **"Verifies GitHub webhooks"** — the `serve` role's `/api/v2/webhook/github` route runs HMAC
   verification before anything else touches the payload (covered in file `02`'s dedup section).
2. **"Owns task/persistence lifecycle"** — every role in this chapter either creates, claims,
   transitions, or reaps rows in the `tasks` table. No other process in the system is allowed to
   write to it.
3. **"Acts as an OAuth2 resource server"** — it validates tokens; it never mints them. That's
   `jwt::from_env()`, wired into `AppState` at `main.rs:81` and checked in `readiness()`
   (`main.rs:586-598`).

[ADR-0002](../docs/adr/0002-rust-control-plane-trust-boundary.md) is the decision this comment is
summarizing: the agent runner (a separate, untrusted-by-design pod) "may read, query the
graph/vector stores, and prepare structured findings, but it never posts to GitHub or persists
state." Every role below exists to either serve that untrusted agent a narrow, mediated surface, or
to be the trusted actor that performs the writes the agent only *proposes*.

## 1.2 Role selection happens before almost anything else

`main()` (`services/control-plane/src/main.rs:625-691`) does four things, strictly in this order,
and the ordering is deliberate:

1. Install a process-default `rustls` crypto provider (a dependency-conflict workaround, unrelated
   to roles — see the comment at `main.rs:627-636`).
2. **Resolve the role** from the first CLI argument or `CONTROL_PLANE_ROLE`.
3. **Initialize tracing/OTel**, naming the service after the resolved role.
4. Branch on the role and run it.

Step 2, quoted in full (`main.rs:645-648`):

```rust
let role = std::env::args()
    .nth(1)
    .or_else(|| std::env::var("CONTROL_PLANE_ROLE").ok())
    .unwrap_or_else(|| "serve".to_string());
```

CLI arg wins over env var; the default, if neither is set, is `serve`. This is the one surface a
Kubernetes Deployment or a local `cargo run -- dispatcher` ever touches to pick a role — nothing
else in the binary branches on anything resembling "which mode am I in."

**Why role resolution has to happen *before* tracing init**, not after — the comment at
`main.rs:650-652` spells it out:

```rust
// Tracing/OTel init (ticket #246), after role resolution so `service.name` is role-aware
// (`control-plane-serve`, `control-plane-dispatcher`, ...) — Tempo's service list otherwise can't
// tell the roles apart. Still before anything that could emit a log line worth correlating.
let otel_guard = lci_observability::init_tracing(&format!("control-plane-{role}"));
```

If tracing initialized first with a fixed service name, every role's spans would land under one
undifferentiated `control-plane` bucket in Tempo — you'd see a trace but have no way to tell whether
it came from the HTTP server, the queue consumer, or the egress poster. Naming the OTel service
`control-plane-{role}` makes the role a first-class dimension of every trace and metric this process
ever emits, which is why it's worth reordering two setup calls around.

## 1.3 The `match` block that dispatches every role

After `AppState::from_env()` builds the shared state (DB pool, GitHub App client, Neo4j handle,
etc. — see `main.rs:73-143` for the full struct), the role dispatch is one `match`
(`main.rs:667-684`):

```rust
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
    // The MCP server role for external clients.
    "mcp" => mcp::run(state).await,
    other => anyhow::bail!(
        "unknown role {other:?} (expected: serve | dispatcher | reconciler | a2a | notifier | replay | mcp | mint-runner-token [| poller])"
    ),
};
```

Seven real roles (`serve`, `dispatcher`, `reconciler`/`poller`, `a2a`, `notifier`, `replay`, `mcp`),
plus the one-shot CLI utility `mint-runner-token` handled *before* this match
(`main.rs:660-664`, since it needs no `AppState`/database at all). Every arm is `async` and returns
`anyhow::Result<()>` — a role either runs until shut down, or the process exits with an error (which
is exactly what you want from a Kubernetes container: a crashed role should crash the pod, not limp
along half-initialized).

Each role listed here maps to its **own Kubernetes Deployment**, same container image, different
`CONTROL_PLANE_ROLE` env var and replica count — `serve`/`a2a`/`mcp` scale out behind a Service,
`dispatcher`/`reconciler`/`notifier`/`replay` run as (mostly) singletons for reasons each role's own
section below explains. File `08` (deployment topology, out of scope for this set) covers the full
manifest shape; this chapter only needs you to know that the role string is also the knob an
operator turns in `ai-helm-values` to decide how many of each process run and where.

## 1.4 The concepts that make this possible: one binary, many personalities

§1.2 and §1.3 showed *what* the code does. This section is about *why it's even possible* — the
general pattern this is an instance of, and the specific Rust mechanics that make it safe.

### 1.4.1 The general concept: a multi-call binary

A single compiled program that behaves as several different logical programs depending on how it's
invoked is a well-known pattern, older than Rust — BusyBox is the canonical example (one executable
providing dozens of Unix tools, dispatched on `argv[0]`); `git` is a subtler one (`git commit` and
`git push` are the same `git` binary, dispatching internally on `argv[1]`). This codebase's variant
dispatches on an env var/CLI arg instead of `argv[0]`, but the shape is identical: **one build
artifact, N runtime personalities, chosen by a string read at startup.**

The payoff is almost entirely operational, not technical: `serve`, `dispatcher`, `reconciler`,
`a2a`, `notifier`, `replay`, and `mcp` all compile from the *same* `main.rs`, link the *same*
dependency graph (`sqlx`, `axum`, `neo4rs`, ...), and ship as the *same* container image
(`main.rs:116`, file `08`). A CI pipeline builds and scans one image, not seven; a security patch to
one shared dependency is one version bump, not seven coordinated ones; an operator rolling out a fix
touches one image tag across every Deployment. The cost is the flip side of that: every pod running
`notifier` still contains the compiled code for `serve`'s HTTP server, `dispatcher`'s queue
consumer, and everything else, unused. This codebase's bet is that a slightly larger binary is a
better trade than seven separately-versioned ones — a judgment call, not a law of nature.

### 1.4.2 Rust has no language feature for this — it's one `main`, and everything else is a function call

Rust (like C) permits exactly one `fn main()` per binary crate. There is no language-level concept
of "alternate entry points" the illusion of multiple personalities is built entirely out of
ordinary control flow *inside* that one `main` — a `match` on a string, calling an ordinary
`async fn`. Nothing here is special-cased by the compiler or the `tokio` runtime; it's the same
`if`/`match`-based dispatch you'd write for any other branching decision. What makes it look like
"picking a different program" rather than "an if-statement" is purely how *differently-shaped* the
work on each branch is (an HTTP server vs. a polling loop vs. a one-shot CLI command) — not any
Rust feature specific to multi-role binaries.

### 1.4.3 Reading the decision from the OS: `std::env::args()` / `std::env::var()`

```rust
// main.rs:645-648
let role = std::env::args()
    .nth(1)
    .or_else(|| std::env::var("CONTROL_PLANE_ROLE").ok())
    .unwrap_or_else(|| "serve".to_string());
```

These are the only two channels any process has, at the OS level, for learning how it was invoked:
the argument vector and the environment block, both handed to it by whatever started it (a shell, a
container runtime, `kubelet`). `std::env::args()` returns an iterator of `String`s (`.nth(1)` skips
`argv[0]`, the binary's own path, same as every C program's `argv` convention);
`std::env::var(...)` returns a `Result<String, VarError>`, turned into an `Option<String>` via
`.ok()` so a missing env var is just "try the next source," not an error. `.or_else(...)` and
`.unwrap_or_else(...)` are `Option` combinators — this whole four-line block is ordinary, unexceptional
Rust standard-library code; there's no framework or macro doing anything role-aware here.

### 1.4.4 `match` as an expression — why every role function has the identical shape

```rust
let result = match role.as_str() {
    "serve" => serve(state).await,
    "dispatcher" => run_dispatcher(state).await,
    "poller" | "reconciler" => run_reconciler(state).await,
    "a2a" => a2a::run(state).await,
    "notifier" => run_notifier(state).await,
    "replay" => run_replay(state).await,
    "mcp" => mcp::run(state).await,
    other => anyhow::bail!("unknown role {other:?} ..."),
};
```

In Rust, a `match` used as an expression (here, the right-hand side of `let result = ...`) must
have the *same type* on every arm — the compiler rejects it otherwise. That single rule is why
every one of these seven functions, despite doing completely unrelated work, was written with the
identical signature, verified directly:

```rust
async fn serve(state: AppState) -> anyhow::Result<()>            // main.rs:802
async fn run_dispatcher(state: AppState) -> anyhow::Result<()>   // main.rs:815
async fn run_reconciler(state: AppState) -> anyhow::Result<()>   // main.rs:751
async fn run_notifier(state: AppState) -> anyhow::Result<()>     // main.rs:870
async fn run_replay(state: AppState) -> anyhow::Result<()>       // main.rs:790
pub async fn run(state: AppState) -> anyhow::Result<()>          // a2a/mod.rs:219
pub async fn run(state: AppState) -> anyhow::Result<()>          // mcp/mod.rs:48
```

This is a form of structural polymorphism with no trait, no `dyn`, no generic — just seven
functions that happen to share one shape (`async fn(AppState) -> anyhow::Result<()>`), which is
*required* to exist because `match`-as-expression demands it. Nobody had to declare a
`trait Role { async fn run(self, state: AppState) -> Result<()>; }` for this to work; the type
system's unification rule for `match` arms enforces the same consistency a trait would, for free.

### 1.4.5 One move, decided at runtime — ownership meets exclusive control flow

`state: AppState` (file `10` covers its own internals) is passed **by value** into whichever branch
runs — `serve(state)` *moves* `state` into `serve`, consuming it. Six other branches also
"move `state` in." Rust's borrow checker is fine with this because it can see that `match` arms are
*mutually exclusive* — at most one arm's code ever executes for a given run of the program, so at
most one of those seven moves ever actually happens. The compiler doesn't need `Rc`/`Arc`-style
shared ownership here at all; "ownership of `state` goes to exactly one of these seven functions,
decided at runtime by a string comparison" is something the type system can verify statically,
*because* the control-flow structure (`match`) guarantees exclusivity. This is a small but genuine
example of Rust's ownership model and its control-flow analysis cooperating — the safety guarantee
("exactly one owner ends up with `state`") doesn't need a runtime check; `match`'s exhaustiveness
and exclusivity already prove it at compile time.

### 1.4.6 One runtime, set up once, regardless of which role runs

```rust
// main.rs:625-626
#[tokio::main]
async fn main() -> anyhow::Result<()> {
```

`#[tokio::main]` expands to ordinary `fn main()` that builds one `tokio` async runtime and blocks on
the `async` body you wrote — it runs exactly once, before role resolution even happens, and it has
no idea a concept of "roles" exists. Whichever role function is chosen, it executes as just another
future driven by that *same* runtime instance — an HTTP server future (`serve`), a `loop { ... }`
polling future (`dispatcher`), and a one-shot future that resolves and returns (`run_replay`'s
sweep-then-exit shape, if applicable) are all, to `tokio`, just "a future this process is currently
polling." There is no role-specific runtime configuration anywhere in this file.

### 1.4.7 Exit code *is* the role's `Result` — tying Rust's error propagation to Kubernetes' restart policy

```rust
// main.rs:690-691 (tail of main())
otel_guard.shutdown().await;
result
```

`main`'s own return value **is** the match's `result` — whatever `anyhow::Result<()>` the chosen
role function produced becomes the process's final return value, and `#[tokio::main]`'s generated
wrapper turns an `Err` into a non-zero process exit code (and a `panic!` into one too). This is the
one place the language mechanism and the deployment model are directly wired together: a role
"crashing" is nothing more exotic than an `async fn` returning `Err` (often via `?` propagating a
database error, a connection failure, anything) — and a non-zero exit from a container is exactly
what makes `kubelet` restart the pod. `serve`/`dispatcher`/etc. are written to run forever under
normal operation (an HTTP server future or a `loop` never resolves on its own); the *only* way any
of them "return" is an unrecoverable error, which is precisely the condition under which you want
Kubernetes to intervene. Nothing bespoke was built for "detect a stuck role and restart it" — it
falls out for free from `Result` propagation plus the orchestrator's own restart policy.

## 1.5 Role by role

### `serve` — the HTTP surface

```rust
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
```
(`main.rs:802-811`)

This is the thinnest role function in the file — nearly everything it needs is already in `app()`
(`main.rs:498-506`), which nests `api_v2_router()` (`main.rs:347-496`, the roughly 60-route surface:
webhook ingress, `/tasks`, `/admin/*`, and the whole `/internal/tasks/{id}/*` mediated-tool contract
the agent runner calls back into) under `/api/v2`. `serve` is the only role that scales freely behind
a load balancer — it holds no exclusive lease and nothing about it assumes one replica.

### `dispatcher` — claim-and-launch

```rust
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
```
(`main.rs:813-841`)

`run_dispatcher` is a thin env/config-resolution wrapper; the real loop is
`queue::dispatcher::run` (`services/control-plane/src/queue/dispatcher.rs:167-284`), covered fully
in file `02` (its `FOR UPDATE SKIP LOCKED` claim query is the centerpiece of that file). The headline
fact for *this* file: unlike `serve`, the dispatcher stands up its own tiny metrics-only HTTP
listener (`spawn_metrics_server`, `main.rs:910-935`) rather than a full axum app — it has no routes
of its own to serve, only `/healthz` and `/metrics` so Alloy can still scrape it. Every other
headless role (`reconciler`, `replay`, `notifier`) repeats this exact pattern.

Multiple dispatcher replicas *can* run concurrently — the module doc comment says so explicitly
(`queue/dispatcher.rs:1-8`): "Claiming uses `SELECT … FOR UPDATE SKIP LOCKED`, so any number of
dispatcher replicas can run concurrently without ever claiming the same task." It shares its loop
with the reaper, the index sweeper, the outbox sweeper, the A2A sweeper, and the webhook-payload
sweeper — all idempotent, so "singleton today" is a deployment choice, not a correctness
requirement.

### `reconciler` (née `poller`) — the sole GitHub egress

```rust
/// The reconciler role (ADR-0058): a single replica that owns **all platform egress** — it drains
/// the `outbox` and posts each intent (ADR-0059) — and reads 👍/👎 reactions back into
/// `review_feedback` (ADR-0035). Requires a database and at least one platform implementation; run
/// as ONE replica so egress isn't double-posted and reactions aren't double-polled.
async fn run_reconciler(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("reconciler requires DATABASE_URL"))?;
    let platforms = state.platforms.clone();
    if platforms.is_empty() {
        anyhow::bail!(
            "reconciler requires at least one platform implementation \
             (set GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY for GitHub, \
             or configure gitlab.projects[] / bitbucket.projects[] in control-plane.json)"
        );
    }
    spawn_metrics_server(state.metrics.clone());
    let interval = std::time::Duration::from_secs(reconciler_env_u64(
        "RECONCILER_INTERVAL_SECS",
        "POLLER_INTERVAL_SECS",
        300,
    ));
    let configured_days = reconciler_env_u64("RECONCILER_WINDOW_DAYS", "POLLER_WINDOW_DAYS", 14);
    if configured_days < 1 {
        tracing::warn!(
            configured_days,
            "RECONCILER_WINDOW_DAYS < 1 would disable the feedback poll; clamping to 1"
        );
    }
    let within_days = reconciler_window_days();
    queue::reconciler::run(pool, platforms, state.review.clone(), interval, within_days).await
}
```
(`main.rs:742-783`)

The role is the opposite of `dispatcher`'s "any number of replicas": it **must** run as exactly one
replica. The module doc comment for `queue::reconciler` (`queue/reconciler.rs:1-16`) states the
reason directly: "Single replica is load-bearing: it makes 'sole consumer' literal and keeps the
outbox's per-task ordering intact." [ADR-0059](../docs/adr/0059-reconciler-owns-all-github-egress.md)
is the decision this role exists to implement — before it, both `serve` and the poller wrote to
GitHub, which forced a settle-buffer hack to avoid double-posting. After it, there is exactly one
writer, and this function is where that writer's process starts.

Two accepted env-var names appear throughout this function (`RECONCILER_*` with `POLLER_*` as a
fallback) — the comment at `main.rs:718-719` explains why: "prefer `RECONCILER_*`; fall back to the
legacy `POLLER_*` so the rename doesn't require a simultaneous values change." This is the same
"accept both names" discipline the `"poller" | "reconciler"` match arm in §1.3 already showed at the
role-string level — [ADR-0058](../docs/adr/0058-rename-poller-role-to-reconciler.md)'s rename was
rolled out without forcing the binary and the Helm values to change in lockstep.

### `a2a` — the agent-to-agent ingress face

```rust
/// The `a2a` role entrypoint. Requires a database (the task queue) and OIDC (there is no anonymous
/// access). Serves the A2A surface on `A2A_BIND` and a metrics-only listener on `METRICS_ADDR`, like
/// the other headless roles.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    if state.db.is_none() {
        anyhow::bail!("the a2a role requires DATABASE_URL (it is the task queue)");
    }
    let jwt = state.jwt.clone().ok_or_else(|| {
        anyhow::anyhow!("the a2a role requires OIDC_ISSUER (no anonymous access)")
    })?;

    crate::spawn_metrics_server(state.metrics.clone());

    let addr = bind_addr();
    let router = build_router(state, jwt);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "a2a role listening (A2A v1.0.1: review skill, polling)");
    axum::serve(listener, router).await?;
    Ok(())
}
```
(`services/control-plane/src/a2a/mod.rs:219-235`)

This function lives in `a2a::run`, not `main.rs` — it's the one role whose entrypoint `main.rs`
calls directly by its module path (`"a2a" => a2a::run(state).await`) rather than through a local
`run_*` wrapper. The module's own doc comment (`a2a/mod.rs:1-19`) frames it as "a fourth ingress face
on the control plane (webhook, admin, internal, **A2A**) — not new execution (ADR-0029 holds; a
skill is a named entry point to existing behaviour)." It creates tasks through the exact same
`crate::db::create_task` path the webhook handler uses, and it explicitly "holds **no forge
credentials** — egress stays on the reconciler drain (ADR-0059)." Structurally it's a sibling of
`serve` (it binds its own HTTP listener and serves real routes), but it is a separate Deployment
because it's a different trust zone: an external A2A caller, authenticated by OIDC bearer token, not
a GitHub webhook signature.

### `notifier` — the first egress to arbitrary internet destinations

```rust
async fn run_notifier(state: AppState) -> anyhow::Result<()> {
    let pool = state
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("notifier requires DATABASE_URL"))?;
    spawn_metrics_server(state.metrics.clone());
    let key = a2a::push_token_key_from_env();
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
    let owner = std::env::var("HOSTNAME").unwrap_or_else(|_| "notifier".to_string());
    queue::notifier::run(pool, sender, key, policy, owner, cfg).await
}
```
(`main.rs:870-907`)

The comment right above the role's helper functions names what makes this role different from every
other one in the file (`main.rs:843-845`): it is "the SSRF-guarded webhook-delivery actor. It turns
the `a2a_task_events` log into ordered, at-least-once webhook POSTs to caller-registered URLs — the
control plane's first outbound egress to untrusted, arbitrary-internet destinations." Every other
role's egress targets a known, trusted endpoint (GitHub/GitLab/Bitbucket's API, the cluster's own
Neo4j/Postgres) — this is the one role that `POST`s to a URL an external caller supplied. That's
exactly why it carries its own SSRF policy (`a2a::ssrf::SsrfPolicy`), with a fixed set of always-blocked
ranges (loopback, RFC1918, link-local/metadata, ULA) plus operator-supplied extra CIDRs from
`A2A_PUSH_DENIED_CIDRS` for the specific cluster's Service/Pod ranges — the comment at `main.rs:884`
notes this is "the app-level half of the defence-in-depth; the deny-internal NetworkPolicy is the
network-level half."

### `replay` — the durable-step journal's lifecycle owner

```rust
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
    let retention = queue::replay::retention_from_env()?;
    // Headless role: stand up the metrics-only listener like the dispatcher/reconciler.
    spawn_metrics_server(state.metrics.clone());
    queue::replay::run(pool, retention).await
}
```
(`main.rs:790-799`)

This is the smallest role function in the file, and intentionally so: `durable_step` (file `02`
covers its schema) is **execution state, not domain data** — a crash-recovery journal for the agent
loop's `CheckpointRuntime`, opt-in and off by default in production. Most rows self-delete on success
on the write path itself; this role exists purely as the TTL backstop for rows that never got the
chance (an abandoned or failed run). Unlike `dispatcher`/`reconciler`, a second `replay` replica is
"harmless but pointless," not actively wrong — the sweep's delete is idempotent either way.

### `mcp` — the MCP server surface

```rust
pub async fn run(state: AppState) -> anyhow::Result<()> {
    // The MCP role requires a database and OIDC config.
    if state.db.is_none() {
        anyhow::bail!("the mcp role requires DATABASE_URL");
    }
    if state.jwt.is_none() {
        anyhow::bail!("the mcp role requires OIDC_ISSUER (no anonymous access)");
    }

    crate::spawn_metrics_server(state.metrics.clone());
    let addr = bind_addr();
    let quota = quota_from_env();

    let service: StreamableHttpService<handler::LightbridgeMcpHandler, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                let quota = quota.clone();
                move || {
                    Ok(handler::LightbridgeMcpHandler::new(
                        state.clone(),
                        quota.clone(),
                    ))
                }
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().disable_allowed_hosts(),
        );

    let protected =
        Router::new()
            .fallback_service(service)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::mcp_auth,
            ));
    let router = Router::new()
        .route(
            metadata::METADATA_PATH,
            get(metadata::protected_resource_metadata),
        )
        .merge(protected)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "mcp role listening");
    axum::serve(listener, router).await?;
    Ok(())
}
```
(`services/control-plane/src/mcp/mod.rs:48-106`)

Like `a2a::run`, this lives in its own module (`mcp::run`) rather than a `main.rs` wrapper. It is the
role external MCP clients (not the agent runner — a different, external integration surface) talk
to, serving the RMCP `StreamableHttpService` under an OIDC auth layer with one deliberate carve-out:
the RFC 9728 protected-resource metadata route (`metadata::METADATA_PATH`) is mounted *outside* the
`mcp_auth` layer, because "it must be readable without credentials or it cannot serve its purpose" —
the same reasoning `a2a` applies to its public agent-card route. `mcp_public_url` in `AppState`
(`main.rs:136-142`) exists specifically because this role's pod can't infer its own externally
reachable URL (Traefik strips the `/mcp` prefix before the request lands), so an operator has to
configure it explicitly — when it's unset, the RFC 9728 document simply isn't served rather than
guessing at a URL and shipping something wrong.

### `mint-runner-token` — not a role, a one-shot utility

```rust
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
```
(`main.rs:697-716`)

This is deliberately *not* in the role `match` — it's intercepted at `main.rs:660-664`, before
`AppState::from_env()` even runs, because it needs only `RUNNER_TOKEN_SIGNING_KEY` from the
environment and would otherwise be forced to also have a working `DATABASE_URL` for no reason. It
prints one token to stdout and exits — the point is purely developer/operator ergonomics: "without
it, running the agent-runner manually (docs/local-setup.md) has no way to obtain a token now that the
internal API rejects anything but a validly-signed, task-scoped one — there is no shared dev bearer
anymore" (comment at `main.rs:655-659`).

## 1.6 What to read next

- `02-postgres-schema-and-queue.md` — the `tasks` table this chapter's roles all read and write, the
  `FOR UPDATE SKIP LOCKED` claim query behind `dispatcher`, and the `outbox` table behind
  `reconciler`.
- `03-neo4j-structural-graph.md` — the structural store every role in this file is careful never to
  hand credentials to the untrusted agent Job for.
- `04-pgvector-and-dual-retrieval.md` — the semantic store, and why retrieval pins to a snapshot
  rather than a PR head.
- `10-appstate-and-rust-shared-state.md` — a deep dive on `AppState` (`main.rs:74`), the struct
  every role in `serve` ends up holding: what "application state" means generally, why Rust needs
  `Arc`/`Mutex` to share it across concurrent request handlers, and this struct's exact memory
  model, field by field.
