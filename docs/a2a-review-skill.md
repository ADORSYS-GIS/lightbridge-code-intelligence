# Calling the A2A `review` skill

How to request a deep code review over Lightbridge's [A2A](https://a2a-protocol.org/latest/specification/)
surface (spec **v1.0.1**). This is the concrete calling guide for the `review` skill introduced in
[RFC-0006](rfc/0006-a2a-agent-surface.md) (Phase 1: card + `SendMessage` + `GetTask` +
`CancelTask`; Phase 2 adds **streaming** — `SubscribeToTask` + the streaming leg of `SendMessage`,
see [ADR-0077](adr/0077-a2a-streaming-event-log.md) and §6 below).

The `review` skill runs the **deep** tier through the *same* pipeline as an `@mention`: same
idempotency, same repo-approval gate, same mediated posting to the PR. The A2A task **additionally**
returns the summary + structured findings + a review-context part (the effective base/head SHAs, the
derived scope, and the posted-review permalink) to the caller — it does not replace the PR review.

The input contract (the authoritative schema) is also published inline in the agent card's `review`
skill `description`, so a peer that only reads the card can still construct a valid request.

---

## 1. Get a token

There is **no anonymous access**. Callers are Keycloak **service accounts** using the OIDC
client-credentials grant. The token must:

- carry audience **`code-intelligence`** (`aud`), and
- carry the **`a2a:review`** permission (ADR-0023) in the configured permissions claim.

```bash
TOKEN=$(curl -s \
  -d 'grant_type=client_credentials' \
  -d "client_id=$A2A_CLIENT_ID" \
  -d "client_secret=$A2A_CLIENT_SECRET" \
  -d 'scope=code-intelligence' \
  "$KEYCLOAK_BASE/realms/lightbridge/protocol/openid-connect/token" \
  | jq -r .access_token)
```

The token is a `Bearer` credential on every protected call. A missing/invalid token is `401`; a
transient JWKS outage is a retryable `503` (back off and retry). Authenticating fine but lacking
`a2a:review` is **not** a transport error — it comes back as a `TASK_STATE_REJECTED` task (see §5).

## 2. Discover the agent card (optional, public)

The card is public discovery — no auth:

```bash
curl -s "$A2A_BASE/.well-known/agent-card.json" | jq '.skills[] | select(.id=="review")'
```

It advertises the two transports (JSON-RPC preferred, then REST/HTTP+JSON), the OIDC security
scheme, and the `review` skill with its inline input schema + examples.

## 3. Submit a review (`SendMessage`)

The request object is carried in a **`data` part** of a **`ROLE_USER`** message. Both transports
serve the same handler.

### Request fields (the `data` object)

| Field     | Type                | Required | Default    | Meaning & effect |
|-----------|---------------------|----------|------------|-------|
| `skill`   | string              | no       | `"review"` | Skill selector. Only `"review"` exists in this phase; any other value → `UNSUPPORTED_OPERATION`. |
| `forge`   | string              | no       | `"github"` | Source forge: `"github"` or `"gitlab"`. Selects which platform `repo`/`pr` resolve against. |
| `repo`    | string              | **yes**  | —          | Repository slug `"owner/name"` (exactly one slash; surrounding whitespace trimmed). Must be an approved repo, else `REJECTED`. |
| `pr`      | integer \| string   | **yes**  | —          | PR/MR number, `> 0`. A JSON integer **or** a numeric string (`164` or `"164"`). Which change set to review. |
| `headSha` | string              | **yes**  | —          | The exact commit to review — the PR/MR head. The repo is checked out here and the review runs against it. Also accepted as `head_sha`. |
| `baseSha` | string              | no (**recommended**) | —  | The PR/MR **base** (target-branch) commit. **Present → diff-scoped review** of just the PR's changes; **absent → whole-working-tree review** at `headSha` (see [Scoping](#scoping-diff-vs-whole-tree)). Also accepted as `base_sha`. |
| `prompt`  | string              | no       | generic    | Free-text focus prompt, recorded as the run's intent and shown to the agent. Steers emphasis; does **not** change scope. |

`headSha` is **required**: this server holds no forge credentials and cannot resolve a PR head
itself. A submission without one is `REJECTED` (a null head would otherwise silently review the
repository's default branch and post a wrong review).

### Scoping: diff vs whole-tree

`baseSha` is optional but **strongly recommended** — it decides *what* the review looks at:

- **With `baseSha` → a diff-scoped review of the PR's changes.** The runner computes
  `git diff merge-base(baseSha, headSha)..headSha` — the same three-dot "Files changed" set the forge
  shows — and scopes the review (and where findings may land) to exactly those changed files. This is
  what you almost always want.
- **Without `baseSha` → a whole-working-tree review at `headSha`.** No diff can be computed, so the
  review falls back to auditing the *entire* repository snapshot at `headSha` — broader, unfocused, and
  **not** the PR's delta. Useful only when you genuinely want a full-tree audit rather than a PR review.

Why the caller must supply the base: the `a2a` role holds **no forge credentials**, so it cannot look
up a PR's base commit itself — it only passes through the SHAs you send. (`headSha`/`baseSha` also
accept the `head_sha`/`base_sha` snake_case aliases.)

A diff-scoped request (both SHAs):

```json
{ "skill": "review", "repo": "acme/api", "pr": "164",
  "headSha": "9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d",
  "baseSha": "1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04" }
```

Omit `baseSha` from the same request and you get a whole-tree review of the checkout at `headSha`
instead.

### JSON-RPC (preferred transport)

`POST /` with a JSON-RPC envelope. Method names are **PascalCase** (`SendMessage`). The envelope
`id` may be a **string or a number**.

```bash
curl -s "$A2A_BASE/" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "req-1",
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "b7c1e0a2-8f3d-4e5a-9c1b-2d4f6a8e0c13",
        "role": "ROLE_USER",
        "parts": [
          { "data": {
              "skill": "review",
              "forge": "github",
              "repo": "acme/api",
              "pr": "164",
              "headSha": "9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d",
              "baseSha": "1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04",
              "prompt": "Focus on the auth changes and the new migration."
          } }
        ]
      }
    }
  }'
```

Notes on the message envelope:

- `messageId` is **required** on the wire (any unique id the caller mints).
- `role` **must** be the ProtoJSON enum `"ROLE_USER"` — not `"user"`.
- The review object is the `data` part; a plain `text` part is not accepted (Phase 1 takes
  structured input).
- `contextId` is optional; supply one to group related tasks into a conversation, else the server
  mints one.

### REST binding (equivalent)

`POST /message:send` with the `SendMessageRequest` body (the JSON-RPC `params` above), e.g.:

```bash
curl -s "$A2A_BASE/message:send" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{ "message": { "messageId": "…", "role": "ROLE_USER", "parts": [ { "data": { "skill": "review", "repo": "acme/api", "pr": "164", "headSha": "9f2a…" } } ] } }'
```

### Response — `SUBMITTED`

Both transports return a **Task**. Over JSON-RPC it is `result.task`; over REST it is the top-level
`task`:

```json
{
  "task": {
    "id": "0190c3f2-....-....-....-............",
    "contextId": "0190c3f2-....-....-....-............",
    "status": { "state": "TASK_STATE_SUBMITTED" },
    "metadata": { "lb.underlyingTaskId": "0190c3ee-....-....-....-............" }
  }
}
```

- `task.id` is the **server-generated** A2A task id you poll with (§4).
- `metadata."lb.underlyingTaskId"` is your correlation handle onto the underlying review run (also
  visible in the review dashboards). The caller identity is never leaked back.

Malformed or unsupported requests are transport errors, not tasks: a body with no `data` part or a
bad `repo`/`pr` is `INVALID_PARAMS`; a non-`review` skill is `UNSUPPORTED_OPERATION`.

## 4. Poll to a terminal state (`GetTask`)

`GetTask` is the authoritative point read: poll it until the state is terminal. (Prefer **streaming**
— §6 — for long deep runs; polling stays fully supported and you may freely mix the two on one task.
Push notifications remain unsupported, Phase 3.)

JSON-RPC:

```bash
curl -s "$A2A_BASE/" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{ "jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": { "id": "'"$TASK_ID"'" } }'
```

REST: `GET /tasks/{id}`.

Tasks are **caller-scoped**: an unknown id, or another caller's id, is a clean `TaskNotFound` (no
existence leak).

### State mapping (A2A ↔ Lightbridge)

| A2A wire state              | Lightbridge status            | Terminal |
|-----------------------------|-------------------------------|----------|
| `TASK_STATE_SUBMITTED`      | `received`, `queued`, `waiting_for_index` | no |
| `TASK_STATE_WORKING`        | `running`, `posting_result`   | no |
| `TASK_STATE_COMPLETED`      | `succeeded`                   | yes |
| `TASK_STATE_FAILED`         | `failed`, `timed_out`         | yes |
| `TASK_STATE_CANCELED`       | `cancelled`                   | yes |
| `TASK_STATE_REJECTED`       | refused at the submission gate | yes |

An unrecognised underlying status maps to `WORKING` (never a terminal guess), so a new status
literal can never make a poller believe a still-running review has finished.

### Terminal `COMPLETED` — artifacts

On completion, `GetTask` returns a single `review` artifact with **three** parts, in order:

1. a **text** summary,
2. a **data** part carrying the structured findings (the ADR-0032 finding shape), and
3. a **data** `context` part echoing *what was actually reviewed* and where to find it:

```json
{
  "id": "0190c3f2-…",
  "contextId": "0190c3f2-…",
  "status": { "state": "TASK_STATE_COMPLETED" },
  "artifacts": [
    { "artifactId": "review", "name": "review",
      "parts": [
        { "text": "Reviewed 3 files. One P1 in the auth path…" },
        { "data": [ { "path": "auth.rs", "severity": "P1", "…": "…" } ], "mediaType": "application/json" },
        { "data": {
            "repo": "acme/api",
            "pr": 164,
            "baseSha": "1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04",
            "headSha": "9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d",
            "scope": "diff",
            "reviewUrl": "https://github.com/acme/api/pull/164#pullrequestreview-1234567890"
          }, "mediaType": "application/json" }
      ] }
  ]
}
```

The **context** part (part 3) closes the loop on the [scoping](#scoping-diff-vs-whole-tree) gotcha
and links straight to the posted review:

| Field       | Meaning |
|-------------|---------|
| `repo` / `pr` | The repository and PR/MR that were reviewed (echoed back). |
| `baseSha` / `headSha` | The base/head SHAs the run was **submitted with**. |
| `scope`     | **Derived from the *request*:** `"diff"` when a `baseSha` was supplied (a diff-scoped review of the PR's changes was requested), `"whole-tree"` when it was not (whole working tree at `headSha`). Tells you at a glance whether you asked for a PR review or a full-repo audit. **Caveat:** `diff` reflects what was *requested*, not a readback of the runner — with a base, the runner still falls back to a whole-tree review when `baseSha == headSha`, the diff is empty, or the diff can't be computed. `whole-tree` (no base) is always accurate. |
| `reviewUrl` | Permalink to the review posted on the PR — jump straight to it. `null` for older rows or if the forge omitted the URL. |

If the underlying run row is unavailable by the time you poll (a rare reap/delete race), unknown
fields come back `null` and only what survived (typically `reviewUrl`) is populated — the shape is
stable either way.

### Cancel (`CancelTask`)

`POST /tasks/{id}/cancel` (REST) or the `CancelTask` JSON-RPC method flips the underlying run to
cancelled (the runner's self-cancel poll then stops the Job). Cancelling an already-terminal task is
`TaskNotCancelable`.

## 5. Stream (`SubscribeToTask` / the streaming leg of `SendMessage`)

Deep reviews can run up to two hours, so instead of tight-loop polling you can **hold a connection and
watch the task progress**. Streaming is advertised on the card (`capabilities.streaming: true`) and
implemented as a durable, replayable, per-task **event log** ([ADR-0077](adr/0077-a2a-streaming-event-log.md)).

Two entry points:

- **`SubscribeToTask`** — subscribe to an existing task (e.g. one you submitted earlier). REST:
  `POST /tasks/{id}/subscribe`; JSON-RPC: `SubscribeToTask` with `{ "id": "<taskId>" }`.
- **The streaming leg of `SendMessage`** — submit *and* stream in one call. REST:
  `POST /message:stream`; JSON-RPC: `SendStreamingMessage`. It runs the same submission gate as
  `SendMessage` (approval / quota / `headSha`), then streams the created task (a REJECTED submission
  streams its single terminal event and closes).

Both return a Server-Sent-Events stream of `StreamResponse` frames. The **ordered** sequence is:

1. an initial **`task`** frame — the current `GetTask` snapshot (so a reconnect immediately re-grounds);
2. then, in strict order, **`statusUpdate`** frames for each state transition
   (`SUBMITTED → WORKING → …`) and, on completion, a **`artifactUpdate`** frame carrying the same
   `review` artifact `GetTask` returns (summary + findings + review-context);
3. the stream **closes** at the terminal state (`COMPLETED` / `FAILED` / `CANCELED` / `REJECTED`).

Guarantees (RFC-0006 R6):

- **Ordering is a property of the log**, not of your connection: every event has a per-task sequence
  number, and *every* subscriber — on any server replica — reads the same rows in the same order. Two
  concurrent subscribers see **identical, identically-ordered** events.
- **Reconnect = re-subscribe.** There is no server-side resume cursor to manage: a fresh
  `SubscribeToTask` replays the whole sequence from the start (the log outlives the connection), then
  joins the live tail — no events are lost across a dropped connection.
- **Streaming and polling never disagree.** Each event is appended in the *same database transaction*
  that flips the underlying status, so an event exists for every transition a poller could observe. You
  may freely mix `GetTask` and streaming on one task.

Timing note: for a **posted** review the full artifact is written asynchronously (after the PR post),
so a stream may close on `COMPLETED` *before* the artifact is available — do a single follow-up
`GetTask` to fetch it. The terminal status and the ordered progress are always delivered on the stream.

## 5b. Push notifications (`CreateTaskPushNotificationConfig`) — webhook callbacks

Instead of holding a stream or polling, you can **register a webhook** and have the server POST task
updates to it as they happen (advertised on the card as `capabilities.pushNotifications: true`,
[ADR-0079](adr/0079-a2a-push-notifications-webhook-egress.md)). Register one (or several) per task:

- **`CreateTaskPushNotificationConfig`** — JSON-RPC method / REST `POST /tasks/{id}/pushNotificationConfigs`,
  carrying `{ "taskId": "<taskId>", "url": "https://…", "token": "<optional-secret>" }`. `GetTask…`,
  `ListTask…`, and `DeleteTaskPushNotificationConfig` manage them; all are **caller-scoped** to your own
  tasks (a foreign/unknown task or config id is `TaskNotFound`, no existence leak).

The POST the receiver gets, and the delivery contract:

- **Body** — the same `StreamResponse` frame the stream emits (a `statusUpdate` or `artifactUpdate`
  object), so push and streaming deliver the identical payload for a given event.
- **Stable event id** — the `X-A2A-Notification-Id: {taskId}:{seq}` header. `seq` is the task's
  per-event sequence number (the same ordering authority the stream uses).
- **Ordered, at-least-once.** Events are delivered strictly in `seq` order; a stuck event blocks only
  its own webhook (backpressure) and is never skipped or reordered. Delivery is **at-least-once** — a
  crash between a successful POST and the server's cursor advance re-delivers that event (a duplicate,
  never a loss), so **dedupe on the `{taskId}:{seq}` id**.
- **Authentication.** If you set a `token`, every POST presents it as `Authorization: Bearer <token>`
  so you can verify the call is from Lightbridge. The token is stored **encrypted at rest** and sent
  only over the HTTPS the URL policy guarantees.
- **URL policy (SSRF).** The `url` **must be HTTPS on port 443 and resolve to a public address** — it
  is validated at registration *and* re-validated at every delivery (DNS-rebinding defence), the
  connect is pinned to the checked IP, and redirects are **not followed**. A non-HTTPS / private /
  loopback / link-local (incl. cloud-metadata) / cluster-internal URL is rejected.
- **Dead-lettering.** A webhook that keeps failing is retried with exponential backoff and, after
  repeated failures, **disabled** (dead-lettered) — re-create it once your endpoint is healthy.

## 6. Gotchas (learned the hard way)

- **`role` is `ROLE_USER`, not `user`.** All A2A v1.0.1 enums are ProtoJSON SCREAMING_SNAKE. Task
  states are likewise `TASK_STATE_*`.
- **`pr` accepts an integer *or* a numeric string.** `"pr": 164` and `"pr": "164"` both parse; the
  value must be `> 0`. Prefer the string form if your ProtoJSON codec is picky about number
  rendering.
- **`headSha` is required.** Omitting it yields `TASK_STATE_REJECTED` — this server can't resolve a
  head without forge credentials.
- **Omitting `baseSha` silently changes *scope*, not just detail.** With it you get a diff-scoped
  review of the PR's changes; without it the review runs against the *whole working tree* at `headSha`
  (a full-repo audit, not the PR delta) — see [Scoping](#scoping-diff-vs-whole-tree). Send `baseSha`
  for a PR review.
- **An unapproved / unknown / unprovisioned repo → `TASK_STATE_REJECTED`.** A2A is not a side door
  around the repo-approval gate (ADR-0063). Also rejected: missing `a2a:review`, or a per-identity
  deep-run **quota** breach.
- **The JSON-RPC `id` may be a string or a number.** Both are echoed back as sent.
- **`messageId` is mandatory** on the message envelope; mint any unique id.
- **Same PR via webhook and A2A dedups to one run** — an A2A review of a head already under a
  webhook-triggered review maps onto the existing run (`lb.underlyingTaskId` points at it).
- **Multi-tenant requests are unsupported** — a `tenant` on the request is refused.

## See also

- [RFC-0006 — A2A-compliant agent surface](rfc/0006-a2a-agent-surface.md) (design, phases, risks).
- Agent card: `GET /.well-known/agent-card.json` — the `review` skill `description` embeds the same
  input schema as a machine-readable JSON-Schema block.

### Future upgrade — a formal extension-based schema

The input schema is published inline in the skill `description` because A2A's `AgentSkill` has no
`inputSchema` field. A more formal, machine-discoverable route is to declare an A2A **extension**
URI in the card's `capabilities.extensions` and attach the JSON-Schema under that URI in the skill
metadata. That is deferred: the SDK's `AgentSkill` type currently exposes no `metadata` field to
hang it on, and inline-in-description is discoverable enough for Phase 1. Revisit when a peer
requires the formal extension form.
