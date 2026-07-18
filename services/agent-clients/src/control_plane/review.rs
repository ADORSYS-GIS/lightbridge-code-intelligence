//! Review buffering + finalization (ADR-0037), plus the run transcript (ADR-0034) and telemetry
//! (ADR-0034/0062/0066). All of these accumulate control-plane-side and are mediated writes: the
//! runner never talks to the forge directly.

use serde::Serialize;
use uuid::Uuid;

use super::ControlPlaneClient;

/// One entry in the agent run transcript (ADR-0034): an assistant turn (its visible answer +
/// chain-of-thought + `tool_calls`, with the turn's token usage) or a tool result. Submitted in
/// order; the control plane assigns the sequence. Tool-result content is truncated by the runner to
/// keep the row bounded.
///
/// `content` and `reasoning` are DISTINCT and mean the same thing across both hosts (native and
/// OpenCode), per epic #459 / #461: `content` = the model's **visible message/answer**, `reasoning`
/// = its **chain-of-thought**. Historically the OpenCode host wrote reasoning into `content`; that is
/// fixed here — reasoning now has its own column.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntry {
    /// `assistant` or `tool`.
    pub role: String,
    /// The model's VISIBLE answer text, or the tool result; `None` for an assistant turn that only
    /// called tools (no prose).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The assistant turn's chain-of-thought (`reasoning_content`); `None` on tool rows and on turns
    /// with no reasoning. Distinct from `content` — never the visible answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The assistant turn's `tool_calls` array (raw JSON), when it called tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    /// For a tool-result entry, which tool produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    /// Reasoning slice of `completion_tokens` (subset, not additive) when the model reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<i64>,
    /// The model that produced this turn (recorded in the transcript, ADR-0034).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ControlPlaneClient {
    /// `POST /internal/tasks/{id}/review/inline` — buffer one inline finding (ADR-0037 mediated write
    /// action). The control plane accumulates it and flushes on [`finalize_review`](Self::finalize_review).
    ///
    /// `start_line` (ADR-0071) is the optional first line of a multi-line range — `line` is always the
    /// range's last line when both are given. Forwarded to the control plane unchanged: the runner does
    /// no range validation (ADR-0022's trust boundary puts that control-plane side), and an
    /// unrecognized/invalid range there simply falls back to a single-line comment, never a hard
    /// failure. `None` serializes as JSON `null` (an added key on the wire, but semantically identical
    /// to today's single-line behavior — same as every other `Option` field on this payload).
    #[allow(clippy::too_many_arguments)]
    pub async fn add_review_comment(
        &self,
        task_id: Uuid,
        file: &str,
        line: i32,
        start_line: Option<i32>,
        title: Option<&str>,
        priority: Option<&str>,
        category: Option<&str>,
        suggestion: Option<&str>,
        body: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/inline", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "file": file, "line": line, "start_line": start_line, "title": title,
                "priority": priority, "category": category, "suggestion": suggestion, "body": body,
            }))
            .send()
            .await
            .context("buffering inline finding")?
            .error_for_status()
            .context("control plane rejected the inline finding")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/inline/retract` — drop a buffered inline finding by
    /// `(file, line)` (Phase 2, ADR-0043): the refute pass removes a P0/P1 that didn't survive
    /// verification before it is ever posted.
    pub async fn retract_finding(
        &self,
        task_id: Uuid,
        file: &str,
        line: i32,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/inline/retract",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "file": file, "line": line }))
            .send()
            .await
            .context("retracting inline finding")?
            .error_for_status()
            .context("control plane rejected the retract")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/inline/clear` — drop ALL buffered inline findings. Used on an
    /// `abort` so an incomplete/untrusted run posts only its note, not its half-baked findings (a
    /// `placeholder` finding reached a PR this way — run 7c15f9bb).
    pub async fn clear_findings(&self, task_id: Uuid) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/inline/clear",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("clearing inline findings")?
            .error_for_status()
            .context("control plane rejected the clear")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/comment` — buffer one plain reply (ADR-0037). `call_id` is
    /// the tool-call id; threading it lets the control plane dedup a replayed reply on
    /// `(task_id, run_epoch, call_id)` (ADR-0087 C2). `None` keeps the append-only behavior.
    pub async fn add_review_reply(
        &self,
        task_id: Uuid,
        call_id: Option<&str>,
        body: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/comment", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "body": body, "call_id": call_id }))
            .send()
            .await
            .context("buffering comment")?
            .error_for_status()
            .context("control plane rejected the comment")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/summary` — set the run's summary/verdict (ADR-0037).
    pub async fn set_review_summary(&self, task_id: Uuid, body: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/summary", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("setting summary")?
            .error_for_status()
            .context("control plane rejected the summary")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/finalize` — flush the accumulated buffer as one grouped
    /// review (ADR-0037). `outcome` is how the run ended (`finished` / `exhausted` / `aborted`,
    /// ADR-0068): the control plane suppresses the post and reacts 👍 ONLY on an explicitly clean
    /// `finished` with zero findings — an aborted/exhausted run's honest note must still post, never
    /// masquerade as a clean pass.
    pub async fn finalize_review(&self, task_id: Uuid, outcome: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/finalize", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "outcome": outcome }))
            .send()
            .await
            .context("finalizing review")?
            .error_for_status()
            .context("control plane rejected the finalize")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/transcript` — submit the agent run transcript (ADR-0034) for
    /// observability. Best-effort: a failure here must not fail the task.
    pub async fn submit_transcript(
        &self,
        task_id: Uuid,
        entries: &[TranscriptEntry],
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/transcript", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "entries": entries }))
            .send()
            .await
            .context("submitting transcript")?
            .error_for_status()
            .context("control plane rejected the transcript")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/telemetry` — record run-level review telemetry at run START
    /// (ADR-0034/0062/0066): the tool set OFFERED to the model this run (`tools`, each `{name, source}`)
    /// and the resolved config, **already redacted + base64-encoded by the caller** (the api_key never
    /// leaves this process in the clear). Best-effort: a failure here must not fail the review.
    pub async fn submit_review_telemetry(
        &self,
        task_id: Uuid,
        tools: &serde_json::Value,
        config_b64: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/telemetry",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "tools": tools, "config_b64": config_b64 }))
            .send()
            .await
            .context("submitting review telemetry")?
            .error_for_status()
            .context("control plane rejected the review telemetry")?;
        Ok(())
    }
}
