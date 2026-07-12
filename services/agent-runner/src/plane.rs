//! The agent-plane selection axes: **mode** × **host** (ADR-0085).
//!
//! One `agent-plane` binary owns every checkout-bearing task, selected on two independent axes:
//!
//! - [`Mode`] — *what work*: `index`, `review`, `open`.
//! - [`Host`] — *how deployed*: `run-once` (do one task, exit — a dispatcher-spawned Job,
//!   [ADR-0004](../../docs/adr/0004-one-k8s-job-per-task.md)) or `serve` (a long-lived Deployment).
//!
//! This module encodes the matrix and its **structural** routing rules as data + a [`validate`]
//! guard, even though `open` (slice 4, [ADR-0088]) and `serve` (slice 5) are not implemented yet:
//! the guard rejects the not-yet-built cells with a clear reason, and the *permanent* structural rule
//! (`open` can never run under `serve`) is distinguished from the *temporary* "deferred to a later
//! slice" rejections so slices 4/5 slot in by deleting a match arm, not rewriting the guard.
//!
//! It carries **no** execution logic — routing an admitted `(mode, host)` pair to the actual work is
//! the entrypoint's job (`run_once`, `main.rs`/`bin/agent_plane.rs`). Keeping the matrix pure makes it
//! unit-testable without a cluster, a control plane, or a checkout.

/// What work the plane does (ADR-0085). `index` builds the code graph + embeddings; `review` is the
/// read-only review loop; `open` is the write-capable autonomous ticket agent (slice 4, ADR-0088 —
/// scaffolding only here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Index,
    Review,
    Open,
}

/// How the plane is deployed (ADR-0085). `run-once` does one task and exits (the dispatcher-spawned
/// Job, today's model); `serve` is a long-lived Deployment accepting many tasks (slice 5 — scaffolding
/// only here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    RunOnce,
    Serve,
}

impl Mode {
    /// The canonical flag/env token for this mode (`index` | `review` | `open`).
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Index => "index",
            Mode::Review => "review",
            Mode::Open => "open",
        }
    }
}

impl Host {
    /// The canonical flag/env token for this host (`run-once` | `serve`).
    pub fn as_str(self) -> &'static str {
        match self {
            Host::RunOnce => "run-once",
            Host::Serve => "serve",
        }
    }
}

/// The mode×host routing guard (ADR-0085 §"The mode × host matrix and its routing rules").
///
/// Returns `Ok(())` iff the pair is admissible for **this** slice. The arms encode the whole matrix
/// so the rejection reason is precise:
///
/// - **`open` + `serve` — permanent structural rejection.** `open` executes untrusted + generated
///   code; a shared `serve` tenant cannot sandbox arbitrary execution (Linux user namespaces isolate
///   *files*, not execution). This cell is *forbidden by construction*, not merely unbuilt.
/// - **`open` + `run-once` — deferred to slice 4** ([ADR-0088]). The mode enum + gated registry are
///   scaffolding here; the toolset/sandbox land later.
/// - **any mode + `serve` — deferred to slice 5.** `run-once` is the default and only host today;
///   `serve` re-owns concurrency bounding / stale reclaim / (for execution) sandboxing that k8s Jobs
///   give free, so it is opt-in and gated on a measurement.
/// - **`index` | `review` + `run-once` — admitted.** Behaviour-identical to today's Jobs.
pub fn validate(mode: Mode, host: Host) -> Result<(), String> {
    match (mode, host) {
        // Permanent structural rule — never becomes legal, even after slice 5 builds `serve`.
        (Mode::Open, Host::Serve) => Err(
            "open mode cannot run under the serve host: it executes untrusted and generated code, \
             and a shared serve tenant cannot sandbox arbitrary execution — namespaces isolate \
             files, not execution (ADR-0085). open is run-once only."
                .to_string(),
        ),
        // Temporary — lands in slice 4 (ADR-0088).
        (Mode::Open, Host::RunOnce) => Err(
            "open mode is not implemented yet: it arrives in slice 4 (ADR-0088). Only index and \
             review are wired in this slice."
                .to_string(),
        ),
        // Temporary — lands in slice 5.
        (_, Host::Serve) => Err(
            "the serve host is not implemented yet: it arrives in slice 5. index and review run \
             under run-once today (ADR-0085)."
                .to_string(),
        ),
        // The two admitted cells for this slice.
        (Mode::Index | Mode::Review, Host::RunOnce) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admitted_cells_are_only_index_and_review_run_once() {
        assert!(validate(Mode::Index, Host::RunOnce).is_ok());
        assert!(validate(Mode::Review, Host::RunOnce).is_ok());
    }

    #[test]
    fn open_under_serve_is_a_permanent_structural_rejection() {
        let err = validate(Mode::Open, Host::Serve).expect_err("open+serve must be rejected");
        // The message names the *structural* reason (execution isolation), not "not implemented",
        // so a future reader doesn't mistake it for a slice-5 gap that will open up.
        assert!(err.contains("run-once only"), "unexpected reason: {err}");
        assert!(err.contains("execution"), "unexpected reason: {err}");
    }

    #[test]
    fn open_run_once_is_deferred_to_slice_4() {
        let err = validate(Mode::Open, Host::RunOnce).expect_err("open is unbuilt this slice");
        assert!(err.contains("slice 4"), "unexpected reason: {err}");
    }

    #[test]
    fn serve_is_deferred_to_slice_5_for_built_modes() {
        for mode in [Mode::Index, Mode::Review] {
            let err = validate(mode, Host::Serve).expect_err("serve host is unbuilt this slice");
            assert!(
                err.contains("slice 5"),
                "unexpected reason for {mode:?}: {err}"
            );
        }
    }

    #[test]
    fn open_serve_and_open_run_once_reject_for_different_reasons() {
        // The permanent structural rule and the temporary slice-4 gate must not collapse into one
        // message — slice 4 deletes the run-once arm but the serve arm stays forever.
        let serve = validate(Mode::Open, Host::Serve).unwrap_err();
        let run_once = validate(Mode::Open, Host::RunOnce).unwrap_err();
        assert_ne!(serve, run_once);
    }

    #[test]
    fn token_round_trips_for_flags_and_env() {
        assert_eq!(Mode::Index.as_str(), "index");
        assert_eq!(Mode::Review.as_str(), "review");
        assert_eq!(Mode::Open.as_str(), "open");
        assert_eq!(Host::RunOnce.as_str(), "run-once");
        assert_eq!(Host::Serve.as_str(), "serve");
    }
}
