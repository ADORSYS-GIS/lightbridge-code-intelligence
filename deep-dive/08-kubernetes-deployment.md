# 08 — Kubernetes deployment

Two repositories cooperate to put this system on a cluster. **This repo**
(`lightbridge-code-intelligence`) contains the Rust code that *dynamically* builds a Kubernetes `Job`
manifest per task, in-process, at dispatch time. **`ai-helm`** (a sibling repo on this machine, at
`/home/christian/ai/ai-helm`) contains the Helm charts that deploy everything that is *not* a
per-task Job — the control-plane roles, Neo4j, and (in a separate chart) Postgres. This doc walks
both halves and is explicit about which pattern governs which piece, because the two patterns look
superficially similar (both produce Pods) but are built by entirely different mechanisms.

## 1. Why one Job per task (ADR-0004)

The decision, verbatim:

> Each review/Q&A task runs an agent that executes potentially untrusted reasoning over repository
> content (including PRs from forks). We need an execution model that isolates tasks from one
> another, cleans up reliably, and lets us scope credentials per task.
>
> Chosen option: **one Kubernetes Job per task.** Each task gets its own Job with
> `ttlSecondsAfterFinished` for cleanup, `activeDeadlineSeconds` for bounding, a least-privilege
> service account, and task-scoped credentials.
(`docs/adr/0004-one-k8s-job-per-task.md:8-30`)

The trade-off it explicitly accepts: per-task pod startup adds latency versus a warm shared worker
pool, in exchange for strong isolation (especially for untrusted fork PRs), predictable cleanup, and
per-task credential scoping.

## 2. The Rust side: `job_manifest` in `k8s.rs`

The dispatcher role (one of the control-plane binary's roles — see §4) builds the actual Job manifest
as a `serde_json::Value`, in `services/control-plane/src/integrations/k8s.rs`. The doc comment on the
function states the manifest's shape directly:

```rust
/// The per-task agent Job manifest (mirrors docs/kubernetes-deployment.md): `restartPolicy: Never`,
/// a TTL for cleanup, an active deadline to bound runtime, a least-privilege service account, and the
/// task id passed through so the runner can fetch its work and report back.
fn job_manifest(name: &str, cfg: JobConfig, task: &ClaimedTask) -> Value {
```
(`services/control-plane/src/integrations/k8s.rs:427-430`)

The skeleton it constructs:

```rust
let mut manifest = json!({
    "apiVersion": "batch/v1",
    "kind": "Job",
    "metadata": {
        "name": name,
        "labels": {
            "app.kubernetes.io/name": "lightbridge",
            "app.kubernetes.io/component": "agent",
            "app.kubernetes.io/part-of": "lightbridge-platform",
            "app.kubernetes.io/managed-by": "control-plane",
            "lightbridge.dev/task-id": task.id.to_string(),
        }
    },
    "spec": {
        "backoffLimit": 1,
        "activeDeadlineSeconds": active_deadline_seconds,
        "ttlSecondsAfterFinished": 900,
        "template": {
            "metadata": { "labels": { ... } },
            "spec": {
                "serviceAccountName": service_account,
                "restartPolicy": "Never",
                "volumes": volumes,
                "containers": [{
                    "name": "runner",
                    "image": image,
                    "env": env,
                    "volumeMounts": volume_mounts,
                }]
            }
        }
    }
});
```
(`services/control-plane/src/integrations/k8s.rs:575-616`, elided)

`activeDeadlineSeconds` defaults to one hour, tunable via `AGENT_JOB_DEADLINE_SECONDS` — the comment
notes indexing on a large repo has been observed to legitimately run over 15 minutes in production
(`services/control-plane/src/integrations/k8s.rs:590-594`). `ttlSecondsAfterFinished: 900` is the
separate post-completion cleanup window (15 minutes after the Job finishes, the Job and its Pod are
garbage-collected).

The env block is where runner-differentiation and the trust boundary actually get enforced in code.
`image_for` and `resources_for` (`services/control-plane/src/integrations/k8s.rs:389-425`) pick a
per-kind (`index` vs everything else) image/resources override with a shared fallback — so a chart
that only sets the shared values keeps single-image, single-resources behaviour, but an operator can
split `index` Jobs (heavier — full tree-sitter parse + embeddings + the in-process graph walk) from
`review` Jobs (lighter — read-mostly, reusing the base index per ADR-0025) onto different resource
tiers or even different image tags.

## 3. `harden_open_pod`: the one sandboxed Job kind

`open` is a (currently routing-admitted but dormant, per `services/agent-runner/src/run.rs:272-284`)
task kind that would let the agent write/execute, not just read and propose findings — the
highest-risk surface in the system, because it runs untrusted repo content *and* LLM-generated code.
Its Job manifest gets extra containment the `index`/`review` Jobs don't:

```rust
/// Apply the ADR-0088 sandbox posture to an `open` Job manifest. `open` executes untrusted repo +
/// generated code, so containment is at the pod boundary: non-root, all Linux capabilities dropped,
/// seccomp `RuntimeDefault`, no privilege escalation, a **read-only root filesystem** with a single
/// writable work `emptyDir` (the checkout root, wiped with the pod), and **no projected service-account
/// token** (the pod has no reason to reach the Kubernetes API).
fn harden_open_pod(manifest: &mut Value) {
    let pod = &mut manifest["spec"]["template"]["spec"];
    pod["automountServiceAccountToken"] = json!(false);
    pod["securityContext"] = json!({
        "runAsNonRoot": true,
        "seccompProfile": { "type": "RuntimeDefault" },
    });
    if let Some(volumes) = pod["volumes"].as_array_mut() {
        volumes.push(json!({ "name": "work", "emptyDir": {} }));
    }
    let container = &mut manifest["spec"]["template"]["spec"]["containers"][0];
    container["securityContext"] = json!({
        "runAsNonRoot": true,
        "allowPrivilegeEscalation": false,
        "readOnlyRootFilesystem": true,
        "capabilities": { "drop": ["ALL"] },
        "seccompProfile": { "type": "RuntimeDefault" },
    });
    if let Some(mounts) = container["volumeMounts"].as_array_mut() {
        mounts.push(json!({ "name": "work", "mountPath": "/workspace" }));
    }
}
```
(`services/control-plane/src/integrations/k8s.rs:631-665`)

`job_manifest` applies this conditionally: `if is_open { harden_open_pod(&mut manifest); }`
(`services/control-plane/src/integrations/k8s.rs:625-627`) — `index` and `review` Jobs never get this
treatment (and a test, `review_manifest_is_not_hardened`
(`services/control-plane/src/integrations/k8s.rs:1116-1135`), asserts exactly that).

The same `is_open` flag also keeps `open` credential-light **before** containment even applies: it
skips the embeddings credentials (`EMBEDDINGS_BASE_URL`/`_API_KEY`/`_MODEL`) and every indexer-tuning
env var entirely:

```rust
if !is_open {
    env.push(json!({ "name": "EMBEDDINGS_BASE_URL", ... }));
    env.push(json!({ "name": "EMBEDDINGS_API_KEY", ... }));
    env.push(json!({ "name": "EMBEDDINGS_MODEL", ... }));
}
```
(`services/control-plane/src/integrations/k8s.rs:503-513`)

So an `open` Job carries only the LLM key and its runner token — never a forge credential (those stay
on the control plane / reconciler), never the embeddings/index credentials the read-only paths use,
and (per `harden_open_pod`) never a Kubernetes API token either. Egress restriction itself (a
default-deny NetworkPolicy plus an allowlist for the LLM gateway, package registries, and the git
remote) is a deploy-side control that can't be expressed on the Job manifest — it's enforced
alongside the chart, not in this Rust function
(`services/control-plane/src/integrations/k8s.rs:634-637`).

## 4. The Helm side: `ai-helm/charts/lightbridge-code-intelligence`

This chart uses the **bjw-s app-template** pattern (aliased `lci` in this chart) — a generic Helm
library chart that turns a declarative `controllers:` map into Deployments/StatefulSets/etc, so this
chart's own `values.yaml` never writes raw Kubernetes YAML for its long-running workloads; it only
declares `type: deployment` / `type: statefulset` plus containers/env/probes/resources per controller
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:354` onward).

### 4a. Six control-plane roles, one image, six Deployments

The Rust `control-plane` binary is one image with a role selected at startup
(`std::env::args().nth(1)` or `CONTROL_PLANE_ROLE`, `services/control-plane/src/main.rs:645-648`).
The chart deploys **six** of these roles as **six separate Deployments**, all pointing at the same
`ghcr.io/adorsys-gis/lightbridge-control-plane` image:

| Controller | `type` | Role | Notes |
|---|---|---|---|
| `control-plane` | `deployment` | `serve` (implicit/default) | The HTTP surface (webhook, admin, internal, A2A card). `replicas: 1` — its GitHub delivery dedupe is still in-process memory, so scaling out would risk double-processing a webhook until that moves to Postgres. |
| `dispatcher` | `deployment` | `dispatcher` | Consumes the Postgres work queue (`SKIP LOCKED`) and launches the per-task Jobs described in §2-3. Runs under its own ServiceAccount (`lightbridge-ci-dispatcher`) holding Job-CRUD RBAC in the agents namespace. |
| `reconciler` | `deployment` | `reconciler` | The **single** replica that owns all GitHub egress — drains the outbox, posts reviews/replies/reactions, runs the ADR-0035 feedback poll. Holds the GitHub App key. One replica on purpose, so egress is never doubled. |
| `a2a` | `deployment` | `a2a` | The A2A surface described in `07-a2a-protocol.md`. `enabled: false` by default (inert until an operator provisions the Keycloak client); `replicas: 2` once enabled — it's stateless HTTP with the quota enforced Postgres-side. |
| `mcp` | `deployment` | `mcp` | `enabled: false` by default; `replicas: 2`. |
| `notifier` | `deployment` | `notifier` | The A2A push-delivery actor from `07-a2a-protocol.md` §5. `enabled: false` by default; `replicas: 1` (the per-config lease already serializes delivery, so RollingUpdate's brief overlap is accepted). |

(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:357-368, 576-589, 680-693, 779-787,
874-882, 968-985` — each entry's own `type: deployment` / `replicas:` / `strategy:` line.)

Every one of these is a plain `Deployment` object underneath the bjw-s abstraction. None of them is a
StatefulSet. None of them has a `volumeClaimTemplate`. They're stateless (or Postgres-backed-stateful,
in A2A's case) HTTP/queue-consumer processes that can be recreated and rescheduled freely.

### 4b. The one StatefulSet: Neo4j

Neo4j is the single exception to "everything here is a Deployment," and the chart's own comment says
so directly:

```yaml
# Neo4j knowledge graph (single-instance StatefulSet; 5.26 LTS).
neo4j:
  type: statefulset
  replicas: 1
  statefulset:
    serviceName:
      identifier: neo4j
    # /data via a StatefulSet volumeClaimTemplate (NOT a standalone PVC): the PVC is owned
    # by the StatefulSet and created WITH the pod, so it binds on schedule (WaitForFirstConsumer)
    # — no separately-ArgoCD-managed PVC to health-gate (sync-wave deadlock), no Replace churn,
    # no orphaned Terminating PVC.
    volumeClaimTemplates:
      - name: data
        accessMode: ReadWriteOnce
        size: 20Gi
        globalMounts:
          - path: /data
```
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:485-506`)

Why Neo4j specifically needs this and the six control-plane roles don't: Neo4j is the only one of
these seven workloads that owns durable local state tied to pod identity — the graph database's own
on-disk store. A Deployment's Pods are fungible and interchangeable; Neo4j's single pod needs the
*same* PVC back every time it (re)schedules, which is exactly what a StatefulSet's
`volumeClaimTemplate` guarantees (the PVC is created with the pod and keeps the same name across
reschedules) and a Deployment cannot.

The pod additionally pins `runAsUser`/`runAsGroup`/`fsGroup` to `7474` (the neo4j image's own uid/gid)
so the non-root process can write the PVC even on a storage class that provisions root-owned volumes
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:507-517`), and its JVM heap/page-cache are
sized explicitly (`1G` heap initial+max, `512M` page cache) rather than left to default to a fraction
of the container memory limit (`ai-helm/charts/lightbridge-code-intelligence/values.yaml:536-541`).
Resource requests/limits: `250m`/`512Mi` requested, `1` CPU / `2Gi` limit
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:561-563`). Its container runs with a
**writable** root filesystem — the one place this chart deliberately does *not* set
`readOnlyRootFilesystem: true` — because the official Neo4j image renders its config to
`/var/lib/neo4j/conf` at startup and is not read-only-rootfs compatible
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:564-571`).

Probes are plain TCP checks against the Bolt port (7687) rather than an HTTP health endpoint
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:542-558`).

### 4c. The per-task Job pattern is *dynamic*, not a Helm template

It's worth stating this explicitly because it's easy to conflate with the StatefulSet/Deployment
distinction above: the agent Jobs described in §2-3 are **not** rendered by this chart at all. There
is no `templates/job.yaml` here. They're constructed as JSON values, by Rust code, inside the
dispatcher Deployment's running process, one per claimed task, and submitted straight to the
Kubernetes API via the `kube` crate (`KubeLauncher::launch`,
`services/control-plane/src/integrations/k8s.rs:229`). The chart's only involvement is standing up the
`dispatcher` Deployment that contains this code, plus the RBAC (`templates/agents-rbac.yaml`) letting
that Deployment's ServiceAccount create/delete Jobs in the separate `agents.namespace`
(`lightbridge-agents` by default) they land in
(`ai-helm/charts/lightbridge-code-intelligence/values.yaml:65-66`).

So: **the StatefulSet is the one genuine exception to "this runs as a plain Deployment," and the
per-task Job pattern is a second, orthogonal axis — dynamic workloads created imperatively by Rust,
not a Helm-templated kind at all.** Six control-plane roles + Neo4j are the chart's own, Helm-declared
inventory; the Job-per-task pattern is a separate, code-driven thing the `dispatcher` controller does
at runtime.

## 5. Postgres: a separate chart, a CloudNativePG `Cluster` CR

Postgres is **not** in `lightbridge-code-intelligence` at all — it's reused from a shared CNPG
cluster defined in the sibling chart `ai-helm/charts/lightbridge-db`:

```yaml
# lightbridge-db — CNPG Cluster (lightbridge-main-db) + barman ObjectStore +
# ScheduledBackup + PodMonitor.
```
(`ai-helm/charts/lightbridge-db/values.yaml:1-2`)

The `Cluster` CR itself:

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: lightbridge-main-db
  namespace: converse
spec:
  instances: 2
  imageName: ghcr.io/cloudnative-pg/postgresql:18.4-system-trixie
  storage:
    size: "40Gi"
  ...
  resources:
    limits:
      cpu: 300m
      memory: 2Gi
    requests:
      cpu: 200m
      memory: 1Gi
```
(`ai-helm/charts/lightbridge-db/values.yaml:28-38, 150-157`, trimmed — the full file includes an
extensive incident narrative about why the storage size was raised from 5Gi, which is operational
history rather than part of this doc's scope)

This repo's code-intelligence data lives in a dedicated **role + database** on that shared cluster,
not a cluster of its own:

```yaml
# Dedicated login role for lightbridge-code-intelligence
# (charts/lightbridge-code-intelligence), isolated from `app`/`repoauth`.
# Owns the `codeintel` Database below; password from the basic-auth Secret
# `lightbridge-codeintel-db-role` (ESO-provisioned, below).
- name: codeintel
  ensure: present
  login: true
  passwordSecret:
    name: lightbridge-codeintel-db-role
```
(`ai-helm/charts/lightbridge-db/values.yaml:131-136`)

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Database
metadata:
  name: codeintel
  namespace: converse
spec:
  cluster:
    name: lightbridge-main-db
  name: codeintel
  owner: codeintel
  ensure: present
  extensions:
    - name: vector
      ensure: present
```
(`ai-helm/charts/lightbridge-db/values.yaml:360-373`)

The `vector` extension is declared right on the `Database` CR — this is the pgvector extension the
semantic index (`code_chunks`, `repo_has_index` from ADR-0025 in `06-the-review-loop.md`) depends on.

## 6. The summary picture

- **Most of the system is plain Deployments.** `control-plane` (serve), `dispatcher`, `reconciler`,
  `a2a`, `mcp`, `notifier` — six roles off one image, six `type: deployment` entries in
  `ai-helm/charts/lightbridge-code-intelligence/values.yaml`.
- **The one StatefulSet is Neo4j**, and specifically because it owns durable local state
  (`volumeClaimTemplates`) that must survive pod rescheduling with its identity intact — a need none
  of the stateless/queue-consuming control-plane roles have.
- **Postgres isn't in this chart at all** — it's a separate CloudNativePG `Cluster` CR in
  `ai-helm/charts/lightbridge-db`, shared with other tenants on the same cluster, with
  `lightbridge-code-intelligence` getting its own `codeintel` role + database + `vector` extension.
- **The per-task Job-per-review pattern (ADR-0004) is dynamic**, assembled as JSON by Rust code
  inside the `dispatcher` Deployment and submitted directly to the Kubernetes API — it is not a
  template this chart renders, and it is the mechanism, not the StatefulSet, that is the
  architecturally distinctive piece of this deployment: isolation and credential-scoping per task,
  not per long-running service.

## Verification notes

- Confirmed `harden_open_pod` is only invoked for `is_open` (`task.command_text == "open"`) by reading
  the conditional at `services/control-plane/src/integrations/k8s.rs:625-627`, then cross-checked
  against the test `review_manifest_is_not_hardened`
  (`services/control-plane/src/integrations/k8s.rs:1116-1135`), which asserts the hardening fields are
  absent on a `review` task's manifest.
- Confirmed the six-Deployments-plus-one-StatefulSet inventory by grepping
  `ai-helm/charts/lightbridge-code-intelligence/values.yaml` for every `type:` line under
  `controllers:` (`control-plane`, `neo4j`, `dispatcher`, `reconciler`, `a2a`, `mcp`, `notifier`) and
  reading each entry's `type:` value directly rather than assuming from the chart's prose comments.
- Confirmed Postgres lives in a separate chart by reading
  `ai-helm/charts/lightbridge-db/values.yaml`'s `Cluster` CR (`instances: 2`,
  `ghcr.io/cloudnative-pg/postgresql:18.4-system-trixie`, `storage.size: "40Gi"`) and the `codeintel`
  `Database`/role entries directly, rather than assuming from the `lightbridge-code-intelligence`
  chart's own (accurate) comment that Postgres is "REUSED from the CloudNativePG cluster."
