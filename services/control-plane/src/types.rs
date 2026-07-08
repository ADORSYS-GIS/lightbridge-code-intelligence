//! Core domain types for the control plane. Mirrors docs/components-and-data-models.md.
//! Persistence (cratestack/SQLx) is intentionally not wired yet — see ADR-0005. These types
//! are the scaffold the generated layer will eventually align with.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoIndexStatus {
    Pending,
    Running,
    Ready,
    Failed,
    Stale,
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Received,
    WaitingForIndex,
    Queued,
    Running,
    PostingResult,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: uuid::Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    pub webhook_delivery_id: String,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub status: TaskStatus,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub repository_id: i64,
    pub branch: String,
    pub commit_sha: String,
    pub graph_version: String,
    pub vector_version: String,
    pub status: RepoIndexStatus,
}
