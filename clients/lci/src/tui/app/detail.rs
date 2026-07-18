//! The Run Detail "page" sub-model: one task's metadata + its review. Opened from a Runs row and torn
//! down on Esc/back. Run observability (the model's turns/reasoning) is Loki-only now (epic #459) — the
//! DB transcript and its live-tailing view were removed — so this page is a compact status + review
//! summary. It refreshes on the periodic view poll while the run is still live.

use crate::api::{ReviewRow, TaskRow};

pub struct DetailState {
    /// The task this page is about.
    pub task_id: uuid::Uuid,
    /// Full metadata (seeded from the Runs row, refreshed by the poll).
    pub task: TaskRow,
    /// The review, once fetched. `None` = not fetched yet or none recorded (see `review_loaded`).
    pub review: Option<ReviewRow>,
    /// True once the review fetch has resolved (so we can distinguish "loading" from "none recorded").
    pub review_loaded: bool,
    /// True while the task is in a non-terminal status (drives the `● live` badge; the periodic view
    /// refresh keeps status + review current until it goes terminal).
    pub live: bool,
    /// Set when the caller lacks `review:read`: we skip the fetch and show an inline notice instead.
    pub permission_denied: bool,
}

impl DetailState {
    /// Open a detail page for `task`. `can_read` gates the review fetch on `review:read`.
    pub fn new(task: TaskRow, can_read: bool) -> Self {
        let live = task.is_active();
        Self {
            task_id: task.id,
            live,
            task,
            review: None,
            review_loaded: false,
            permission_denied: !can_read,
        }
    }

    /// Reflect a refreshed task row (status may have advanced). Flips `live` off on a terminal status.
    pub fn set_task(&mut self, task: TaskRow) {
        self.live = task.is_active();
        self.task = task;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_row(status: &str) -> TaskRow {
        TaskRow {
            id: uuid::Uuid::from_u128(42),
            repository_id: 1,
            target_type: "pull_request".into(),
            target_id: 128,
            command_text: "review".into(),
            kind: "review".into(),
            status: status.into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            completed_at: None,
            repo_owner: Some("vymalo".into()),
            repo_name: Some("lci".into()),
            job_name: Some("review-abc".into()),
            error_detail: None,
            base_sha: Some("a1b2c3d4e5f6".into()),
            head_sha: Some("e4f5a6b7c8d9".into()),
        }
    }

    #[test]
    fn set_task_flips_live_off_on_terminal_status() {
        let mut d = DetailState::new(task_row("running"), true);
        assert!(d.live, "a running task reads as live");
        d.set_task(task_row("succeeded"));
        assert!(!d.live, "a terminal status is no longer live");
    }
}
