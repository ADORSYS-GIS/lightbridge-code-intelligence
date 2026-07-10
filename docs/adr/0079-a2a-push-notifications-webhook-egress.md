# ADR-0079: A2A push notifications via an SSRF-guarded webhook egress (RFC-0006 Phase 3)

- **Status:** Proposed
- **Date:** 2026-07-10
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0006](../rfc/0006-a2a-agent-surface.md) exposes Lightbridge's `review` agent over A2A. Phase 1
(card + `SendMessage` + `GetTask` + `CancelTask`, polling) is live (#308); Phase 2 (streaming over
an append-only per-task event log, [ADR-0077](0077-a2a-streaming-event-log.md)) is merged (#320).
This ADR designs **Phase 3 — push notifications**: instead of holding an SSE connection or polling,
a caller **registers a webhook** and the server **POSTs task updates to it** as they happen.

Today the config methods are hard-stubbed `push_notification_not_supported`
([`handler.rs`](../../services/control-plane/src/a2a/handler.rs)) and the card advertises
`capabilities.push_notifications: false` ([`card.rs`](../../services/control-plane/src/a2a/card.rs)).

Two forces dominate, and both are new to this codebase:

1. **This is the control plane's first *outbound* egress to caller-controlled, arbitrary-internet
   URLs.** The `a2a` role runs **inside** the `converse` namespace. An unvalidated webhook URL is a
   **server-side request forgery (SSRF)** primitive: a caller could register
   `https://10.0.0.5/…`, `http://169.254.169.254/latest/meta-data/…` (cloud metadata), or an
   in-cluster Service DNS name and turn our delivery pod into a probe into the cluster and the cloud
   fabric. RFC-0006 R3; the A2A spec makes SSRF defence a §13.2 SHOULD — **for us it is a MUST.**
   (Forge egress, by contrast, targets a *known, trusted* host set and rides credentials; this is a
   different, untrusted trust domain — see Option C.)
2. **Delivery must be durable, ordered, at-least-once, and scale-safe.** The `a2a` role runs
   `replicas: 2` and must stay horizontally scalable; a naive in-process POST loses events on
   restart, double-sends across pods, and cannot honour retry/backoff/dead-letter. Deep reviews run
   up to 2 h ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md)), so "fire the webhook and
   forget" is not acceptable.

The question: **how does the `a2a` role deliver ordered, at-least-once webhook notifications to
caller-registered URLs without becoming an SSRF vector or a source of lost/duplicate/out-of-order
posts — reusing the substrate we already run?**

## Decision Drivers

- **SSRF safety is the load-bearing requirement (R3).** The delivery actor is in-cluster; the blast
  radius of a miss is the whole namespace plus the cloud metadata endpoint. Validation must be
  structural and defence-in-depth (app-level **and** network-level), and hold at **both**
  registration and delivery (DNS rebinding).
- **Durable, ordered, at-least-once delivery** with retry / exponential backoff / dead-letter; no
  lost events, no pod-pinned state, no double-send across replicas.
- **Reuse the substrate:** the Phase-2 `a2a_task_events` log is already the ordered, durable,
  per-task projection of state — push is *another delivery* of it, not a new source. Mirror the house
  egress discipline ([ADR-0056](0056-control-plane-owns-the-posted-output.md) intent rows /
  [ADR-0059](0059-reconciler-owns-all-github-egress.md) single-writer /
  [ADR-0074](0074-restate-egress-pilot.md) `PlatformEgress` per-key serialization) **without sharing
  the forge egress pipe** (different trust domain).
- **Postgres-only (RFC-0006 R7).** Phases 1–3 carry no RFC-0005 (Restate) dependency; but the design
  is shaped so it can later migrate to a `WebhookEgress` virtual object exactly as forge egress did.
- **Spec conformance:** config CRUD (`Create/Get/List/Delete TaskPushNotificationConfig`, multiple
  configs per task), the notification payload, and an authentication mechanism the receiver can use
  to verify the call is really from us.
- **Trust boundary unchanged ([ADR-0029](0029-focused-review-not-generic-runner.md)):** push is a
  *read projection* delivered outward; it never reaches the Job, and the delivery actor holds **no
  forge credentials** and no internal-service reach beyond Postgres.

## Considered Options

- **A. An SSRF-guarded webhook-delivery actor driven by the `a2a_task_events` log, with a per-config
  delivery cursor and per-config serialized, ordered, at-least-once delivery (chosen).**
- **B. In-process best-effort POST on event append.** Reject: no durability (a restart drops
  in-flight notifications), double-send across `replicas: 2`, no retry/backoff/dead-letter, and the
  SSRF check races the connect.
- **C. Reuse the forge `outbox` / `PlatformEgress` path for webhooks.** Reject: forge egress targets
  a *trusted, credential-bearing, known* host set; caller webhooks are *untrusted, arbitrary-internet*
  destinations that require SSRF validation and network isolation the forge path neither has nor
  should have. Sharing the pipe couples two trust domains and would let a caller URL ride the same
  egress actor that holds GitHub App keys. **Mirror the discipline; do not share the pipe.**

## Decision Outcome

Chosen: **Option A.** A dedicated, network-isolated delivery actor turns the durable event log into
webhook POSTs, guarded by an SSRF policy enforced at both ends.

### 1. Config storage + CRUD

A new table **`a2a_push_configs`** (illustrative; final DDL lands with the implementing PR):

```sql
CREATE TABLE IF NOT EXISTS a2a_push_configs (
    config_id     UUID        PRIMARY KEY,
    a2a_task_id   UUID        NOT NULL REFERENCES a2a_tasks (a2a_task_id) ON DELETE CASCADE,
    url           TEXT        NOT NULL,             -- validated HTTPS, public-resolving (see §2)
    token_enc     BYTEA,                            -- caller's auth token, encrypted at rest (see §3)
    -- delivery state (the "outbox" — see §4): the log IS the queue, this is the per-config cursor
    delivered_seq BIGINT      NOT NULL DEFAULT 0,   -- highest a2a_task_events.seq delivered to this url
    attempts      INT         NOT NULL DEFAULT 0,   -- consecutive failed attempts on the next event
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    state         TEXT        NOT NULL DEFAULT 'active', -- active | disabled (dead-lettered)
    lease_owner   TEXT, lease_expires_at TIMESTAMPTZ,    -- single in-flight delivery per config
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by    TEXT        NOT NULL              -- caller identity (authz)
);
```

- The four handler methods replace their Phase-1/2 stubs: `create` (validates the URL per §2 **at
  registration** and rejects a private/invalid one *synchronously*, so a bad URL never reaches the
  DB), `get`, `list`, `delete`. All are **caller-scoped** — the same `load_owned` ownership check as
  `GetTask`, so a caller can register/read/delete webhooks only on *its own* tasks (an
  unknown/foreign task or config id → `TaskNotFound`, no existence leak). Multiple configs per task
  are allowed (spec).
- `ON DELETE CASCADE` ties configs to the task mapping, so the `a2a_tasks` TTL sweep (the #308 /
  [#321](https://github.com/vymalo/lightbridge-code-intelligence/issues/321) follow-up) reaps them
  with their parent.

### 2. The SSRF policy (the crux) — enforced at registration **and** delivery

A webhook URL is untrusted input pointing our in-cluster pod at an arbitrary address. The policy,
applied by a shared validator on **both** the `create` path and **every** delivery attempt:

- **HTTPS only.** Reject `http`, and any non-`https` scheme (`file`, `gopher`, `ftp`, …). No plaintext
  (also protects the auth token in §3).
- **Public-address resolution.** Resolve the host; reject if **any** resolved address is in a blocked
  range: loopback (`127.0.0.0/8`, `::1`), RFC 1918 (`10/8`, `172.16/12`, `192.168/16`), link-local
  **including the cloud-metadata IP** (`169.254.0.0/16` → `169.254.169.254`, `fe80::/10`), unique-local
  (`fc00::/7`), unspecified/broadcast/multicast, IPv4-mapped-IPv6 of any of these, **and the cluster's
  own Service/Pod CIDRs**. A literal-IP URL is validated the same way. This is the "reject RFC 1918,
  localhost, link-local" of RFC-0006 R3, widened to the metadata IP and the cluster CIDRs.
- **DNS-rebinding defence (re-validate + pin at delivery).** Validation at registration is necessary
  but not sufficient: a hostname that resolved public at `create` can resolve private at delivery
  (TOCTOU). So the delivery client **re-resolves, re-validates, and connects to the *validated,
  pinned* IP** (custom connector / `connect-to` the checked address), never to a re-resolution that
  could differ. The check and the socket target are the same address.
- **No redirect following** (or follow only to a re-validated public HTTPS host). A `302 → http://169.254.169.254`
  is the canonical SSRF bypass; the delivery client disables redirects.
- **Network isolation (defence in depth, RFC-0006 R3).** A dedicated **NetworkPolicy** on the
  delivery pod: allow egress to **Postgres + DNS + the public internet only**; **deny** the cluster
  Service/Pod CIDRs, the metadata endpoint, and the k8s API. If an app-level SSRF check ever has a
  bug, the network layer still refuses the internal connection. (This is why delivery is a *separate*
  pod — §4 — with a policy the request-serving `a2a` role cannot have, since it needs internal reach.)
- **Timeouts + caps.** 10–30 s connect+total timeout; response body is **not** consumed beyond the
  status line (we deliver, we don't read); a small max-header cap. Port restricted to 443 (a caller
  needing a non-standard HTTPS port is an explicit future allowlist).

### 3. Webhook authentication (the receiver verifies it is us)

Per spec, a `TaskPushNotificationConfig` may carry a `token` / `authentication`. On each delivery the
client presents it (e.g. `Authorization: Bearer <token>` or a caller-named header) so the receiver
can (a) confirm the call is from Lightbridge and (b) correlate it to the config. The token is
**caller-supplied, stored encrypted at rest** (`token_enc`), never logged, and only sent over the
HTTPS the policy guarantees. A stronger, optional mode — a short-lived **JWT signed by the `a2a`
role's key** with `aud` = the webhook origin — is reserved as a follow-up; the caller-token echo is
the Phase-3 baseline and is spec-sufficient.

### 4. Delivery mechanics — durable, ordered, at-least-once, scale-safe

The **`a2a_task_events` log is the durable queue**; each config carries its own **delivery cursor**
(`delivered_seq`). No second event store, no forge-outbox coupling.

- **Production is free.** Phase 2 already appends an ordered `a2a_task_events` row per status/artifact
  transition inside the `set_task_status` transaction. Push adds **nothing** to that hot path — it is
  a *consumer* of the same log. A config created mid-run starts at `delivered_seq = 0` and replays the
  task's history, or at the current head (a `create`-time choice; default: from the current head, so a
  late subscriber gets *future* updates — matching the webhook mental model — with an opt-in "replay
  from start").
- **A dedicated `notifier` role/Deployment** (a new `CONTROL_PLANE_ROLE`, mirroring how `reconciler`
  is a separate egress role) runs the delivery loop, so its restrictive NetworkPolicy (§2) is
  enforceable independently of the request-serving `a2a` pods. It:
  1. **Claims** an `active` config with work due (`delivered_seq < max(seq)` for its task **and**
     `next_attempt_at <= now()`) via `SELECT … FOR UPDATE SKIP LOCKED` + a lease — the same claim
     discipline as the dispatcher/reconciler, so **exactly one worker delivers a given config at a
     time** (no double-send across replicas, order preserved).
  2. **Delivers the next event(s)** past `delivered_seq` **in `seq` order** through the SSRF-guarded
     client, advancing `delivered_seq` after each success. Woken by a `LISTEN`/`NOTIFY` on new events
     (the same wake the streaming tail uses) with a bounded fallback poll for retries.
  3. **On failure:** increment `attempts`, set `next_attempt_at = now() + backoff(attempts)`
     (exponential with cap, as the reaper already computes); after `MAX_ATTEMPTS`, **dead-letter** the
     config (`state = 'disabled'`) and stop — a persistently-failing webhook is disabled, not retried
     forever, and the caller can re-create/re-enable it. `attempts` resets to 0 on success.
- **At-least-once, ordered.** A crash between a successful POST and the `delivered_seq` advance
  re-delivers that event (a duplicate, never a loss or a reorder). The payload therefore carries a
  **stable event id (`{a2a_task_id}:{seq}`)** so the receiver can dedupe; the calling guide documents
  the at-least-once + idempotency contract. Because `delivered_seq` advances strictly monotonically
  per config, a receiver sees the task's events **in order**.
- **Terminal + cleanup.** After the `final` event is delivered, the config has no more work; it is
  reaped with its `a2a_tasks` parent by the TTL sweep.
- **Restate-ready.** This is the pre-Restate form. Later, per-config serialized delivery maps 1:1 onto
  a **`WebhookEgress` virtual object keyed by `config_id`** (exactly as ADR-0074's `PlatformEgress`
  keys per installation) — the claim/lease is replaced by engine-guaranteed single-execution, with no
  change to the SSRF policy or the event-log source. Out of scope here; noted so Phase 3 doesn't
  paint Restate into a corner.

### 5. Card + wiring changes

- **`card.rs`:** `capabilities.push_notifications` → `Some(true)`.
- **`handler.rs`:** the four push-config methods implement §1; `create` runs the §2 validator
  synchronously. `list_tasks` **stays** `unsupported_operation` (Phase 4). Streaming/polling are
  untouched — a caller may register a webhook *and* stream *and* poll the same task; all three are the
  same projection of the same log and cannot disagree.
- **New:** the `notifier` role, its Deployment + NetworkPolicy (ai-helm), the `a2a_push_configs`
  migration, and the SSRF validator + hardened HTTPS client.

### Consequences

- **Good:** callers get server-push without holding a connection or polling; it reuses the Phase-2
  event log and the house egress claim discipline — no new datastore, Postgres-only (RFC-0006 R7).
- **Good:** SSRF is handled **structurally and in depth** — HTTPS-only + public-resolution +
  pinned-connect + no-redirects at the app layer, and a deny-internal NetworkPolicy at the network
  layer; validated at both registration and delivery.
- **Good:** ordering + at-least-once + no-double-send are properties of the per-config cursor + lease,
  not of hand-rolled fan-out; the delivery actor scales like the reconciler.
- **Bad:** the control plane gains its **first untrusted-destination egress domain** — a permanent
  SSRF-surface to keep hardened, plus webhook-token secret-at-rest handling. This is precisely why it
  is isolated into its own role + NetworkPolicy rather than added to an existing egress path.
- **Bad:** delivery amplification (configs × events × retries) and a hostile/slow receiver are new
  abuse vectors — bounded by per-task/per-identity config caps, timeouts, and dead-lettering.
- **Neutral:** one more Deployment (`notifier`) in the estate; it earns its keep by owning the
  isolated egress.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| P1 | **SSRF** — a webhook URL probes the cluster / cloud-metadata (`169.254.169.254`) / internal Services | Medium | **High** | HTTPS-only + public-address resolution (reject loopback/RFC1918/link-local+metadata/ULA/cluster CIDRs) **at registration and delivery**; re-resolve-and-**pin the connect** to the validated IP (DNS-rebinding); **no redirect following**; a dedicated **NetworkPolicy** denying internal + metadata egress on the delivery pod (defence in depth). This is the load-bearing mitigation of RFC-0006 R3 |
| P2 | **DNS rebinding / TOCTOU** — host resolves public at `create`, private at delivery | Medium | High | Validation is re-run at delivery and the socket connects to the *validated, pinned* address, not a fresh resolution; the check and the connection target are identical |
| P3 | **Redirect-to-internal bypass** — receiver returns `302 → 169.254.169.254` | Medium | High | Redirects are not followed (or only to a re-validated public HTTPS host) |
| P4 | **Webhook token leakage** (secret at rest / in logs) | Low | Medium | Token stored **encrypted** (`token_enc`), never logged, sent only over policy-guaranteed HTTPS |
| P5 | **Double-send across `replicas: 2`** | Medium | Low–Med | Per-config `FOR UPDATE SKIP LOCKED` + lease → exactly one in-flight delivery per config; `delivered_seq` advances once |
| P6 | **Duplicate delivery** (at-least-once: POST succeeded, cursor advance lost to a crash) | Medium | Low | Stable event id `{a2a_task_id}:{seq}` in the payload for receiver-side dedupe; documented idempotency contract |
| P7 | **Delivery amplification / hostile-or-slow receiver DoS** | Medium | Medium | Per-task + per-identity config caps; connect+total timeouts; bounded `MAX_ATTEMPTS`; **dead-letter (disable) a persistently-failing config**; no response body consumed |
| P8 | **Ordering violation** — receiver sees events out of order | Low | Medium | `delivered_seq` advances strictly monotonically per config; events delivered in `seq` order; a stuck event blocks *only* its own config (backpressure), never reorders |
| P9 | **Config-CRUD authz** — a caller registers a webhook on another caller's task | Low | High | All four methods are caller-scoped via `load_owned`; a foreign/unknown id is `TaskNotFound` |
| P10 | **Amplification of the event log** (push adds write load to the hot path) | Low | Low | Push is a **read consumer** of `a2a_task_events`; it adds nothing to `set_task_status`. Only the notifier's cursor/lease writes are new, and they are per-config, not per-event-per-subscriber |

## Out of scope (later phases / follow-ups)

- **Phase 4 — `input-required` + `ListTasks`** ([RFC-0006](../rfc/0006-a2a-agent-surface.md) Phase 4),
  gated on [ADR-0076](0076-restate-task-lifecycle-workflow.md) (Restate Phase B awakeables).
  `list_tasks` stays `unsupported_operation`.
- **The `WebhookEgress` Restate virtual object** — the post-Restate form of §4's per-config serialized
  delivery; a migration once RFC-0005 Phase A/B lands, not this ADR.
- **JWT-signed webhook auth** (§3) — the caller-token echo is the Phase-3 baseline; server-signed JWTs
  are a follow-up when a peer requires them.
- **Non-443 HTTPS ports / an egress allowlist mode** — a future explicit-allowlist knob if a real peer
  needs it.

## More Information

- [RFC-0006](../rfc/0006-a2a-agent-surface.md) — the A2A surface; §Phase 3 (push) and R3 (SSRF) are
  the sketch this ADR fills in.
- [ADR-0077](0077-a2a-streaming-event-log.md) — the `a2a_task_events` log this delivers from (the same
  ordered projection; push and streaming can never disagree).
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) / [ADR-0056](0056-control-plane-owns-the-posted-output.md)
  / [ADR-0074](0074-restate-egress-pilot.md) — the egress claim/serialization discipline this mirrors
  (in a *separate* trust domain) and the `PlatformEgress` pattern the future `WebhookEgress` follows.
- [ADR-0029](0029-focused-review-not-generic-runner.md) — the boundary this does not reopen (push is a
  read projection delivered outward, not operator-defined execution).
- Phase 1/2 code this extends: [`a2a/handler.rs`](../../services/control-plane/src/a2a/handler.rs)
  (the stubbed push-config methods), [`a2a/card.rs`](../../services/control-plane/src/a2a/card.rs)
  (`push_notifications`), [`a2a/store.rs`](../../services/control-plane/src/a2a/store.rs)
  (caller-scoped loads), [0026_a2a_task_events.sql](../../services/control-plane/migrations/0026_a2a_task_events.sql).
- [#321](https://github.com/vymalo/lightbridge-code-intelligence/issues/321) — the `a2a_tasks` TTL
  sweep whose cascade also reaps push configs.
