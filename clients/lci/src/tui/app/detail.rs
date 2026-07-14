//! The Run Detail "page" sub-model: one task's metadata + review + a live-tailing transcript. Opened
//! from a Runs row and torn down on Esc/back, so the poll only runs while the page is visible.
//!
//! Scroll semantics (the "live log tail"): the transcript renders newest-at-bottom. When new turns
//! arrive we **autoscroll** to the bottom — *unless* the operator has scrolled up, in which case we
//! hold their position and count the unseen turns (`new_since_scroll`) for a `▼ N new` indicator.
//! `G`/End jumps to the bottom and re-engages autoscroll.

use crate::api::{ReviewRow, TaskRow, TranscriptRow};

pub struct DetailState {
    /// The task this page is about.
    pub task_id: uuid::Uuid,
    /// Full metadata (seeded from the Runs row, refreshed by the poll).
    pub task: TaskRow,
    /// The review, once fetched. `None` = not fetched yet or none recorded (see `review_loaded`).
    pub review: Option<ReviewRow>,
    /// True once the review fetch has resolved (so we can distinguish "loading" from "none recorded").
    pub review_loaded: bool,
    /// The transcript turns, ordered by `seq` (dedup on append).
    pub transcript: Vec<TranscriptRow>,
    /// True once the first transcript fetch resolved (distinguishes "loading" from genuinely empty).
    pub transcript_loaded: bool,
    /// Top line of the transcript viewport (line-granular scroll offset).
    pub scroll: u16,
    /// Total wrapped content-line count measured by the last render (the renderer knows the wrap
    /// width, so it writes this back through a `Cell` during the otherwise-immutable draw). Read by
    /// [`Self::sync_after_render`] to clamp + pin.
    pub content_lines: std::cell::Cell<u16>,
    /// Height of the transcript viewport's inner area from the last render (also renderer-written).
    pub viewport_lines: std::cell::Cell<u16>,
    /// When true, new turns keep the view pinned to the bottom. Disengaged when the user scrolls up,
    /// re-engaged by `G`/End (or scrolling back to the bottom).
    pub autoscroll: bool,
    /// Count of turns that arrived while the user was scrolled up (for the `▼ N new` badge).
    pub new_since_scroll: usize,
    /// True while the task is in a non-terminal status and we're polling (`● live`).
    pub live: bool,
    /// Set when the caller lacks `review:read`: we skip the fetch and show an inline notice instead.
    pub permission_denied: bool,
    /// True while a live-tail fetch is outstanding. Guards the ~2.5s poll so a slow/hung backend can't
    /// accumulate overlapping requests (a fresh `get_task`+`get_transcript` pair would otherwise be
    /// spawned every tick regardless of whether the last one returned). Set on spawn, cleared when the
    /// `Msg::DetailTail` result lands. The per-request timeout is the hard backstop; this avoids even
    /// queuing the redundant work.
    pub tail_in_flight: bool,
}

impl DetailState {
    /// Open a detail page for `task`. `can_read` gates the review/transcript fetch on `review:read`.
    pub fn new(task: TaskRow, can_read: bool) -> Self {
        let live = task.is_active();
        Self {
            task_id: task.id,
            live,
            task,
            review: None,
            review_loaded: false,
            transcript: Vec::new(),
            transcript_loaded: false,
            scroll: 0,
            content_lines: std::cell::Cell::new(0),
            viewport_lines: std::cell::Cell::new(0),
            autoscroll: true,
            new_since_scroll: 0,
            permission_denied: !can_read,
            tail_in_flight: false,
        }
    }

    /// Whether the live-tail poll should spawn *now*: the task is still live, we're allowed to read
    /// it, and no tail fetch is already outstanding (the in-flight guard).
    pub fn should_poll(&self) -> bool {
        self.live && !self.permission_denied && !self.tail_in_flight
    }

    /// The maximum valid scroll offset given the last-known content + viewport sizes.
    pub fn max_scroll(&self) -> u16 {
        self.content_lines
            .get()
            .saturating_sub(self.viewport_lines.get())
    }

    /// Called by the renderer (during the immutable `draw`) to record the geometry it measured for
    /// this frame. Stored in `Cell`s so the mutable [`Self::sync_after_render`] can apply autoscroll
    /// on the next loop turn without the renderer needing `&mut`.
    pub fn record_geometry(&self, content_lines: u16, viewport_lines: u16) {
        self.content_lines.set(content_lines);
        self.viewport_lines.set(viewport_lines);
    }

    /// After a render, reconcile the scroll offset with the freshly-measured geometry: pin to the
    /// bottom while autoscroll is engaged, otherwise just clamp so a shrunk transcript can't scroll
    /// past the end. Returns true if the offset moved (a redraw is warranted).
    pub fn sync_after_render(&mut self) -> bool {
        let target = if self.autoscroll {
            self.max_scroll()
        } else {
            self.scroll.min(self.max_scroll())
        };
        let moved = target != self.scroll;
        self.scroll = target;
        moved
    }

    /// Test/geometry helper: set the measured geometry and immediately reconcile the offset. Mirrors
    /// what `record_geometry` + `sync_after_render` do across a render/loop boundary.
    #[cfg(test)]
    pub fn set_geometry(&mut self, content_lines: u16, viewport_lines: u16) {
        self.record_geometry(content_lines, viewport_lines);
        self.sync_after_render();
    }

    /// Merge freshly-fetched turns, deduped by `seq`. Returns how many were genuinely new. When the
    /// user is scrolled up (autoscroll off), the new count feeds the `▼ N new` badge.
    pub fn merge_transcript(&mut self, incoming: Vec<TranscriptRow>) -> usize {
        let mut added = 0;
        for row in incoming {
            if !self.transcript.iter().any(|t| t.seq == row.seq) {
                self.transcript.push(row);
                added += 1;
            }
        }
        if added > 0 {
            // Keep the canonical newest-at-bottom order regardless of fetch ordering.
            self.transcript.sort_by_key(|t| t.seq);
            if !self.autoscroll {
                self.new_since_scroll += added;
            }
        }
        self.transcript_loaded = true;
        added
    }

    /// Reflect a refreshed task row (status may have advanced). Flips `live` off on a terminal status.
    pub fn set_task(&mut self, task: TaskRow) {
        self.live = task.is_active();
        self.task = task;
    }

    /// Scroll up by `n` lines — disengages autoscroll (the operator is inspecting history).
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
        self.autoscroll = false;
    }

    /// Scroll down by `n` lines. Re-engages autoscroll (and clears the new-badge) once the bottom is
    /// reached, so scrolling back down resumes the tail.
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll());
        if self.scroll >= self.max_scroll() {
            self.autoscroll = true;
            self.new_since_scroll = 0;
        }
    }

    /// Jump to the top (disengages autoscroll).
    pub fn scroll_top(&mut self) {
        self.scroll = 0;
        self.autoscroll = false;
    }

    /// Jump to the bottom and re-engage autoscroll (`G`/End), clearing the new-turn badge.
    pub fn scroll_bottom(&mut self) {
        self.scroll = self.max_scroll();
        self.autoscroll = true;
        self.new_since_scroll = 0;
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

    fn turn(seq: i32) -> TranscriptRow {
        TranscriptRow {
            seq,
            role: "assistant".into(),
            content: Some(format!("turn {seq}")),
            tool_calls: None,
            tool_name: None,
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            model: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn detail_autoscroll_pins_to_bottom_as_turns_arrive() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript(vec![turn(0), turn(1), turn(2)]);
        // 10 content lines in a 4-line viewport → max scroll 6; autoscroll pins there.
        d.set_geometry(10, 4);
        assert!(d.autoscroll);
        assert_eq!(d.scroll, 6, "pinned to the bottom");

        // A new turn extends the content; autoscroll keeps us pinned.
        let added = d.merge_transcript(vec![turn(3)]);
        assert_eq!(added, 1);
        d.set_geometry(13, 4);
        assert_eq!(d.scroll, 9, "still pinned after growth");
        assert_eq!(d.new_since_scroll, 0, "no badge while pinned");
    }

    #[test]
    fn detail_hold_position_and_count_new_when_scrolled_up() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript((0..5).map(turn).collect());
        d.set_geometry(20, 5); // max scroll 15, pinned at 15
        assert_eq!(d.scroll, 15);

        // User scrolls up → autoscroll disengages, position held.
        d.scroll_up(6);
        assert!(!d.autoscroll);
        assert_eq!(d.scroll, 9);

        // New turns arrive: position must NOT jump; the badge counts them.
        d.merge_transcript(vec![turn(5), turn(6)]);
        d.set_geometry(28, 5);
        assert_eq!(d.scroll, 9, "held while scrolled up");
        assert_eq!(d.new_since_scroll, 2, "▼ 2 new");

        // G/End re-engages autoscroll, clears the badge, jumps to bottom.
        d.scroll_bottom();
        assert!(d.autoscroll);
        assert_eq!(d.new_since_scroll, 0);
        assert_eq!(d.scroll, d.max_scroll());
    }

    #[test]
    fn detail_merge_dedupes_by_seq_and_orders() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript(vec![turn(2), turn(0)]);
        // Re-fetch overlaps seq 0 and 2, adds 1 and 3 (out of order).
        let added = d.merge_transcript(vec![turn(0), turn(3), turn(1), turn(2)]);
        assert_eq!(added, 2, "only the genuinely-new seqs counted");
        let seqs: Vec<i32> = d.transcript.iter().map(|t| t.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3], "sorted, no dupes");
    }

    #[test]
    fn detail_tail_in_flight_guard_gates_polling() {
        let mut d = DetailState::new(task_row("running"), true);
        // Live + readable + no fetch outstanding → poll.
        assert!(d.should_poll(), "first tick may poll");

        // Simulate a spawn: the guard is set, so the next tick must NOT poll again.
        d.tail_in_flight = true;
        assert!(
            !d.should_poll(),
            "in-flight guard blocks a second overlapping poll"
        );

        // When the result lands the guard clears and polling resumes.
        d.tail_in_flight = false;
        assert!(d.should_poll(), "cleared guard re-enables polling");

        // A terminal status stops polling regardless of the guard.
        d.tail_in_flight = false;
        d.set_task(task_row("succeeded"));
        assert!(!d.should_poll(), "terminal status stops the tail");
    }

    #[test]
    fn detail_set_task_flips_live_off_on_terminal_status() {
        let mut d = DetailState::new(task_row("running"), true);
        assert!(d.live);
        d.set_task(task_row("succeeded"));
        assert!(!d.live, "terminal status stops the tail");
        assert!(!d.should_poll());
    }

    #[test]
    fn scroll_down_to_bottom_reengages_autoscroll() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript((0..5).map(turn).collect());
        d.set_geometry(20, 5);
        d.scroll_top();
        assert!(!d.autoscroll);
        assert_eq!(d.scroll, 0);
        // Scroll all the way back down re-engages the tail.
        d.scroll_down(100);
        assert!(d.autoscroll);
        assert_eq!(d.scroll, 15);
    }
}
