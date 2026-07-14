//! ADR-0087 durable-step journal: the agent journals each loop step through the control plane
//! (rather than holding a DB credential itself) so a resume can replay completed steps instead of
//! re-running them. See [`crate::checkpoint`] for the `StepRuntime` that drives this.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ControlPlaneClient;

/// A journaled durable-step result read back from the control plane (ADR-0087): the stored value and
/// its content hash (so replay can verify the rehydrated bytes).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredStep {
    pub result: serde_json::Value,
    pub content_hash: String,
}

impl ControlPlaneClient {
    /// `POST /internal/tasks/{id}/steps/upsert` — journal one agent-loop step result (ADR-0087).
    /// The agent supplies `(step_name, result, content_hash)`; the control plane resolves `run_epoch`
    /// from the task row (the agent never knows it — trust boundary). Idempotent on the key.
    pub async fn upsert_step(
        &self,
        task_id: Uuid,
        step_name: &str,
        result: &serde_json::Value,
        content_hash: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        // Serialize `result` by reference rather than through `json!`, which would clone the
        // (possibly large) step result into an intermediate `Value` before writing the body.
        #[derive(Serialize)]
        struct UpsertRequest<'a> {
            step_name: &'a str,
            result: &'a serde_json::Value,
            content_hash: &'a str,
        }
        let url = format!("{}/internal/tasks/{task_id}/steps/upsert", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&UpsertRequest {
                step_name,
                result,
                content_hash,
            })
            .send()
            .await
            .context("journaling durable step")?
            .error_for_status()
            .context("control plane rejected the durable-step upsert")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/steps/fetch` — read a journaled step result back (ADR-0087). `None`
    /// when the step has not run yet (the replay gap where the loop continues live) — a `404` from the
    /// control plane maps to `Ok(None)`, distinct from a transport/other error which is `Err`.
    pub async fn fetch_step(
        &self,
        task_id: Uuid,
        step_name: &str,
    ) -> anyhow::Result<Option<StoredStep>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/steps/fetch", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "step_name": step_name }))
            .send()
            .await
            .context("fetching durable step")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let stored = response
            .error_for_status()
            .context("control plane rejected the durable-step fetch")?
            .json::<StoredStep>()
            .await
            .context("parsing durable step")?;
        Ok(Some(stored))
    }
}

#[cfg(test)]
// ── ADR-0087 durable-step fetch: the wire contract of `fetch_step` ───────────────────────────
// The production journal-read path (`ControlPlaneStepStore::fetch` → `fetch_step`) hinges on one
// undertested mapping: a `404` from the control plane is the replay GAP (the step has not run yet →
// continue live), NOT an error. These tests pin that contract with a mock control plane.
mod fetch_step_wire_contract {
    use uuid::Uuid;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::ControlPlaneClient;

    /// A `404` from the fetch endpoint maps to `Ok(None)` — the replay gap where the loop
    /// continues live — and is NOT surfaced as an `Err` (which would abort the run).
    #[tokio::test]
    async fn fetch_step_maps_404_to_ok_none() {
        let server = MockServer::start().await;
        let task_id = Uuid::new_v4();
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{task_id}/steps/fetch")))
            .and(bearer_token("tok"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = ControlPlaneClient::new(server.uri(), "tok");
        let result = client
            .fetch_step(task_id, "llm_turn:0")
            .await
            .expect("a 404 is Ok(None), not an error");
        assert!(
            result.is_none(),
            "an un-journaled step is the replay gap: Ok(None), continue live"
        );
    }

    /// A `200` with a `StoredStep` body maps to `Ok(Some(..))` and rehydrates the journaled value
    /// and its content hash — the served-from-storage replay case.
    #[tokio::test]
    async fn fetch_step_maps_200_to_ok_some_stored_step() {
        let server = MockServer::start().await;
        let task_id = Uuid::new_v4();
        let stored = serde_json::json!({
            "result": { "content": "hi", "tool_calls": [] },
            "content_hash": "sha256:cafe",
        });
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{task_id}/steps/fetch")))
            .and(bearer_token("tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stored))
            .mount(&server)
            .await;

        let client = ControlPlaneClient::new(server.uri(), "tok");
        let step = client
            .fetch_step(task_id, "llm_turn:0")
            .await
            .expect("a 200 body parses")
            .expect("a present step is Some");
        assert_eq!(step.content_hash, "sha256:cafe");
        assert_eq!(
            step.result,
            serde_json::json!({ "content": "hi", "tool_calls": [] })
        );
    }

    /// A non-404 error status (e.g. `500`) is a real transport error → `Err`, distinct from the
    /// `404` gap. The `ControlPlaneStepStore` downgrades this to "run live", but the client layer
    /// must not silently swallow it as a miss.
    #[tokio::test]
    async fn fetch_step_surfaces_non_404_errors() {
        let server = MockServer::start().await;
        let task_id = Uuid::new_v4();
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{task_id}/steps/fetch")))
            .and(bearer_token("tok"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = ControlPlaneClient::new(server.uri(), "tok");
        let result = client.fetch_step(task_id, "llm_turn:0").await;
        assert!(
            result.is_err(),
            "a 500 is a transport error, not the replay gap — it must not map to Ok(None)"
        );
    }
}
