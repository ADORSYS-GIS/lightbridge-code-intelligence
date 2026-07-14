//! Task lifecycle: load context, poll status, report status transitions.

use serde::Serialize;
use uuid::Uuid;

use super::{ControlPlaneClient, TaskContext};

#[derive(Debug, Serialize)]
struct StatusUpdate<'a> {
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl ControlPlaneClient {
    /// `GET /internal/tasks/{id}` — load this task's context (with a freshly-minted token).
    pub async fn get_context(&self, task_id: Uuid) -> anyhow::Result<TaskContext> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}", self.base_url);
        let context = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("requesting task context")?
            .error_for_status()
            .context("control plane rejected the task-context request")?
            .json::<TaskContext>()
            .await
            .context("parsing task context")?;
        Ok(context)
    }

    /// `GET /internal/tasks/{id}/status` — the task's current status, for the self-cancel poll.
    pub async fn task_status(&self, task_id: Uuid) -> anyhow::Result<String> {
        use anyhow::Context;
        #[derive(serde::Deserialize)]
        struct StatusResponse {
            status: String,
        }
        let url = format!("{}/internal/tasks/{task_id}/status", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("requesting task status")?
            .error_for_status()
            .context("control plane rejected the task-status request")?
            .json::<StatusResponse>()
            .await
            .context("parsing task status")?;
        Ok(resp.status)
    }

    /// `POST /internal/tasks/{id}/status` — report a status transition (best-effort `detail`).
    pub async fn report_status(
        &self,
        task_id: Uuid,
        status: &str,
        detail: Option<&str>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/status", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&StatusUpdate { status, detail })
            .send()
            .await
            .context("reporting status")?
            .error_for_status()
            .context("control plane rejected the status report")?;
        Ok(())
    }
}
