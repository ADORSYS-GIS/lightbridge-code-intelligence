# 10. `AppState` — shared application state, the general concept and this binary's exact memory model

Every role in file 01's `match` block eventually ends up holding one value: `AppState`
(`services/control-plane/src/main.rs:74`). This chapter explains what "application state" means as
a general pattern, why Rust's ownership rules make it a genuinely different problem than in a
garbage-collected language, and exactly how this codebase's `AppState` is laid out in memory, field
by field, with real code.

## 10.1 The general concept, independent of any language

Almost every long-running server needs a handful of things that are expensive or impossible to
recreate per request: a database connection pool, parsed configuration, a secret, an HTTP client,
maybe a small in-memory cache. "Application state" is just the conventional name for the bag of
these long-lived values that the *whole process* needs — as opposed to per-request state (a single
incoming HTTP request's headers and body), which lives and dies with that one request.

The hard part is never "where do I put these values" — it's "how does every concurrently-running
request handler get access to the *same* long-lived values at once, safely." In a
garbage-collected language (Python, JS, Java, Go) this is almost a non-problem: you build one
object, close over it or pass it around, and the GC keeps it alive as long as anything still
references it; if two requests mutate the same dict concurrently, nothing stops you at compile
time — you only find out at runtime, if you're unlucky enough to hit the race.

## 10.2 Why Rust can't do that by accident — and has to be explicit about it

Rust's ownership rules say: a value has exactly one owner, and you may have either (a) many
*read-only* borrows of it at once, or (b) exactly one *mutable* borrow — never both at the same
time. That rule is checked entirely at compile time and is what makes a huge class of concurrency
bugs impossible to even compile. But it also means a plain struct cannot simply be "handed to" an
unbounded number of concurrently-running async tasks (one per in-flight HTTP request) the way an
object reference can in a GC'd language — ownership has to go *somewhere*, and only one task can
own a plain value.

Rust's standard answer is two separate, composable types, and `AppState` uses both:

- **`Arc<T>`** ("atomically reference-counted") — shared *ownership*. Many places can hold an
  `Arc<T>` pointing at the same heap allocation at once; the compiler still only ever hands out
  `&T` (read-only) through it, never `&mut T`, so this alone does not reintroduce the race-bug
  problem — it only solves "many owners," not "many owners who need to mutate it."
- **`Mutex<T>`** (or `RwLock<T>`) — *interior mutability*, checked at runtime instead of compile
  time. Wrapping a value in a `Mutex` is how you tell the compiler "yes, I need `&mut T` through a
  shared reference; enforce the single-writer rule with a runtime lock instead of a compile-time
  borrow check."

The combination `Arc<Mutex<T>>` is the idiomatic Rust answer to "many concurrent owners, one of
whom may need to mutate it at a time" — and reading a struct's field types tells you, with zero
need to trace through the rest of the codebase, exactly which pieces of shared state can change at
runtime and which are fixed for the life of the process. That is the payoff of being explicit: the
type signature *is* the concurrency audit.

## 10.3 The real struct

```rust
// services/control-plane/src/main.rs:73-120 (abridged to the fields, comments kept — they state
// the reasoning for each one's exact type)
#[derive(Clone)]
pub struct AppState {
    pub github_webhook_secret: Arc<String>,
    pub seen_deliveries: Arc<Mutex<HashSet<String>>>,
    pub jwt: Option<Arc<JwtValidator>>,
    pub db: Option<PgPool>,
    pub allow_no_db: bool,
    pub github: Option<github::GithubApp>,
    pub gitlab: Option<gitlab::GitlabRegistry>,
    pub bitbucket: Option<bitbucket::BitbucketRegistry>,
    pub platforms: std::collections::HashMap<
        integrations::platform::Platform,
        Arc<dyn integrations::platform::CodePlatform>,
    >,
    pub runner_token_signer: Option<Arc<runner_token::RunnerTokenSigner>>,
    pub neo4j: Option<Arc<neo4rs::Graph>>,
    pub metrics: PrometheusHandle,
    pub review: Arc<config::ReviewSection>,
    pub knowledge_tools: Arc<config::KnowledgeToolsSection>,
    pub app_handle: Arc<String>,
    pub permissions_claim: Arc<String>,
    pub model_allowlist: Arc<Vec<String>>,
    // ... a few more Option<Arc<_>>/Arc<_> fields follow the same pattern
}
```

Reading just the types, without reading a single doc comment, already tells you the concurrency
shape of this whole service:

- **Every field except one is either a plain `Arc<T>`, an `Option<Arc<T>>`, or a cheap-to-clone
  handle (`PgPool`, `PrometheusHandle`, `bool`).** None of those can be mutated through `AppState`
  — they are set once in `AppState::from_env()` (`main.rs:145`) at process startup and are
  read-only for the rest of the process's life. A deployment's webhook secret, model allowlist,
  and GitHub App credentials do not change while the pod is running; the types enforce that.
- **Exactly one field, `seen_deliveries`, is `Arc<Mutex<HashSet<String>>>`.** That is the *only*
  piece of state in this entire struct that any request handler can mutate at runtime — and the
  type alone tells you that, before you've read a line of `webhook.rs`.
- **`Option<T>`/`Option<Arc<T>>` fields** (`jwt`, `db`, `github`, `gitlab`, `neo4j`,
  `runner_token_signer`, …) encode "this feature may or may not be configured for this
  deployment" directly in the type — a handler that needs Neo4j has to explicitly handle the
  `None` case (fail closed, usually a 503) rather than the code silently assuming it's always
  there.
- **`platforms: HashMap<Platform, Arc<dyn CodePlatform>>`** is built once, at startup, from
  whichever of GitHub/GitLab/Bitbucket are configured, and never mutated afterward either — it's a
  plain `HashMap`, not wrapped in a `Mutex`, precisely *because* nothing ever inserts or removes
  from it after construction; only `Arc<dyn CodePlatform>`'s shared ownership is needed, not
  interior mutability.

## 10.4 `Arc<T>` at the memory level

An `Arc<T>` is, concretely: one heap allocation holding `T` itself plus two atomic integers (a
strong count and a weak count), and every `Arc<T>` handle you hold is a pointer to that one
allocation. `.clone()`-ing an `Arc` does **not** copy `T` — it atomically increments the strong
count and copies the pointer. That's the entire operation: one atomic add, a few bytes copied. The
`T` itself — however large, a `String`, a `Vec<String>`, a `HashMap` — is allocated exactly once
and never duplicated no matter how many `Arc` handles end up pointing at it. `T` is only ever
actually deallocated when the strong count drops to zero, i.e. when the *last* `Arc<T>` handle
anywhere in the process is dropped.

This is why wrapping an immutable config value in `Arc` (`app_handle: Arc<String>`,
`model_allowlist: Arc<Vec<String>>`) is the idiomatic Rust move for "many places need read access
to this, and I don't want to clone the actual data every time": the `String`/`Vec` is built once at
startup, and every one of the (potentially thousands of) in-flight request handlers that ends up
holding a copy of `AppState` is really just holding another pointer to that one heap allocation,
plus a reference-count bump — not a fresh copy of the string's bytes.

## 10.5 Why only `seen_deliveries` needs a `Mutex` — and the real lock site

```rust
// main.rs:278 — constructed once, at startup
seen_deliveries: Arc::new(Mutex::new(HashSet::new())),
```

```rust
// http/webhook.rs:354-361 — the only place this field is ever locked and mutated
let is_new = state
    .seen_deliveries
    .lock()
    .expect("dedup lock poisoned")
    .insert(delivery_id.to_string());
if is_new {
    DeliveryResult::Accepted
} else {
    DeliveryResult::Duplicate
}
```

Two things worth noticing precisely here:

1. **This is `std::sync::Mutex`** (`main.rs:54: use std::sync::{Arc, Mutex};`), the plain
   OS-thread-blocking mutex from the standard library — not `tokio::sync::Mutex`, the async-aware
   one. That's a deliberate, correct choice, not an oversight: the critical section here is a
   single synchronous `HashSet::insert` call — no `.await` ever happens while the lock is held. A
   `std::sync::MutexGuard` held across an `.await` point would block the entire async executor
   thread (a real, classic Rust-async footgun); a guard that's acquired, used for one synchronous
   call, and dropped before the function's next `.await` is exactly the case `std::sync::Mutex` is
   correct and cheaper for.
2. **The comment on the struct field is explicit about *why* this field exists at all**
   (`main.rs:77-78`): it's "the fallback when no database is configured (dev)" — with a real
   Postgres pool present, deduplication instead goes through the `webhook_deliveries` table's
   `PRIMARY KEY` (file 02), which is itself safe across multiple replicas. `seen_deliveries` is
   explicitly an in-process, single-replica-only fallback — the doc comment and `allow_no_db`'s
   own comment (`main.rs:84-86`) both call out that relying on it with more than one pod replica
   would silently reintroduce a duplicate-task bug (RFC-0001 Phase 0).

## 10.6 How one `AppState` value reaches every concurrently-running handler

```rust
// main.rs:498-505
fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics_endpoint))
        .nest("/api/v2", api_v2_router())
        .layer(axum::middleware::from_fn(track_http_metrics))
        .with_state(state)
}
```

```rust
// main.rs:513 — one of dozens of handlers using the same extractor pattern
async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse { ... }
```

`Router::with_state` stores exactly one `AppState` value inside the router. For every incoming
HTTP request axum spawns an async task to handle it, and that task's `State<AppState>` extractor
argument is populated by **cloning** the router's stored `AppState` — which, per §10.4, is cheap:
copying a handful of pointers and bumping a handful of atomic counters, not deep-copying a secret
string, a `HashSet`, or a connection pool. `#[derive(Clone)]` on the struct is only *safe* to be
this cheap because every field was deliberately chosen to be an `Arc`-wrapped handle, an
`Option` of one, or something already internally reference-counted (`PgPool` is itself built on an
`Arc`-based connection pool under the hood).

So "shared state" here is slightly different from what it sounds like: there is no single
`AppState` struct instance that every request reaches into. Every concurrently-running request
handler gets its *own* `AppState` struct value — but almost every field inside that value is a
pointer into the *same* underlying heap allocations as every other handler's copy. Two requests
handled on two different executor threads at the same instant are never fighting over one struct;
they're each holding their own cheap bundle of pointers to data that is either permanently
read-only (no conflict possible) or guarded by the one `Mutex` that actually needs it.

## 10.7 The picture end to end

```
Process startup:
  AppState::from_env()  ──builds once──▶  github_webhook_secret: Arc<String>        (heap, once)
                                           seen_deliveries:       Arc<Mutex<HashSet>> (heap, once)
                                           db:                    PgPool             (heap pool, once)
                                           ... every other field, built once

  app(state) ──.with_state(state)──▶  Router owns ONE AppState

Per request (N requests running concurrently):
  Request 1's task:  State<AppState> ← clone #1  (N pointers + refcount bumps, no deep copy)
  Request 2's task:  State<AppState> ← clone #2  (same heap allocations, different clone)
  Request N's task:  State<AppState> ← clone #N

  Every clone's `seen_deliveries` Arc points at the SAME Mutex<HashSet> —
  only ONE task can hold that Mutex's lock at a time; every other field
  is read-only, so no lock is needed to read it from any clone.
```

This is the general pattern worth taking away, independent of this specific service: in Rust,
"application state shared across concurrent request handlers" is not one mechanism but a
composition of two — `Arc` for *who gets to own a reference to the long-lived data*, and
`Mutex`/`RwLock` only on the specific fields that genuinely need runtime-checked mutation. A
struct that is cheap to `Clone` because it's built almost entirely from `Arc` handles, with a
`Mutex` only where the comments explicitly justify one, is the idiomatic shape of "app state" in
any async Rust web service — not particular to axum or to this codebase, though this is a real,
verified instance of exactly that shape.
