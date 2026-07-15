//! [`RunnerConfig`] — the minimal env-sourced wiring the runner needs to find and authenticate to the
//! control plane before it has fetched anything else (the task context comes from the control plane
//! itself, not env).

use uuid::Uuid;

use super::env::{parse_required, require};

/// Everything the runner needs to start: which task it is, and how to reach the control plane.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub task_id: Uuid,
    pub control_plane_url: String,
    pub runner_token: String,
    /// Directory the repository is cloned into. Defaults to `/workspace` (an emptyDir in the Job).
    pub workdir: String,
}

impl RunnerConfig {
    /// Parse from process env. Errors name the missing/invalid variable so a misconfigured Job is
    /// diagnosable from the runner's first log line.
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            task_id: parse_required("TASK_ID")?,
            control_plane_url: require("CONTROL_PLANE_URL")?,
            runner_token: require("AGENT_RUNNER_TOKEN")?,
            workdir: std::env::var("WORKDIR").unwrap_or_else(|_| "/workspace".to_string()),
        })
    }
}
