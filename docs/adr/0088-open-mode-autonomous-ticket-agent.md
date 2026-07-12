# ADR-0088: The `open` mode — the autonomous ticket-to-PR agent (write access, credential-light)

- **Status:** Proposed
- **Date:** 2026-07-12
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0007](../rfc/0007-control-plane-v2-planes.md) splits the runtime into planes; the
**agent-plane** ([ADR-0085](0085-agent-execution-plane.md)) is the one that runs an `AgentLoop`, and
its behavior is selected by a **mode**. Two modes exist today: `review` (read-only, comments on a
diff) and `index` (batch retrieval build). Both are, at bottom, *readers* — the agent surface has
never held write access to anything but a buffer of comments, and every side effect leaves through
the mediated internal API ([ADR-0037](0037-agent-acts-via-mediated-tools.md)).

This ADR records a third mode, **`open`**: a classical coding agent that picks up a **ticket** and
proposes a **pull request**. A human opens a GitHub **Issue** and **@mentions** the bot (the same
@mention plumbing the deep review already uses); the dispatcher picks the task and launches the
agent-plane in `mode=open`. The agent reads the ticket, investigates the repo, **edits code**,
**builds and tests its own change**, and produces a PR for a human to review and merge.

`open` breaks the "agent surface only reads" assumption in the two ways that matter most for
security. First, it **writes code** — LLM-generated edits to a real working tree. Second, and
worse, it **executes code** — it runs builds, test suites, formatters, and whatever tooling the
repository ships, over *untrusted repository content* and its *own untrusted generated code*. That
is a qualitatively different threat surface from anything the agent-plane has run before, and the
question this ADR answers is: **how does `open` get write-and-execute power without becoming the
place a forge credential or a cluster secret leaks from?** It never auto-merges — it *proposes*; a
human owns the merge decision.

## Decision Drivers

- **Arbitrary code execution is the dominant risk.** `open` runs untrusted repo code and untrusted
  LLM-generated code by design. Linux user namespaces isolate *files*, not *what code can do once it
  runs*; a shared multi-tenant worker cannot contain a build script that decides to exfiltrate or
  pivot. Containment must be at the pod boundary, not the process boundary.
- **Keep the highest-risk pod credential-light** (the ADR-0002 / ADR-0037 lineage). The pod that
  executes attacker-influenceable code is the *worst* place in the system to store a forge write
  token, a DB credential, or a cluster identity. Producing code and holding the credential to push
  it must be separated.
- **Inherit durability, don't re-invent it.** An `open` task can run for a long time (investigate →
  edit → build → test → iterate). A crash at minute 40 must resume, not restart —
  [ADR-0087](0087-durable-replay-checkpoint-runtime.md) already gives the loop replay; `open`
  should ride it, and the irreversible act (opening the PR) must be replay-safe.
- **Reuse the loop, swap the toolset.** `open` is `AgentLoop` + a *write-capable* tool set + `open`
  policies + an `open` prompt. It must not fork the loop; the mode×host matrix and the tool registry
  are the composition seams.
- **A human is the merge gate, always.** The agent proposes; it never merges. The PR is AI-authored
  code and must be reviewable as such, with the governance declaration attached.

## Considered Options

- **Option A — a `run-once` strongly-sandboxed per-task pod that commits to a local branch, and
  hands the branch to the egress plane to push and open the PR via the mediated internal API**
  (this ADR).
- **Option B — the same sandbox, but give it a scoped, short-lived forge write token** so it pushes
  and opens the PR itself. One fewer hop.
- **Option C — run `open` in a shared `serve` agent-plane worker**, the way a long-lived tenant
  hosts many `review` tasks.

## Decision Outcome

Chosen option: **Option A**, because it is the only option that both contains arbitrary code
execution *and* keeps forge credentials off the pod that runs it. The two other options each break
one of those two invariants (B keeps the pod but puts a credential on it; C keeps the credential
separation but abandons the containment).

`open` therefore resolves, in the [ADR-0085](0085-agent-execution-plane.md) **mode×host routing
matrix**, to a single unconditional rule:

> **`mode=open` → host=`run-once`, always. Never `serve`.**

This is not a tunable or a default that ops can relax under load; it is a security property of the
mode. `review` may run `serve` (a long-lived tenant is safe for read-only work); `open` may not,
because a shared tenant cannot sandbox one task's build script from another task's checkout, secrets,
or in-flight edits. `open` is the highest-risk surface in the system, and its host binding says so.

### The sandbox spec (treat this section with the weight of a role spec)

Every `open` task is a fresh, throwaway pod. The spec is the containment boundary, so it is written
as hard requirements, not guidance:

- **Non-root, no capabilities, seccomp on.** `runAsNonRoot`, all Linux capabilities dropped, the
  default (or a tightened) seccomp profile, `allowPrivilegeEscalation: false`, no host mounts, no
  service-account token projected (`automountServiceAccountToken: false` — the pod has no reason to
  talk to the Kubernetes API).
- **Read-only root filesystem, one writable scratch.** The root FS is read-only; the *only* writable
  surface is a per-task work `emptyDir` mounted at the checkout root. The agent's edits, the build's
  outputs, and the test artifacts all live there and are destroyed with the pod.
- **Egress-restricted network — allowlist, default-deny.** A NetworkPolicy (or equivalent CNI
  control) permits egress **only** to: (1) the **LLM gateway**, (2) an **allowlisted
  package-registry set** (the registries the build legitimately needs), and (3) the **git remote**
  for the clone. **All cluster-internal traffic is denied** — no control-plane DB, no internal
  services, no other pods, no metadata endpoint. The pod cannot reach anything it could pivot
  through even if the code it runs turns hostile.
- **Ephemeral and wiped.** The pod and its `emptyDir` are deleted on completion (success, abort, or
  failure). Nothing persists between tasks; blast radius is one task by construction.
- **Bounded.** CPU/memory `limits`, a **wall-clock budget**, and a **turn budget** cap runaway; the
  reaper (below) reclaims sandboxes that outlive their budget or lose their controller.

Note the deliberate contrast with [ADR-0085](0085-agent-execution-plane.md)'s `serve` host, whose
relaxed per-task isolation is justified *precisely because* those modes never execute
attacker-influenceable code. `open` does, so `open` gets the strong box back. This is the same
reasoning ADR-0082 used to keep the opengrep SAST parser in a sandboxed Job while the read-only
review loop moved to a shared worker — code execution over untrusted input pins you to the pod
boundary.

### The trust boundary — the agent-plane stays credential-light (the crux)

This is the load-bearing decision. **`open` does not hold forge write credentials, even though it
produces code.** The mechanism:

1. The sandbox clones the repo (over the allowlisted git remote), reads the ticket, investigates,
   edits files in its `emptyDir` workdir, and **builds and tests the change in-place**.
2. When the agent is satisfied, it **commits to a local branch inside the sandbox** and calls the
   terminal tool `propose_pr(title, body, base)`. Committing is a purely local `git` operation; it
   needs no forge credential.
3. `propose_pr` does **not** push. It hands the **branch (as a patch/bundle) + the PR metadata** to
   the **egress plane** — the `reconciler`, which already holds the forge App credentials for every
   other write the system performs — through the **mediated internal API**.
4. The **egress plane** pushes the branch and opens the PR against the forge. The sandbox never sees
   a forge token, never talks to `api.github.com`, and cannot push directly (the git remote in its
   allowlist is scoped to the fetch it needs, not authenticated write).

This is [ADR-0037](0037-agent-acts-via-mediated-tools.md) **extended from comments to code
changes.** ADR-0037 established that the agent *acts* by handing structured intents to the control
plane, which owns every credentialed write; the review agent's `add_review_comment` is such an
intent. `open`'s `propose_pr` is the same shape — the intent just carries a branch instead of a
comment body. The highest-risk, code-executing pod in the system holds **exactly** what `review`
holds and no more: the LLM gateway key and a task-scoped internal-API runner token. **No forge
credential. No DB. No cluster identity.**

**The tension, named honestly:** a `propose_pr` intent can be *large* — a multi-file diff is far
bigger than a comment body, and it travels over the internal API. This ADR adopts ADR-0082's
**offload rule** verbatim: the branch/patch is **content-hashed and offloaded to blob storage
(or Postgres large-object) via the internal API**, keyed by `(task_id, run_epoch)` + a step name;
the intent on the wire carries the **key + content hash**, not the bytes, so the egress plane
rehydrates exactly the branch the sandbox produced and replay can verify it. The mediated boundary
holds; only the transport gets a pointer instead of a payload.

### Durability — one loop, replay-safe PR proposal

`open` runs the same `AgentLoop` behind the same `StepRuntime` seam as every other mode, so it
inherits **CheckpointRuntime** replay from [ADR-0087](0087-durable-replay-checkpoint-runtime.md)
for free. An `open` task is long-lived and side-effect-heavy — investigate, edit, `run_command`
(build/test), iterate — and a crash mid-task resumes from the last journaled step rather than
re-cloning and re-reasoning from zero.

Two replay properties are specific to `open` and must hold:

- **The working tree is local ephemeral state** (the ADR-0082 replay trap). The `emptyDir` does not
  survive a pod restart, so — exactly as `review`'s `ensure_checkout` guard — the edit/build/test
  steps must reconstruct the workdir from the journaled sequence of edits (or re-clone + re-apply
  the journaled patch) rather than trusting that files are still on disk. A `run-once` pod that dies
  and is rescheduled rebuilds its workdir deterministically before continuing.
- **The PR-proposal step is idempotent by dedup key.** `propose_pr` is the irreversible act; a naive
  replay would open a **duplicate PR**. The step therefore carries a dedup key **`(task_id,
  run_epoch)`**, enforced at the egress plane (the same outbox dedup discipline ADR-0074/ADR-0082
  use): replaying the terminal step re-sends the *same* keyed intent, and egress recognizes it as
  already-proposed and returns the existing PR rather than opening a second one.

### The toolset — write-capable, sandbox-scoped

`open` composes the loop with a **write-and-execute** tool set. Every write and every execution is
confined to the sandbox `emptyDir`; nothing here is a mediated *forge* call (the only mediated forge
call is the terminal `propose_pr`):

- **`read_file`, `grep`, `find_files`** — read-only navigation of the checkout (shared with
  `review`).
- **`apply_patch` / `edit_file`** — writes, **only inside the sandbox workdir**. The tool rejects
  any path that escapes the checkout root; the read-only root FS is the backstop if it doesn't.
- **`run_command`** — a **sandboxed** build/test/tooling runner. It executes inside the same pod,
  under the same seccomp/non-root/egress-restricted posture, bounded by the per-command and
  wall-clock budgets. This is the tool that runs untrusted code; the sandbox *is* its safety.
- **Retrieval** — `vector_semantic_search` / `graph_*`, served by the control-plane. Note these are
  *read* queries; if the egress-deny policy forbids reaching the control-plane's retrieval endpoint
  from the sandbox, retrieval is brokered the same mediated way results flow back — the sandbox does
  not get a direct line to the DB. (The exact brokering is an [ADR-0085](0085-agent-execution-plane.md)
  detail; the invariant is *no direct cluster-internal reach*.)
- **Terminal tools** — `propose_pr(title, body, base)` (hands the branch to egress; the only
  credentialed side effect, and it happens *off* the pod) and `abort(reason)` (gives up cleanly,
  proposes nothing).

Contrast with `review`, whose toolset is **read-only + mediated comment tools** and which never
executes repository code. The mode is the toolset: same `AgentLoop`, different registry view.

## Consequences

- **Good:** the system gains an autonomous ticket→PR capability while keeping every forge credential
  on the egress plane — the highest-risk, code-executing pod holds no credential a compromise could
  weaponize. The ADR-0037 boundary that has survived every phase survives write access too.
- **Good:** arbitrary code execution is contained at the pod boundary, where it belongs — a poisoned
  repo or a hostile generated build script can, at worst, burn one throwaway pod's budget; it cannot
  reach the DB, another task, or a secret, because the network policy denies all of it.
- **Good:** `open` inherits durable replay for free (ADR-0087), so long autonomous runs survive
  eviction/OOM/deploy, and the dedup-keyed `propose_pr` makes the one irreversible act replay-safe.
- **Good:** one loop, one tool registry — `open` is a composition, not a fork; the mode×host matrix
  and the `StepRuntime` seam do the work.
- **Bad / accepted:** `open` reintroduces the per-task pod cost model that `serve` was meant to
  retire — a fresh strongly-sandboxed pod per ticket is heavier than a shared tenant. This is the
  price of containing code execution and is accepted deliberately; the mode×host rule encodes it.
- **Bad / accepted:** a large diff crosses the internal API. The offload rule keeps the wire payload
  bounded, but the branch bytes now live (briefly, content-hashed) in blob storage keyed by the run
  — one more place task-derived data transits, flagged not hidden.
- **Bad / accepted:** the agent can *add dependencies*. There is no way to let a coding agent write
  code and forbid it from touching a manifest; the compensating controls are the registry allowlist
  (it can only pull from vetted registries) and, decisively, the **mandatory human PR review** — a
  human reads the dependency change before merge. It is a control, not a boundary, and the ADR says
  so.
- **Neutral / to watch:** `open` is design-only in this ADR. It depends on RFC-0007's plane split
  and ADR-0085's host matrix landing first, and on ADR-0087's replay being real for the durability
  claims. Until those ship, `open` is a recorded shape, not a running mode.

### Governance — the PR is AI-authored code

The pull request `open` opens is **AI-authored code**, and the repo's AI-governance doctrine applies
to it in full. `propose_pr` **must** populate the PR body with the project's **AI Usage
Declaration** (what the agent did, what it verified — the build/test evidence it ran in the
sandbox), a **source-of-truth reference** (the triggering Issue `#id`), and a **Verification**
section citing the sandbox build/test results. A human owns the merge decision; the agent never
merges. This is not optional polish — a PR without the declaration is a governance-check failure by
the repo's own rules, and `open` generating it is the mechanism that keeps AI acceleration from
laundering unreviewed code into `main`.

### Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| O1 | **Prompt injection** from ticket text or repo content steers the agent to hostile actions (exfiltrate, pivot, open a malicious PR) | High | High | Egress-restricted sandbox (no secrets/DB/internal services reachable); no forge credential on the pod; mediated push via egress; **mandatory human PR review**; never auto-merge. Even a fully-hijacked agent can only produce a PR a human must read and approve. |
| O2 | **Resource / wall-clock runaway** — an unbounded edit/build/test loop burns compute or hangs | Medium | Medium | CPU/memory limits + wall-clock + turn budgets on the pod; the **reaper reclaims stale sandboxes** that outlive their budget or lose their controller. |
| O3 | **Supply-chain** — the agent adds a hostile or typo-squatted dependency | Medium | High | Registry **allowlist** on the sandbox egress (only vetted registries reachable); caught at the **human PR gate** by normal dependency review; the change is visible in the diff, not hidden. |
| O4 | **Poisoned repo crashes / exploits the sandbox** — a malicious build script or a parser bomb in repo content | Medium | Low | Per-task throwaway pod — blast radius is **one task by construction**; non-root/seccomp/read-only-root contains the process; crash just fails the task and reclaims the pod. |
| O5 | **Duplicate PR on replay** — the terminal step re-executes after a crash in the ack window | Low | Medium | `propose_pr` carries dedup key `(task_id, run_epoch)`; egress dedups (outbox discipline, ADR-0074/0082) and returns the existing PR. |
| O6 | **Large-diff transport** — an oversized branch inflates the internal-API payload | Low | Low | Offload rule: content-hashed branch to blob storage, key + hash on the wire; egress rehydrates and verifies. |
| O7 | **Standing internal-API runner token** on the sandbox is misused | Low | Medium | Task-scoped authz (ADR-0017 lineage) — the token acts only on the task it authenticated for; folds into the standing-token hardening surface [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243); no forge/cluster creds accompany it. |

## Pros and Cons of the Options

### Option A — sandboxed `run-once` pod + mediated egress push (chosen)

- Good: contains arbitrary code execution at the pod boundary **and** keeps forge credentials off
  the pod — the only option satisfying both invariants.
- Good: extends the proven ADR-0037 mediated-action boundary from comments to code with no new trust
  surface; inherits ADR-0087 replay.
- Bad: per-task pod cost; a large diff crosses the internal API (mitigated by the offload rule).

### Option B — sandbox holds a scoped, short-lived forge write token

- Good: one fewer hop — the sandbox pushes and opens the PR itself; no branch-over-API transport.
- Bad: **places a forge write credential on the single highest-risk, code-executing pod in the
  system** — the worst possible location for it. "Scoped and short-lived" narrows the blast radius
  but does not change *where* the credential lives; a prompt-injected agent with a push token can
  push. The mediated push (Option A) keeps the boundary intact at the cost of one hop, and the hop
  is cheap. Rejected on mechanism.

### Option C — run `open` in a shared `serve` agent-plane worker

- Good: reuses the long-lived tenant; no per-task pod cost.
- Bad: **cannot sandbox arbitrary code execution across tenants.** User namespaces isolate files,
  not what a build script does once it runs; one task's `run_command` shares a kernel, a network
  reach, and a pod identity with every co-resident task. A shared worker is safe for read-only
  `review` and batch `index` precisely because they never execute attacker-influenceable code —
  `open` does. Rejected on mechanism.

## More Information

- [RFC-0007](../rfc/0007-control-plane-v2-planes.md) — control-plane v2 plane split; the agent-plane
  and egress-plane this ADR wires together.
- [ADR-0085](0085-agent-execution-plane.md) — the agent-plane, its modes (`review` / `index` /
  `open`), and the **mode×host routing matrix** that expresses `open → run-once, always`.
- [ADR-0087](0087-durable-replay-checkpoint-runtime.md) — `CheckpointRuntime` replay behind the
  `StepRuntime` seam, which `open` inherits; the dedup-keyed terminal step pattern.
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — the mediated-tools trust boundary this ADR
  **extends from comments to code changes** (`add_review_comment` → `propose_pr`).
- [ADR-0082](0082-restate-durable-agent-runtime.md) — the durable-runtime lineage: the offload rule
  (content-hashed blob + verified pointer) reused here for the branch transport, and the "code
  execution over untrusted input pins you to the pod boundary" reasoning (opengrep-in-a-Job).
- [ADR-0002](0002-rust-control-plane-trust-boundary.md) / [ADR-0017](0017-agent-runner-control-plane-bootstrap.md)
  — the credential-separation and task-scoped-runner-token invariants `open` preserves.
- Standing-token hardening surface: [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243).
- AI governance (mandatory PR declaration + human-owned merge): the repo governance doctrine —
  <https://adorsys-gis.github.io/ai-governance/>.
