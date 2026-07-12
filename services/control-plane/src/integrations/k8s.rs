//! Task execution: turn a claimed task into one Kubernetes Job (ADR-0004).
//!
//! The dispatcher owns *which* task runs and *when*; the actual (potentially untrusted) work runs in
//! an isolated, per-task Job with TTL cleanup and task-scoped credentials. [`TaskLauncher`] is the
//! seam: `KubeLauncher` creates real Jobs in production, while tests build the manifest and assert
//! its shape without a cluster.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ServiceAccount;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use serde_json::{Value, json};

use crate::db::ClaimedTask;

/// Liveness of a task's Kubernetes Job, as the reaper reads it (RFC-0001 Phase 2). The Job — not a
/// timer — is the source of truth for whether a `running` task is still doing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLiveness {
    /// Still running or pending — do not reclaim.
    Active,
    /// Finished successfully (Job condition `Complete`).
    Succeeded,
    /// Finished unsuccessfully (Job condition `Failed` — e.g. `DeadlineExceeded`, backoff exhausted).
    Failed,
    /// No Job by that name exists (deleted, TTL-expired, or never created).
    Gone,
}

/// Launches the execution of a claimed task and returns the created Job's name; also lets the reaper
/// inspect and clean up Jobs by name.
#[allow(async_fn_in_trait)] // crate-internal trait; never used across crates or as `dyn`.
pub trait TaskLauncher {
    async fn launch(&self, task: &ClaimedTask) -> anyhow::Result<String>;
    /// Current liveness of a previously-launched Job (by name).
    async fn job_liveness(&self, job_name: &str) -> anyhow::Result<JobLiveness>;
    /// Delete a Job (background propagation). A missing Job is success — the call is idempotent.
    async fn delete_job(&self, job_name: &str) -> anyhow::Result<()>;
}

/// Creates one Kubernetes Job per task via the cluster API.
pub struct KubeLauncher {
    jobs: Api<Job>,
    /// The agent-runner image. Shared fallback for both kinds when a per-kind image below is unset.
    image: String,
    /// Image for **index** Jobs (`command_text == "index"`); falls back to `image`. Shares the one
    /// lean runtime with review now that Graphify is retired (ADR-0086). See [`image_for`].
    indexer_image: Option<String>,
    /// Image for **review** Jobs (everything else); falls back to `image`. Same lean runtime as the
    /// index image (ADR-0086); override kept so operators can pin the kinds separately. See [`image_for`].
    review_image: Option<String>,
    service_account: String,
    /// In-cluster URL the runner calls back for context + status (the control plane's own Service).
    control_plane_url: String,
    /// Shared bearer the runner presents to that internal API (ADR-0017). Injected into the Job so
    /// the runner can authenticate; empty when unset (the internal API is then disabled anyway).
    runner_token: String,
    /// Secret (in the agents namespace) holding the internal CA (`ca.crt`) the runner must trust to
    /// reach the eaig gateway's HTTPS embeddings endpoint. `None` → no CA volume mounted.
    ca_secret: Option<String>,
    /// The Job's `activeDeadlineSeconds` — its hard runtime cap. From `AGENT_JOB_DEADLINE_SECONDS`
    /// (default 3600 = 1h); large repos can legitimately index for tens of minutes (#51).
    active_deadline_seconds: i64,
    /// Optional override for the reviewer's system prompt, injected into each agent Job as
    /// `REVIEW_SYSTEM_PROMPT` so operators can tune review behaviour without a rebuild. `None` → the
    /// runner uses its built-in default guidance.
    review_system_prompt: Option<String>,
    /// ConfigMap (agents namespace) with the runner's `agent.json` + prompt templates, mounted at
    /// `/etc/lightbridge` in each Job. `None` → not mounted (runner falls back to env).
    agent_config_map: Option<String>,
    /// The runner container's `resources` block (requests/limits), set verbatim when present. Shared
    /// fallback for both kinds when a per-kind block below is unset.
    resources: Option<Value>,
    /// Resources for **index** Jobs (`command_text == "index"`, the heavy path); falls back to
    /// `resources`. See [`resources_for`].
    indexer_resources: Option<Value>,
    /// Resources for **review** Jobs (everything else, read-mostly); falls back to `resources`.
    review_resources: Option<Value>,
    /// Handle to the agent ServiceAccount, used to resolve its UID for the Job `ownerReference`.
    sa: Api<ServiceAccount>,
    /// Lazily-resolved Job `ownerReference` (to the agent SA) for k8s GC + traceability. A
    /// [`OnceCell`] so the SA UID is fetched on first launch and cached; until it resolves, each
    /// launch retries (a not-yet-created SA or transient API error at startup must not permanently
    /// leave Jobs un-owned). `get_or_try_init` only holds its internal lock during the one-time init,
    /// not across every launch. Owner must be the same-namespace SA (k8s forbids cross-namespace
    /// ownerRefs), not the cross-ns dispatcher.
    owner_reference: tokio::sync::OnceCell<Value>,
}

/// The default Job runtime cap when `AGENT_JOB_DEADLINE_SECONDS` is unset or unparseable.
const DEFAULT_JOB_DEADLINE_SECONDS: i64 = 3600;

/// Parse `AGENT_JOB_DEADLINE_SECONDS` into a positive deadline, falling back to the default on
/// absent/empty/zero/negative/unparseable input (a bad value must not silently disable the cap).
fn parse_deadline_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_JOB_DEADLINE_SECONDS)
}

/// A config value: the file's value (if non-empty) wins, else the env var, else `default`.
fn pick(file: Option<&str>, env: &str, default: &str) -> String {
    file.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| default.to_string())
}

/// Like [`pick`] but optional: file value, else env var, else `None`.
fn pick_opt(file: Option<&str>, env: &str) -> Option<String> {
    file.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
}

impl KubeLauncher {
    /// Build from the ambient Kubernetes config, taking each knob from the file config's `agent`
    /// section when set, else the matching env var, else a built-in default (RFC-0001 / ADR-0021).
    pub async fn resolve(agent: Option<&crate::config::AgentSection>) -> anyhow::Result<Self> {
        let client = Client::try_default().await?;
        let namespace = pick(
            agent.and_then(|a| a.namespace.as_deref()),
            "AGENT_NAMESPACE",
            "lightbridge-agents",
        );
        let image = pick(
            agent.and_then(|a| a.runner_image.as_deref()),
            "AGENT_RUNNER_IMAGE",
            "ghcr.io/vymalo/lightbridge-agent-runner:latest",
        );
        let indexer_image = pick_opt(
            agent.and_then(|a| a.indexer_runner_image.as_deref()),
            "AGENT_INDEXER_RUNNER_IMAGE",
        );
        let review_image = pick_opt(
            agent.and_then(|a| a.review_runner_image.as_deref()),
            "AGENT_REVIEW_RUNNER_IMAGE",
        );
        let service_account = pick(
            agent.and_then(|a| a.service_account.as_deref()),
            "AGENT_SERVICE_ACCOUNT",
            "lightbridge-agent",
        );
        // Where the runner calls back. Defaults to the in-cluster Service name the ai-helm chart
        // gives the serve role; override via config or CONTROL_PLANE_INTERNAL_URL.
        let control_plane_url = pick(
            agent.and_then(|a| a.control_plane_url.as_deref()),
            "CONTROL_PLANE_INTERNAL_URL",
            "http://lightbridge-ci-control-plane:8080",
        );
        let runner_token = std::env::var("AGENT_RUNNER_TOKEN").unwrap_or_default();
        let ca_secret = pick_opt(
            agent.and_then(|a| a.ca_secret.as_deref()),
            "AGENT_CA_SECRET",
        );
        let active_deadline_seconds = agent
            .and_then(|a| a.job_deadline_seconds)
            .filter(|&secs| secs > 0)
            .unwrap_or_else(|| {
                parse_deadline_secs(std::env::var("AGENT_JOB_DEADLINE_SECONDS").ok().as_deref())
            });
        let review_system_prompt = pick_opt(
            agent.and_then(|a| a.review_system_prompt.as_deref()),
            "REVIEW_SYSTEM_PROMPT",
        );
        let agent_config_map = pick_opt(
            agent.and_then(|a| a.config_configmap.as_deref()),
            "AGENT_CONFIG_CONFIGMAP",
        );
        let resources = agent.and_then(|a| a.resources.clone());
        let indexer_resources = agent.and_then(|a| a.indexer_resources.clone());
        let review_resources = agent.and_then(|a| a.review_resources.clone());
        Ok(Self {
            // The SA's UID (for the Job ownerReference) is resolved lazily on first launch, not here:
            // at startup the SA may not exist yet (Helm install ordering) or the API may be briefly
            // unavailable, and resolving once would then leave Jobs permanently un-owned.
            sa: Api::namespaced(client.clone(), &namespace),
            jobs: Api::namespaced(client, &namespace),
            image,
            indexer_image,
            review_image,
            service_account,
            control_plane_url,
            runner_token,
            ca_secret,
            active_deadline_seconds,
            review_system_prompt,
            agent_config_map,
            resources,
            indexer_resources,
            review_resources,
            owner_reference: tokio::sync::OnceCell::new(),
        })
    }
}

/// Read the agent ServiceAccount's UID and shape it into a Job `ownerReference`. Returns `Err` (so the
/// caching `OnceCell` retries on the next launch) when the SA can't be read or has no UID yet — a
/// startup race we want to recover from, not cache as "un-owned forever".
async fn fetch_owner_reference(sa: &Api<ServiceAccount>, sa_name: &str) -> anyhow::Result<Value> {
    use anyhow::Context;
    let account = sa
        .get(sa_name)
        .await
        .with_context(|| format!("reading agent ServiceAccount {sa_name}"))?;
    let uid = account
        .metadata
        .uid
        .with_context(|| format!("agent ServiceAccount {sa_name} has no uid yet"))?;
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "name": sa_name,
        "uid": uid,
        // Not a controlling owner (the reaper/TTL manage lifecycle); just a GC + trace link that must
        // not block the SA's own deletion.
        "controller": false,
        "blockOwnerDeletion": false,
    }))
}

impl TaskLauncher for KubeLauncher {
    async fn launch(&self, task: &ClaimedTask) -> anyhow::Result<String> {
        let name = job_name(task);
        // Resolve the SA ownerReference lazily + cached via `OnceCell`: fetched on first launch (when
        // the SA exists), retried each launch until it succeeds, then reused — so a startup race or
        // transient API error doesn't permanently leave Jobs un-owned. The internal lock is held only
        // during the one-time init, never across every launch's `.await`.
        let owner_reference = match self
            .owner_reference
            .get_or_try_init(|| fetch_owner_reference(&self.sa, &self.service_account))
            .await
        {
            Ok(owner) => Some(owner),
            Err(error) => {
                tracing::warn!(%error, "could not resolve agent SA ownerReference; Job created un-owned");
                None
            }
        };
        let manifest = job_manifest(
            &name,
            JobConfig {
                image: image_for(
                    task,
                    self.indexer_image.as_deref(),
                    self.review_image.as_deref(),
                    &self.image,
                ),
                service_account: &self.service_account,
                control_plane_url: &self.control_plane_url,
                runner_token: &self.runner_token,
                ca_secret: self.ca_secret.as_deref(),
                active_deadline_seconds: self.active_deadline_seconds,
                review_system_prompt: self.review_system_prompt.as_deref(),
                agent_config_map: self.agent_config_map.as_deref(),
                resources: resources_for(
                    task,
                    self.indexer_resources.as_ref(),
                    self.review_resources.as_ref(),
                    self.resources.as_ref(),
                ),
                owner_reference,
            },
            task,
        );
        let job: Job = serde_json::from_value(manifest)?;
        match self.jobs.create(&PostParams::default(), &job).await {
            Ok(_) => Ok(name),
            // The Job name is derived from the unique task id, so a 409 means *our own* Job already
            // exists — e.g. a previous attempt created it but we crashed before recording job_name,
            // or the create timed out after the apiserver accepted it. Adopt it instead of erroring,
            // which would requeue and 409 forever (dispatch is at-least-once).
            Err(kube::Error::Api(error)) if error.code == 409 => {
                tracing::warn!(job_name = %name, task_id = %task.id, "job already exists; adopting it");
                Ok(name)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn job_liveness(&self, job_name: &str) -> anyhow::Result<JobLiveness> {
        let Some(job) = self.jobs.get_opt(job_name).await? else {
            return Ok(JobLiveness::Gone);
        };
        // Terminal state is carried in the Job's status conditions (`Complete` / `Failed`, status
        // "True"); absent either, it's still active.
        let conditions = job.status.as_ref().and_then(|s| s.conditions.as_ref());
        let is_true = |cond_type: &str| {
            conditions.is_some_and(|cs| {
                cs.iter()
                    .any(|c| c.type_ == cond_type && c.status == "True")
            })
        };
        Ok(if is_true("Complete") {
            JobLiveness::Succeeded
        } else if is_true("Failed") {
            JobLiveness::Failed
        } else {
            JobLiveness::Active
        })
    }

    async fn delete_job(&self, job_name: &str) -> anyhow::Result<()> {
        match self
            .jobs
            .delete(job_name, &DeleteParams::background())
            .await
        {
            Ok(_) => Ok(()),
            // Already gone — that's the desired end state.
            Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Deterministic, collision-free Job name for a task (the task id is unique).
fn job_name(task: &ClaimedTask) -> String {
    format!("lightbridge-agent-{}", task.id)
}

/// The launcher-derived inputs to a Job manifest — everything that isn't the task itself. Grouped so
/// `job_manifest` takes a handful of args, and so a test can build one explicitly without a cluster.
struct JobConfig<'a> {
    image: &'a str,
    service_account: &'a str,
    control_plane_url: &'a str,
    runner_token: &'a str,
    ca_secret: Option<&'a str>,
    active_deadline_seconds: i64,
    review_system_prompt: Option<&'a str>,
    agent_config_map: Option<&'a str>,
    resources: Option<&'a Value>,
    owner_reference: Option<&'a Value>,
}

/// Pick the resource block for a task's kind. **Index** Jobs (`command_text == "index"`) are the heavy
/// path — full tree-sitter parse + embeddings + in-process lci-codegraph — and want more CPU/RAM; **review** Jobs are
/// read-mostly (they reuse the latest indexed snapshot, ADR-0050, and are LLM/network-bound) so they
/// run leaner. Each kind falls back to the shared `resources` when its own block is unset, so a chart
/// that sets only `resources` keeps today's behaviour.
///
/// CAVEAT: a *cold-repo* review still self-indexes (ADR-0050 only makes *warm* reviews reuse the
/// snapshot — `repo_indexed` is false until `latest_indexed_commit` exists, and the runner indexes
/// when `!repo_indexed`). So `review_resources` must still be able to absorb a one-off full index —
/// **do not set it below what a cold index needs.** In practice the approval-time index task (Epic #75)
/// indexes a repo before any review runs, so reviews are normally warm; this residual cold path is
/// removed once review no longer self-indexes (a later step in the runner-differentiation plan).
fn resources_for<'a>(
    task: &ClaimedTask,
    indexer: Option<&'a Value>,
    review: Option<&'a Value>,
    shared: Option<&'a Value>,
) -> Option<&'a Value> {
    if task.command_text == "index" {
        indexer.or(shared)
    } else {
        review.or(shared)
    }
}

/// Pick the container image for a task's kind (runner-differentiation, RFC-0001). Since the structural
/// graph is now built in-process by `lci-codegraph` (ADR-0086, retiring the Python Graphify CLI), the
/// **index** and **review** images share one lean runtime — the split image (index bundled Python +
/// Graphify) is gone. The per-kind overrides remain so operators can still pin the two Job kinds to
/// different tags/digests; each falls back to the shared `shared` image when its override is unset, so
/// a chart that sets only `runner_image` keeps single-image behaviour.
///
/// CAVEAT: a *cold-repo* review still self-indexes (ADR-0050 only makes *warm* reviews reuse the
/// snapshot), including the structural-graph step — now in-process, so no separate image is needed for
/// it. That step is best-effort/non-fatal (`index_graph` in services/agent-runner/src/indexer/graph.rs):
/// it logs and skips on failure. So a first cold review still builds the semantic (pgvector) index and
/// the graph inline. The proper fix — review never self-indexes — is a separate runner-differentiation slice.
fn image_for<'a>(
    task: &ClaimedTask,
    indexer: Option<&'a str>,
    review: Option<&'a str>,
    shared: &'a str,
) -> &'a str {
    if task.command_text == "index" {
        indexer.unwrap_or(shared)
    } else {
        review.unwrap_or(shared)
    }
}

/// The per-task agent Job manifest (mirrors docs/kubernetes-deployment.md): `restartPolicy: Never`,
/// a TTL for cleanup, an active deadline to bound runtime, a least-privilege service account, and the
/// task id passed through so the runner can fetch its work and report back.
fn job_manifest(name: &str, cfg: JobConfig, task: &ClaimedTask) -> Value {
    let JobConfig {
        image,
        service_account,
        control_plane_url,
        runner_token,
        ca_secret,
        active_deadline_seconds,
        review_system_prompt,
        agent_config_map,
        resources,
        owner_reference,
    } = cfg;
    // ADR-0088: the `open` mode is the highest-risk surface — it executes untrusted repo + generated
    // code — so its pod is hardened AND credential-light. It carries **only** the LLM key + runner
    // token (retrieval is brokered, not a direct DB reach), never the embeddings/index credentials the
    // read-only index/review paths use, and never a forge or DB credential (those live on the egress
    // plane / control plane). This split is enforced below.
    let is_open = task.command_text == "open";
    // Pass the claimed task's context so the runner knows what to act on (target + SHAs) and how to
    // report back: the task id, plus where to call (CONTROL_PLANE_URL) and the shared bearer it
    // presents (AGENT_RUNNER_TOKEN). The runner fetches full context from the internal API rather
    // than trusting these env values for anything security-sensitive.
    let mut env = vec![
        json!({ "name": "TASK_ID", "value": task.id.to_string() }),
        json!({ "name": "REPOSITORY_ID", "value": task.repository_id.to_string() }),
        json!({ "name": "INSTALLATION_ID", "value": task.installation_id.to_string() }),
        json!({ "name": "COMMAND", "value": task.command_text }),
        json!({ "name": "TARGET_TYPE", "value": task.target_type }),
        json!({ "name": "TARGET_ID", "value": task.target_id.to_string() }),
        json!({ "name": "ATTEMPT", "value": task.attempts.to_string() }),
        json!({ "name": "CONTROL_PLANE_URL", "value": control_plane_url }),
        json!({ "name": "AGENT_RUNNER_TOKEN", "value": runner_token }),
        // Review LLM config (ADR-0021). `optional: true`: absent these keys, the secretKeyRef
        // resolves to unset (not a pod start failure), so `LLM_MODEL` is unset and the runner skips
        // the review step (indexing-only). The operator enables review by populating these keys.
        // `open` keeps this (its loop needs the LLM) but NOT the embeddings creds below.
        json!({ "name": "LLM_BASE_URL",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "llm-base-url", "optional": true } } }),
        json!({ "name": "LLM_API_KEY",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "llm-api-key", "optional": true } } }),
        json!({ "name": "LLM_MODEL",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "llm-model", "optional": true } } }),
    ];
    if !is_open {
        // Embeddings config (ADR-0018): required for index/review — a misconfigured Job fails loud
        // rather than silently embedding with the wrong model. `open` never gets these: it does not
        // embed, and retrieval (if any) is brokered through the mediated API, not a direct index reach.
        env.push(json!({ "name": "EMBEDDINGS_BASE_URL",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "embeddings-base-url" } } }));
        env.push(json!({ "name": "EMBEDDINGS_API_KEY",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "embeddings-api-key" } } }));
        env.push(json!({ "name": "EMBEDDINGS_MODEL",
            "valueFrom": { "secretKeyRef": { "name": "lightbridge-agent-secrets", "key": "embeddings-model" } } }));
    }
    if let Some(base_sha) = &task.base_sha {
        env.push(json!({ "name": "BASE_SHA", "value": base_sha }));
    }
    if let Some(head_sha) = &task.head_sha {
        env.push(json!({ "name": "HEAD_SHA", "value": head_sha }));
    }
    // Operator-supplied reviewer system prompt (ADR-0021), passed through to the runner. Absent it,
    // the runner uses its built-in default guidance.
    if let Some(prompt) = review_system_prompt {
        env.push(json!({ "name": "REVIEW_SYSTEM_PROMPT", "value": prompt }));
    }

    // Operator-tunable indexer knobs (INDEX_*): forwarded from the control-plane's own environment so
    // they can be set once on the control-plane deployment (Helm) instead of baked into the runner
    // image. Unset → the runner's built-in defaults apply (embed batch 32, chunk-line ceiling 150,
    // window 100/50). Chief use: shrinking INDEX_EMBED_BATCH_SIZE when a gateway caps the batched
    // embeddings response body.
    if !is_open {
        for key in [
            "INDEX_EMBED_BATCH_SIZE",
            "INDEX_MAX_CHUNK_LINES",
            "INDEX_WINDOW_SIZE",
            "INDEX_WINDOW_STEP",
        ] {
            if let Ok(value) = std::env::var(key) {
                env.push(json!({ "name": key, "value": value }));
            }
        }
    }

    // Internal CA: when configured, mount the gateway's CA so the Job can verify the HTTPS eaig
    // endpoint (the gateway's cert is from a private issuer, ClusterIssuer/self-signed-ca). The
    // runner's reqwest clients (embeddings + the review LLM) add it via `add_root_certificate`
    // (ADR-0018); `EMBEDDINGS_CA_CERT` is the path they read.
    let mut volumes: Vec<Value> = Vec::new();
    let mut volume_mounts: Vec<Value> = Vec::new();
    if let Some(secret) = ca_secret {
        env.push(json!({ "name": "EMBEDDINGS_CA_CERT", "value": "/etc/internal-ca/ca.crt" }));
        volumes.push(json!({ "name": "internal-ca", "secret": { "secretName": secret } }));
        volume_mounts.push(
            json!({ "name": "internal-ca", "mountPath": "/etc/internal-ca", "readOnly": true }),
        );
    }
    // The runner's file config + prompt templates (ADR-0021): mount the ConfigMap at /etc/lightbridge
    // and point `AGENT_CONFIG` at it so the runner reads `agent.json` instead of legacy env vars.
    if let Some(config_map) = agent_config_map {
        env.push(json!({ "name": "AGENT_CONFIG", "value": "/etc/lightbridge/agent.json" }));
        volumes.push(json!({ "name": "agent-config", "configMap": { "name": config_map } }));
        volume_mounts.push(
            json!({ "name": "agent-config", "mountPath": "/etc/lightbridge", "readOnly": true }),
        );
    }
    let volumes = Value::Array(volumes);
    let volume_mounts = Value::Array(volume_mounts);

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
            // Runtime cap: clone + tree-sitter chunk + embed + lci-codegraph can legitimately run for
            // tens of minutes on a large repo (observed >15min in prod purely on indexing). Operator-
            // tunable via AGENT_JOB_DEADLINE_SECONDS (default 3600 = 1h) rather than killing healthy
            // long indexers. `ttlSecondsAfterFinished` below is a separate post-completion cleanup.
            "activeDeadlineSeconds": active_deadline_seconds,
            "ttlSecondsAfterFinished": 900,
            "template": {
                "metadata": {
                    "labels": {
                        "app.kubernetes.io/name": "lightbridge",
                        "app.kubernetes.io/component": "agent",
                    }
                },
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
    // Operator-configurable container resources (requests/limits), set verbatim when present.
    if let Some(resources) = resources {
        manifest["spec"]["template"]["spec"]["containers"][0]["resources"] = resources.clone();
    }
    // ownerReference (to the agent ServiceAccount) for k8s GC + traceability, when we have the UID.
    if let Some(owner) = owner_reference {
        manifest["metadata"]["ownerReferences"] = json!([owner]);
    }
    if is_open {
        harden_open_pod(&mut manifest);
    }
    manifest
}

/// Apply the ADR-0088 sandbox posture to an `open` Job manifest. `open` executes untrusted repo +
/// generated code, so containment is at the pod boundary: non-root, all Linux capabilities dropped,
/// seccomp `RuntimeDefault`, no privilege escalation, a **read-only root filesystem** with a single
/// writable work `emptyDir` (the checkout root, wiped with the pod), and **no projected service-account
/// token** (the pod has no reason to reach the Kubernetes API). Egress restriction is a NetworkPolicy —
/// a deploy-side control (default-deny + a three-entry allowlist: LLM gateway, package registries, git
/// remote); it cannot be expressed on the Job manifest, so it is enforced alongside the chart, not here.
/// The credential-light env (LLM key + runner token only — no embeddings/forge/DB secret) is set in
/// [`job_manifest`]; this only adds the containment fields.
fn harden_open_pod(manifest: &mut Value) {
    let pod = &mut manifest["spec"]["template"]["spec"];
    // The pod never talks to the Kubernetes API — do not project its service-account token.
    pod["automountServiceAccountToken"] = json!(false);
    pod["securityContext"] = json!({
        "runAsNonRoot": true,
        "seccompProfile": { "type": "RuntimeDefault" },
    });
    // The ONLY writable surface: a per-task work emptyDir at the checkout root (`WORKDIR` default).
    // Everything the agent edits, the build outputs, and the test artifacts live here and die with
    // the pod. The root filesystem itself is read-only (below).
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample_task() -> ClaimedTask {
        ClaimedTask {
            id: Uuid::nil(),
            repository_id: 1,
            installation_id: 42,
            target_type: "pull_request".to_string(),
            target_id: 7,
            command_text: "review".to_string(),
            base_sha: None,
            head_sha: Some("deadbeef".to_string()),
            attempts: 1,
        }
    }

    #[test]
    fn job_name_is_derived_from_the_task_id() {
        assert_eq!(
            job_name(&sample_task()),
            "lightbridge-agent-00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn resources_for_picks_by_kind_with_shared_fallback() {
        let indexer = json!({ "limits": { "memory": "4Gi" } });
        let review = json!({ "limits": { "memory": "2Gi" } });
        let shared = json!({ "limits": { "memory": "3Gi" } });

        let mut index_task = sample_task();
        index_task.command_text = "index".to_string();
        let review_task = sample_task(); // command_text == "review"

        // Per-kind blocks win for their own kind.
        assert_eq!(
            resources_for(&index_task, Some(&indexer), Some(&review), Some(&shared)),
            Some(&indexer)
        );
        assert_eq!(
            resources_for(&review_task, Some(&indexer), Some(&review), Some(&shared)),
            Some(&review)
        );
        // A missing per-kind block falls back to the shared one (today's behaviour for a chart that
        // only sets `resources`).
        assert_eq!(
            resources_for(&index_task, None, None, Some(&shared)),
            Some(&shared)
        );
        assert_eq!(
            resources_for(&review_task, None, None, Some(&shared)),
            Some(&shared)
        );
        // Nothing set → None (cluster defaults / LimitRange apply).
        assert_eq!(resources_for(&index_task, None, None, None), None);
        // Any non-"index" command (e.g. an @mention re-review) is treated as a review Job.
        let mut mention = sample_task();
        mention.command_text = "@lightbridge please re-review".to_string();
        assert_eq!(
            resources_for(&mention, Some(&indexer), Some(&review), Some(&shared)),
            Some(&review)
        );
    }

    #[test]
    fn image_for_picks_by_kind_with_shared_fallback() {
        let indexer = "ghcr.io/vymalo/lightbridge-agent-runner:tag";
        let review = "ghcr.io/vymalo/lightbridge-agent-review:tag";
        let shared = "ghcr.io/vymalo/lightbridge-agent-runner:latest";

        let mut index_task = sample_task();
        index_task.command_text = "index".to_string();
        let review_task = sample_task(); // command_text == "review"

        // Per-kind images win for their own kind.
        assert_eq!(
            image_for(&index_task, Some(indexer), Some(review), shared),
            indexer
        );
        assert_eq!(
            image_for(&review_task, Some(indexer), Some(review), shared),
            review
        );
        // A missing per-kind image falls back to the shared one (today's single-image behaviour for
        // a chart that only sets `runner_image`).
        assert_eq!(image_for(&index_task, None, None, shared), shared);
        assert_eq!(image_for(&review_task, None, None, shared), shared);
        // Any non-"index" command (e.g. an @mention re-review) is treated as a review Job.
        let mut mention = sample_task();
        mention.command_text = "@lightbridge please re-review".to_string();
        assert_eq!(
            image_for(&mention, Some(indexer), Some(review), shared),
            review
        );
    }

    #[test]
    fn manifest_is_a_valid_job_with_task_wiring() {
        let task = sample_task();
        let value = job_manifest(
            &job_name(&task),
            JobConfig {
                image: "img:tag",
                service_account: "sa",
                control_plane_url: "http://cp:8080",
                runner_token: "runner-secret",
                ca_secret: Some("lightbridge-agent-ca"),
                active_deadline_seconds: 1800,
                review_system_prompt: Some("Be a terse reviewer."),
                agent_config_map: Some("lightbridge-agent-config"),
                resources: Some(&json!({
                    "requests": { "cpu": "500m", "memory": "1Gi" },
                    "limits": { "memory": "2Gi" }
                })),
                owner_reference: Some(&json!({
                    "apiVersion": "v1", "kind": "ServiceAccount", "name": "lightbridge-agent",
                    "uid": "sa-uid-123", "controller": false, "blockOwnerDeletion": false
                })),
            },
            &task,
        );

        // Deserializes into the typed k8s Job (catches structural mistakes without a cluster).
        let job: Job = serde_json::from_value(value.clone()).expect("valid Job manifest");
        let spec = job.spec.expect("job spec");
        let pod = spec.template.spec.expect("pod spec");
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        // The Job carries an ownerReference to the agent ServiceAccount (k8s GC + traceability).
        let owner = &job
            .metadata
            .owner_references
            .as_ref()
            .expect("ownerReferences")[0];
        assert_eq!(owner.kind, "ServiceAccount");
        assert_eq!(owner.name, "lightbridge-agent");
        assert_eq!(owner.uid, "sa-uid-123");
        // The runtime cap is the value the launcher passes (from AGENT_JOB_DEADLINE_SECONDS).
        assert_eq!(spec.active_deadline_seconds, Some(1800));
        assert_eq!(spec.ttl_seconds_after_finished, Some(900));
        assert_eq!(pod.service_account_name.as_deref(), Some("sa"));

        let container = &pod.containers[0];
        assert_eq!(container.image.as_deref(), Some("img:tag"));
        // Operator-configured resources are set on the runner container.
        let resources = container.resources.as_ref().expect("resources set");
        assert_eq!(
            resources
                .requests
                .as_ref()
                .and_then(|r| r.get("cpu"))
                .map(|q| q.0.as_str()),
            Some("500m")
        );
        let env = container.env.as_ref().expect("env");
        let value_of = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            value_of("TASK_ID").as_deref(),
            Some(task.id.to_string().as_str())
        );
        assert_eq!(value_of("COMMAND").as_deref(), Some("review"));
        assert_eq!(value_of("HEAD_SHA").as_deref(), Some("deadbeef"));
        // base_sha is None on the sample, so BASE_SHA is omitted entirely.
        assert!(value_of("BASE_SHA").is_none());
        // The runner's callback wiring travels with the Job.
        assert_eq!(
            value_of("CONTROL_PLANE_URL").as_deref(),
            Some("http://cp:8080")
        );
        assert_eq!(
            value_of("AGENT_RUNNER_TOKEN").as_deref(),
            Some("runner-secret")
        );

        // Embeddings (required) + LLM (optional) config are injected from the agent secret.
        let env_ref = |name: &str| env.iter().find(|e| e.name == name);
        let secret_key = |name: &str| -> Option<(String, Option<bool>)> {
            let e = env_ref(name)?;
            let sel = e.value_from.as_ref()?.secret_key_ref.as_ref()?;
            Some((sel.key.clone(), sel.optional))
        };
        assert_eq!(
            secret_key("EMBEDDINGS_MODEL"),
            Some(("embeddings-model".to_string(), None)),
            "embeddings is required (not optional)"
        );
        assert_eq!(
            secret_key("LLM_MODEL"),
            Some(("llm-model".to_string(), Some(true))),
            "review LLM is optional so the Job starts without it"
        );

        // The internal CA is mounted and the runner's reqwest clients are pointed at it.
        assert_eq!(
            value_of("EMBEDDINGS_CA_CERT").as_deref(),
            Some("/etc/internal-ca/ca.crt")
        );
        let mount = container
            .volume_mounts
            .as_ref()
            .and_then(|m| m.iter().find(|m| m.name == "internal-ca"))
            .expect("internal-ca volumeMount");
        assert_eq!(mount.mount_path, "/etc/internal-ca");
        let vol = pod
            .volumes
            .as_ref()
            .and_then(|v| v.iter().find(|v| v.name == "internal-ca"))
            .expect("internal-ca volume");
        assert_eq!(
            vol.secret.as_ref().and_then(|s| s.secret_name.as_deref()),
            Some("lightbridge-agent-ca")
        );
        // The operator's reviewer prompt override is passed through to the runner.
        assert_eq!(
            value_of("REVIEW_SYSTEM_PROMPT").as_deref(),
            Some("Be a terse reviewer.")
        );

        // The agent ConfigMap is mounted at /etc/lightbridge and AGENT_CONFIG points at it.
        assert_eq!(
            value_of("AGENT_CONFIG").as_deref(),
            Some("/etc/lightbridge/agent.json")
        );
        let cfg_mount = container
            .volume_mounts
            .as_ref()
            .and_then(|m| m.iter().find(|m| m.name == "agent-config"))
            .expect("agent-config volumeMount");
        assert_eq!(cfg_mount.mount_path, "/etc/lightbridge");
        let cfg_vol = pod
            .volumes
            .as_ref()
            .and_then(|v| v.iter().find(|v| v.name == "agent-config"))
            .expect("agent-config volume");
        assert_eq!(
            cfg_vol.config_map.as_ref().map(|c| c.name.as_str()),
            Some("lightbridge-agent-config")
        );
    }

    // ADR-0088 merge bar: the `open` Job manifest satisfies every sandbox hard requirement (non-root,
    // caps dropped, seccomp, read-only root FS + single writable emptyDir, no projected SA token), and
    // is credential-light — it carries the LLM key + runner token and NO forge/DB/embeddings secret.
    #[test]
    fn open_manifest_is_hardened_and_credential_light() {
        let mut task = sample_task();
        task.command_text = "open".to_string();
        let value = job_manifest(
            &job_name(&task),
            JobConfig {
                image: "img:tag",
                service_account: "sa",
                control_plane_url: "http://cp:8080",
                runner_token: "runner-secret",
                ca_secret: None,
                active_deadline_seconds: 1800,
                review_system_prompt: None,
                agent_config_map: None,
                resources: None,
                owner_reference: None,
            },
            &task,
        );
        // Structurally valid Job.
        let job: Job = serde_json::from_value(value.clone()).expect("valid open Job manifest");

        // Pod-level hardening: non-root + seccomp + no projected SA token.
        let pod = &value["spec"]["template"]["spec"];
        assert_eq!(
            pod["automountServiceAccountToken"],
            serde_json::json!(false)
        );
        assert_eq!(
            pod["securityContext"]["runAsNonRoot"],
            serde_json::json!(true)
        );
        assert_eq!(
            pod["securityContext"]["seccompProfile"]["type"],
            serde_json::json!("RuntimeDefault")
        );

        // Container-level hardening: read-only root FS, no priv-esc, all caps dropped, seccomp.
        let container = &value["spec"]["template"]["spec"]["containers"][0];
        let sc = &container["securityContext"];
        assert_eq!(sc["readOnlyRootFilesystem"], serde_json::json!(true));
        assert_eq!(sc["allowPrivilegeEscalation"], serde_json::json!(false));
        assert_eq!(sc["runAsNonRoot"], serde_json::json!(true));
        assert_eq!(sc["capabilities"]["drop"], serde_json::json!(["ALL"]));
        assert_eq!(
            sc["seccompProfile"]["type"],
            serde_json::json!("RuntimeDefault")
        );

        // The ONLY writable surface is the work emptyDir at the checkout root.
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let work_vol = pod_spec
            .volumes
            .as_ref()
            .and_then(|v| v.iter().find(|v| v.name == "work"))
            .expect("work emptyDir volume");
        assert!(work_vol.empty_dir.is_some(), "work must be an emptyDir");
        let mount = pod_spec.containers[0]
            .volume_mounts
            .as_ref()
            .and_then(|m| m.iter().find(|m| m.name == "work"))
            .expect("work volumeMount");
        assert_eq!(mount.mount_path, "/workspace");

        // Credential-light: LLM key + runner token present; NO embeddings/index/forge/DB secret.
        let env = pod_spec.containers[0].env.as_ref().expect("env");
        assert!(env.iter().any(|e| e.name == "AGENT_RUNNER_TOKEN"));
        assert!(
            env.iter().any(|e| e.name == "LLM_MODEL"),
            "open keeps the LLM key"
        );
        assert!(
            !env.iter().any(|e| e.name.starts_with("EMBEDDINGS")),
            "open must NOT carry embeddings credentials"
        );
        assert!(
            !env.iter().any(|e| e.name.starts_with("INDEX_")),
            "open must NOT carry index knobs"
        );
        // Every secret this pod references is an `llm-*` key of the agent secret — no forge/DB secret.
        for e in env {
            if let Some(sel) = e
                .value_from
                .as_ref()
                .and_then(|v| v.secret_key_ref.as_ref())
            {
                assert!(
                    sel.key.starts_with("llm-"),
                    "open pod references a non-LLM secret {:?} (forge/DB/embeddings creds are forbidden)",
                    sel.key
                );
            }
        }
    }

    // The hardening is `open`-only: a review Job keeps the relaxed posture (writable root FS, its
    // embeddings credentials, a projected SA token) — the hardening must not regress the read paths.
    #[test]
    fn review_manifest_is_not_hardened() {
        let task = sample_task(); // command_text == "review"
        let value = job_manifest(
            &job_name(&task),
            JobConfig {
                image: "img:tag",
                service_account: "sa",
                control_plane_url: "http://cp:8080",
                runner_token: "runner-secret",
                ca_secret: None,
                active_deadline_seconds: 1800,
                review_system_prompt: None,
                agent_config_map: None,
                resources: None,
                owner_reference: None,
            },
            &task,
        );
        assert!(value["spec"]["template"]["spec"]["securityContext"].is_null());
        assert!(value["spec"]["template"]["spec"]["automountServiceAccountToken"].is_null());
        assert!(value["spec"]["template"]["spec"]["containers"][0]["securityContext"].is_null());
        let job: Job = serde_json::from_value(value).unwrap();
        let env = job.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .clone()
            .unwrap();
        assert!(
            env.iter().any(|e| e.name == "EMBEDDINGS_MODEL"),
            "review keeps its embeddings credentials"
        );
    }

    #[test]
    fn review_prompt_passthrough_is_omitted_when_unset() {
        let task = sample_task();
        let value = job_manifest(
            &job_name(&task),
            JobConfig {
                image: "img:tag",
                service_account: "sa",
                control_plane_url: "http://cp:8080",
                runner_token: "runner-secret",
                ca_secret: None,
                active_deadline_seconds: 3600,
                review_system_prompt: None,
                agent_config_map: None,
                resources: None,
                owner_reference: None,
            },
            &task,
        );
        let job: Job = serde_json::from_value(value).expect("valid Job manifest");
        let env = job.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .clone()
            .expect("env");
        assert!(
            !env.iter().any(|e| e.name == "REVIEW_SYSTEM_PROMPT"),
            "no override → the env var is absent (runner uses its default)"
        );
    }

    #[test]
    fn deadline_parsing_falls_back_on_bad_values() {
        assert_eq!(parse_deadline_secs(Some("7200")), 7200);
        assert_eq!(parse_deadline_secs(Some("  600 ")), 600, "trims whitespace");
        assert_eq!(parse_deadline_secs(None), DEFAULT_JOB_DEADLINE_SECONDS);
        assert_eq!(parse_deadline_secs(Some("")), DEFAULT_JOB_DEADLINE_SECONDS);
        assert_eq!(
            parse_deadline_secs(Some("abc")),
            DEFAULT_JOB_DEADLINE_SECONDS
        );
        assert_eq!(
            parse_deadline_secs(Some("0")),
            DEFAULT_JOB_DEADLINE_SECONDS,
            "zero would disable the cap → fall back"
        );
        assert_eq!(
            parse_deadline_secs(Some("-5")),
            DEFAULT_JOB_DEADLINE_SECONDS
        );
    }
}
