//! ADR-0088 open-mode mediated PR-open egress: the only credentialed side effect the `open` agent
//! triggers, and it happens off the pod (the runner token authenticates it, no forge token ever
//! reaches the sandbox).

use uuid::Uuid;

use super::ControlPlaneClient;

impl ControlPlaneClient {
    /// `POST /internal/tasks/{id}/propose-pr` — the open-mode mediated PR-open egress (ADR-0088).
    ///
    /// The open agent holds **no forge credential**: it commits to a local branch in its sandbox, then
    /// hands the branch (captured as a `git format-patch` series) + the PR metadata here. The control
    /// plane content-hashes + offloads the patch and enqueues a PR-open intent keyed by
    /// `(task_id, run_epoch)` (the agent never knows `run_epoch` — trust boundary); the egress plane
    /// (which holds the forge creds) pushes the branch and opens the PR. This call is idempotent on that
    /// key, so a replay opens exactly one PR. It is the only credentialed side effect the open agent
    /// triggers, and it happens *off* the pod — the runner token authenticates it, no forge token ever
    /// reaches the sandbox.
    pub async fn propose_pr(
        &self,
        task_id: Uuid,
        title: &str,
        body: &str,
        base: Option<&str>,
        branch: &str,
        patch: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/propose-pr", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "title": title, "body": body, "base": base, "branch": branch, "patch": patch,
            }))
            .send()
            .await
            .context("proposing pull request")?
            .error_for_status()
            .context("control plane rejected the PR proposal")?;
        Ok(())
    }
}
