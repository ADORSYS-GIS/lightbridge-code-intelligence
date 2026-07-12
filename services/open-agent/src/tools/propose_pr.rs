//! `propose_pr` — the terminal mediated egress tool (the crux of ADR-0088).
//!
//! The open agent holds **no forge credential** and never pushes. When it is satisfied, it has already
//! committed its change to a *local* branch in the sandbox (via `run_command` git commits). `propose_pr`
//! captures that branch as a patch (`git format-patch base..HEAD`) and hands it — with the PR metadata —
//! to the control plane through the mediated internal API ([`ControlPlaneClient::propose_pr`]). The
//! egress plane (which holds the forge creds) content-hashes + offloads the patch, then pushes the
//! branch and opens the PR against the forge. The sandbox never sees a forge token and never reaches
//! `api.github.com`.
//!
//! This is [ADR-0037] extended from comments to code: `add_review_comment` → `propose_pr`, the intent
//! just carries a branch instead of a comment body. It is the **only** credentialed side effect, and it
//! happens *off* the pod. Egress dedups on `(task_id, run_epoch)` (computed control-plane-side), so a
//! replay of this terminal step opens exactly one PR (ADR-0088 O5). It never auto-merges — it proposes.

use std::process::Stdio;
use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use tokio::process::Command;

use super::{OpenServices, parse};

pub const PROPOSE_PR: &str = "propose_pr";

#[derive(Deserialize)]
struct Args {
    title: String,
    body: String,
    /// The base ref/sha the change is proposed against (the branch the PR targets). Defaults to the
    /// repository default branch when omitted.
    #[serde(default)]
    base: Option<String>,
    /// The local branch name the agent committed to; the egress plane pushes it under this name.
    branch: String,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        PROPOSE_PR,
        "Propose a pull request from the branch you committed locally. This does NOT push or merge: it \
         hands the branch + PR metadata to the control plane, which pushes and opens the PR for a human \
         to review. Call once, after you have committed your change (git add/commit via run_command) \
         and verified it builds/tests. The PR body must include an AI Usage Declaration, the \
         source-of-truth issue reference, and your sandbox build/test verification.",
        serde_json::json!({"type":"object","properties":{
            "title":{"type":"string","description":"The pull request title."},
            "body":{"type":"string","description":"The PR body — MUST include the AI Usage Declaration, source-of-truth issue #, and Verification (sandbox build/test results)."},
            "base":{"type":"string","description":"Optional base ref/branch the PR targets (defaults to the repo default branch)."},
            "branch":{"type":"string","description":"The local branch name you committed to."}
        },"required":["title","body","branch"]}),
    )
}

struct ProposePrTool {
    spec: ToolSpec,
    services: OpenServices,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &OpenServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(
        Arc::new(ProposePrTool {
            spec: spec(),
            services: services.clone(),
        }),
        caps,
    )
}

impl Tool for ProposePrTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Terminal
    }
    fn replay(&self) -> ReplaySafety {
        // The endpoint is idempotent on `(task_id, run_epoch)` (dedup enforced at egress); re-sending
        // the same keyed intent returns the existing PR rather than opening a second one (ADR-0088 O5).
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<Args>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => return ToolOutcome::Continue(error),
            };
            let root = match cx.workspace.root().await {
                Ok(root) => root.to_path_buf(),
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not materialize the sandbox workdir: {error}"
                    ));
                }
            };
            let base = args.base.as_deref().unwrap_or("HEAD~1");
            let patch = match capture_patch(&root, base).await {
                Ok(patch) if patch.trim().is_empty() => {
                    return ToolOutcome::Continue(
                        "error: the branch has no commits over the base — commit your change with \
                         run_command (git add/commit) before calling propose_pr."
                            .into(),
                    );
                }
                Ok(patch) => patch,
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not capture the branch patch: {error}. Ensure you committed your \
                         change (git add/commit) and that `base` is a valid ref."
                    ));
                }
            };
            match self
                .services
                .client
                .propose_pr(
                    cx.task_id,
                    &args.title,
                    &args.body,
                    args.base.as_deref(),
                    &args.branch,
                    &patch,
                )
                .await
            {
                Ok(()) => ToolOutcome::Finish,
                Err(error) => ToolOutcome::Continue(format!(
                    "error: the control plane rejected the PR proposal: {error:#}. You may call \
                     propose_pr again."
                )),
            }
        })
    }
}

/// Capture the branch as a patch series (`git format-patch <base>..HEAD --stdout`). This is a purely
/// local git operation — it needs no forge credential. The bytes travel to the control plane, which
/// content-hashes + offloads them before enqueuing the PR-open intent (the offload rule, ADR-0082/0088).
async fn capture_patch(root: &std::path::Path, base: &str) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("format-patch")
        .arg(format!("{base}..HEAD"))
        .arg("--stdout")
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "git format-patch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
