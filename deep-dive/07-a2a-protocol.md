# 07 — The A2A protocol surface

A2A (Agent2Agent) is a fourth ingress face on the control plane — alongside the GitHub webhook, the
admin dashboard, and the internal runner API. It lets another agent (not a human, not a GitHub event)
request a deep PR review over a standard protocol (JSON-RPC and REST, with streaming and push
notifications), poll/subscribe for the result, and get back a structured summary + findings. All of
this lives under `services/control-plane/src/a2a/`.

## 1. Shape: one role, one skill, no new execution

The module doc states the architectural constraint up front:

```rust
//! A fourth ingress face on the control plane (webhook, admin, internal, **A2A**) — not new
//! execution (ADR-0029 holds; a skill is a named entry point to existing behaviour). It:
//!
//! - serves the public agent card at `/.well-known/agent-card.json` ([`card`]);
//! - serves JSON-RPC (`POST /`) + REST (`/message:send`, `/tasks/{id}`, …) via the SDK bindings,
//!   behind an OIDC auth layer ([`a2a_auth`]) ...
//! - creates deep-tier review tasks through the SAME path as the webhook handler
//!   ([`crate::db::create_task`]), enforcing the repo-approval gate and a per-identity quota, ...
//! - holds **no forge credentials** — egress stays on the reconciler drain (ADR-0059).
```
(`services/control-plane/src/a2a/mod.rs:6-18`)

So A2A doesn't add a new review pipeline; it adds a new *front door* to the same task queue the
webhook handler and the dispatcher already drive — complete with the same repo-approval and quota
gates, and the same GitHub-credential-free posture every other untrusted-adjacent surface in this
system has (ADR-0002: the control plane is the only thing holding write credentials).

## 2. The agent card: one skill, two transports

The public discovery document is served unauthenticated at `/.well-known/agent-card.json`
(`services/control-plane/src/a2a/mod.rs:151-152`). It advertises exactly **one skill** — `review` —
built by `build_agent_card`:

```rust
supported_interfaces: vec![
    AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_JSONRPC),
    AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_HTTP_JSON),
],
capabilities: AgentCapabilities {
    streaming: Some(true),
    push_notifications: Some(true),
    ...
},
```
(`services/control-plane/src/a2a/card.rs:80-93`)

Both transports — **JSON-RPC** (preferred; listed first per the A2A spec's ordering rule, "because
Bedrock AgentCore and most current peers speak it") and **REST/HTTP+JSON** — are served from the same
base URL. Streaming (`SubscribeToTask` + the streaming leg of `SendMessage`) and push notifications
are both live, not placeholders — the comments pin exactly which phase shipped each:

```rust
// Phase 2 (ADR-0077): streaming is live (SubscribeToTask + the streaming leg of SendMessage).
// Phase 3 (ADR-0079): push notifications are live — a caller registers a webhook via
// `CreateTaskPushNotificationConfig` and the `notifier` role POSTs task updates to it. No
// extended card.
```
(`services/control-plane/src/a2a/card.rs:84-87`)

The `review` skill's description embeds a full inline JSON-Schema (draft-07) documenting every
request field, because `AgentSkill` has no dedicated `inputSchema` field
(`services/control-plane/src/a2a/card.rs:21-24`). The required fields are `repo` (`owner/name`), `pr`
(the PR/MR number), and `headSha`; `baseSha` is optional but "STRONGLY RECOMMENDED" because its
presence is what scopes the review to a diff rather than the whole working tree:

```
"baseSha": { ... "description": "OPTIONAL but STRONGLY RECOMMENDED base commit SHA ... SCOPING
EFFECT: when present, the review is DIFF-SCOPED to just the PR's changes ... When ABSENT, no diff
can be computed and the review FALLS BACK to the WHOLE WORKING TREE at headSha ..." }
```
(`services/control-plane/src/a2a/card.rs:42`)

A caller can also attach a free-text `text` part alongside the structured `data` part — but the card
is explicit, and tested, that text sets **emphasis only**, never the target or scope:

```rust
assert!(
    desc.contains("emphasis only") && desc.contains("never the target or scope"),
    "description states text sets emphasis only, not target/scope"
);
```
(`services/control-plane/src/a2a/card.rs:260-263`)

This matters because this server holds **no forge credentials** — it cannot resolve "that PR Alice
mentioned" from prose; the target must arrive as exact, structured data.

Every call authenticates with a Keycloak OIDC client-credentials token, and the `review` skill
additionally requires the `a2a:review` permission (ADR-0023), enforced per-skill in the card's
`security_requirements` (`services/control-plane/src/a2a/card.rs:137-141`).

## 3. The auth layer: strip-then-inject, never trust an inbound header

`build_router` wraps the protected JSON-RPC + REST bindings in `a2a_auth`
(`services/control-plane/src/a2a/mod.rs:154-164`). The middleware does three things, in this order:

1. **Strip** any inbound `x-lb-a2a-caller` / `x-lb-a2a-perms` header — a client cannot pre-seed its
   own identity (`services/control-plane/src/a2a/mod.rs:172-175`).
2. **Validate** the bearer token against Keycloak's JWKS. A JWKS fetch failure maps to **503**, not
   401 — "a transient server-side condition, not a bad token," because most A2A SDKs treat 4xx as
   non-retryable and would otherwise give up on an IdP blip
   (`services/control-plane/src/a2a/mod.rs:187-195`).
3. **Inject** the verified `sub` and permissions list as those same internal headers, now carrying
   only what the token actually proved (`services/control-plane/src/a2a/mod.rs:197-211`).

The test suite proves the anti-spoof property directly: a request with a *valid* bearer plus a
forged `x-lb-a2a-caller: admin` header still reaches the downstream handler as the token's real
subject, never the spoofed one (`services/control-plane/src/a2a/mod.rs:333-384`).

## 4. The tables: `a2a_tasks`, `a2a_task_events`, `a2a_push_configs`

### 4a. `a2a_tasks` — and why it needs optimistic concurrency

The A2A SDK's `TaskStore::update(task)` trait method takes **no expected-version argument** — it is a
blind upsert by design. Under horizontal scaling (the `a2a` role runs with `replicas: 2`, see
`08-kubernetes-deployment.md`), two replicas can each `get` the same task, mutate their own copy, and
`update` — and without a version check, the second write silently clobbers the first. This is a
textbook **lost update**: replica A reads state X, replica B reads state X, A writes "working", B
(still holding its stale copy) writes "failed" — and the row ends up `failed` even though A's
"working" transition was the more recent, correct one, with no error raised anywhere.

The store's own doc comment names this exactly:

```rust
//! The SDK's `TaskStore::update(task)` takes **no expected version** (issue #299): it is a blind
//! upsert. Under horizontal scaling, two `a2a` replicas can each read a task, mutate their copy, and
//! write back — the second write silently clobbering the first (a lost update). We enforce optimistic
//! concurrency at the DB layer instead.
//!
//! The trick is to round-trip the version *through the task object itself*. [`PgTaskStore::get`]
//! stamps the row's current `version` into the returned task's metadata under [`LB_VERSION`]. ...
//! the UPDATE is `... WHERE version = <expected>` with a `version + 1` bump.
```
(`services/control-plane/src/a2a/store.rs:4-17`)

The column itself, from the migration:

```sql
CREATE TABLE IF NOT EXISTS a2a_tasks (
    ...
    version     ...  -- bumped on every update
    ...
);
```
The actual CAS write:

```rust
async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
    let expected = meta_i64(&task, LB_VERSION).ok_or_else(|| {
        A2AError::internal("a2a store: update without an expected version (call get first)")
    })?;
    ...
    let new_version: Option<i64> = /* step-wrapped */ sqlx::query_scalar(
        "UPDATE a2a_tasks \
         SET task_json = $2, state = $3, underlying_task_id = COALESCE($4, underlying_task_id), \
             version = version + 1, updated_at = now() \
         WHERE a2a_task_id = $1 AND version = $5 \
         RETURNING version",
    )
    .bind(id).bind(&json).bind(&state).bind(underlying).bind(expected)
    .fetch_optional(pool).await ...
    match new_version {
        Some(v) => Ok(v as TaskVersion),
        None => {
            // Zero rows matched: either the id is unknown, or a concurrent writer moved past
            // `expected` — distinguish so a genuine conflict is reported as such.
            ...
        }
    }
}
```
(`services/control-plane/src/a2a/store.rs:219-289`, trimmed)

The race this prevents is proven directly in a test: two readers observe version 1; writer A updates
first and wins at version 2; writer B's update (still carrying `expected = 1`) is rejected with a
conflict error rather than silently overwriting A's write; a fresh read lets B retry successfully
against version 2 (`services/control-plane/src/a2a/store.rs:455-491`,
`cas_update_lets_one_writer_win_no_lost_update`).

One more subtlety: `get()` re-derives the `underlyingTaskId` metadata from the authoritative
`underlying_task_id` **column**, not from the (possibly stale) serialized `task_json` blob — because
the FK is `ON DELETE SET NULL`, and without this resync a later `update()` could write back a dead
UUID and hit a foreign-key violation (`services/control-plane/src/a2a/store.rs:292-299`).

### 4b. `a2a_task_events` — the append-only streaming log

A2A streaming must deliver an ordered event sequence that every concurrent subscriber on any replica
sees identically, survives a reconnect with no lost events, and closes cleanly at the terminal state
(spec §3.5.2 / RFC-0006 R6). The migration's own comment explains why this is a table rather than an
in-process fan-out:

```sql
-- We satisfy R6 STRUCTURALLY with this table rather than with in-process fan-out (which cannot
-- survive `replicas: 2`): ordering is a property of the log, not of any subscriber or pod.
```
(`services/control-plane/migrations/0026_a2a_task_events.sql:9-10`)

`seq` — not `created_at` — is the sole ordering authority, assigned inside the appending transaction
as `COALESCE(MAX(seq), 0) + 1`, because wall-clock time is unreliable across replicas and under ties
(`services/control-plane/migrations/0026_a2a_task_events.sql:20-24`). `(a2a_task_id, seq)` is the
primary key, so a duplicate seq is a hard constraint violation (retryable), never a silent gap.

### 4c. `a2a_push_configs` — webhook registration + delivery cursor

A caller can register a webhook instead of polling or holding an SSE connection open. This table is
both the config store **and** the per-config delivery cursor into the event log above:

```sql
-- the config store AND the per-config delivery cursor/outbox: the append-only `a2a_task_events` log
-- (ADR-0077, migration 0026) IS the durable queue; each config carries its own `delivered_seq`
-- cursor into that log, so push is a *consumer* of the event log — it adds nothing to the
-- `set_task_status` hot path.
```
(`services/control-plane/migrations/0027_a2a_push_configs.sql:6-9`)

Key columns: `delivered_seq` (the cursor), `attempts` + `next_attempt_at` (backoff), `state`
(`active`/`disabled` — a persistently-failing webhook is dead-lettered), and `lease_owner` /
`lease_expires_at` (claimed via `SELECT … FOR UPDATE SKIP LOCKED` so exactly one notifier replica
delivers a given config at a time, even with `replicas: 2`)
(`services/control-plane/migrations/0027_a2a_push_configs.sql:33-51`).

## 5. The `notifier` role's SSRF policy

A webhook URL is caller-controlled input that points the cluster's own egress at an arbitrary
address — this is the control plane's **first** outbound egress to a destination the caller chose,
and therefore a textbook SSRF primitive. A malicious caller could register
`https://169.254.169.254/` (the cloud metadata IP) or an in-cluster Service address and turn the
delivery pod into a probe against the cluster/cloud fabric. `src/a2a/ssrf.rs` is the app-layer half of
the defence (a deny-internal NetworkPolicy on the delivery pod is the other half, at the deploy
layer):

```rust
//! This module is the **app-layer** half of the defence ... It is deliberately **pure and
//! exhaustively testable**: the DNS resolver is *injected* ... The same validator runs at
//! **registration** and at **every delivery attempt** (DNS-rebinding / TOCTOU defence — ADR-0079 §2, P2).
```
(`services/control-plane/src/a2a/ssrf.rs:9-13`)

The check pipeline, in order: parse → scheme must be exactly `https` → host present → port must be
443 → resolve the host → **every** resolved address must be public (`validate_webhook_url`,
`services/control-plane/src/a2a/ssrf.rs:101-153`). If a domain resolves to a mixed set
`[public, private]`, the *whole* URL is rejected — the code never cherry-picks the public address
out of a split-horizon/DNS-rebinding set (`services/control-plane/src/a2a/ssrf.rs:141-147`, verified
by the test `any_blocked_address_rejects_the_whole_set`).

The fixed deny table (`is_blocked_v4` / `is_blocked_v6`) is **hand-rolled and auditable on purpose** —
the module doc insists `ipnet` backs only the *extra* operator deny-list, never the fixed ranges, "so
a reviewer must be able to see every range" (`services/control-plane/src/a2a/ssrf.rs:15-17`). It
blocks: `0.0.0.0/8`, loopback (`127.0.0.0/8`), RFC 1918 private ranges, **link-local** (which
deliberately includes the cloud-metadata IP `169.254.169.254` — called out as "the single most
important address to block" in its own test,
`services/control-plane/src/a2a/ssrf.rs:511-515`), CGNAT (`100.64.0.0/10`), the benchmarking range
(`198.18.0.0/15`), multicast, broadcast, and reserved space on the v4 side
(`services/control-plane/src/a2a/ssrf.rs:194-235`); unspecified, loopback, ULA (`fc00::/7`),
link-local (`fe80::/10`), multicast, and the documentation range (`2001:db8::/32`) on the v6 side
(`services/control-plane/src/a2a/ssrf.rs:240-280`). It also unwraps every IPv4-in-IPv6 transition
form — mapped/compatible, NAT64, 6to4, Teredo — and re-checks the embedded v4, so
`::ffff:169.254.169.254` or a NAT64-wrapped private address can't slip past a "that's a global-looking
v6" NetworkPolicy (`services/control-plane/src/a2a/ssrf.rs:269-317`).

### `A2A_PUSH_DENIED_CIDRS`: extending the fixed table

The fixed ranges alone don't know your cluster's Pod/Service CIDR if it happens to live outside RFC
1918 (unusual, but possible). `A2A_PUSH_DENIED_CIDRS` is the operator's way to extend the deny-list
with exactly those addresses, parsed at `notifier` startup:

```rust
/// Parse a comma-separated CIDR list; blanks are skipped, a malformed entry is an error (fail-closed).
fn parse_denied_cidrs(raw: &str) -> anyhow::Result<Vec<ipnet::IpNet>> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty())
        .map(|s| s.parse::<ipnet::IpNet>()
            .map_err(|e| anyhow::anyhow!("invalid CIDR {s:?} in A2A_PUSH_DENIED_CIDRS: {e}")))
        .collect()
}
```
(`services/control-plane/src/main.rs:858-868`)

A malformed entry is a **hard startup error** — the notifier refuses to run with a gap in the SSRF
policy rather than silently deliver without it. An unset env var degrades to an empty extra list (the
fixed ranges still apply) but logs a warning naming the risk explicitly:

```rust
tracing::warn!(
    "notifier: A2A_PUSH_DENIED_CIDRS is unset — only the fixed SSRF ranges apply. A non-RFC1918 \
     cluster CIDR would be reachable; set it (+ the deny-internal NetworkPolicy) before enabling \
     push in such a cluster."
);
```
(`services/control-plane/src/main.rs:888-892`)

The resulting `SsrfPolicy` is threaded into `queue::notifier::run` alongside the push-token decryption
key (`services/control-plane/src/main.rs:880-906`) — the `notifier` role is the actor that actually
drains `a2a_push_configs` and performs the deliveries this policy guards.

## Verification notes

- Confirmed the agent card advertises exactly one skill (`review`) and both transports by reading
  `build_agent_card` directly (`services/control-plane/src/a2a/card.rs:72-154`), not the RFC prose.
- Confirmed the optimistic-concurrency CAS mechanism end-to-end: read the `update()` SQL
  (`WHERE a2a_task_id = $1 AND version = $5`), then read the test
  `cas_update_lets_one_writer_win_no_lost_update` (`services/control-plane/src/a2a/store.rs:455-491`)
  which actually exercises two concurrent writers off one stale read and asserts the loser gets
  `error_code::INVALID_REQUEST`, not a silent overwrite.
- Confirmed the SSRF fixed-range table and the IPv4-in-IPv6 unwrapping logic by reading
  `is_blocked_v4`/`is_blocked_v6`/`embedded_v4s` directly (`services/control-plane/src/a2a/ssrf.rs:194-317`),
  and cross-checked against their own test cases (e.g. `blocks_ipv6_transition_forms_embedding_internal_v4`)
  rather than trusting the doc comment's claim alone.
