//! The agent-plane selection axes: **mode** × **host** (ADR-0085).
//!
//! One `agent-plane` binary owns every checkout-bearing task, selected on two independent axes:
//!
//! - [`Mode`] — *what work*: `index`, `review`, `open`.
//! - [`Host`] — *how deployed*: `run-once` (do one task, exit — a dispatcher-spawned Job,
//!   [ADR-0004](../../docs/adr/0004-one-k8s-job-per-task.md)) or `serve` (a long-lived Deployment).
//!
//! This module encodes the matrix and its **structural** routing rules as data + a [`validate`]
//! guard. `open` (slice 4, [ADR-0088]) now **routes** (`open + run-once` is admitted); `serve` (slice
//! 5) is still not built. The *permanent* structural rule (`open` can never run under `serve`) is kept
//! distinct from the *temporary* "deferred to a later slice" rejection so slice 5 slots in by deleting
//! a match arm, not rewriting the guard.
//!
//! Admitting `open + run-once` is a **routing** decision, not an execution one: it says the pair is a
//! legal cell of the matrix. The dormant machinery it routes to — the write-capable loop assembly
//! ([`lci-open-agent`](../../../open-agent)), the mediated PR-open egress, and the hardened sandbox Job
//! spec — lands in this slice too, but no trigger creates an `open` task and the `run-once` host does
//! not yet drive the open loop (it refuses `Mode::Open` rather than mis-running it — see [`crate::run`]).
//! Activation is gated on a security sign-off (ADR-0088).
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

/// The distinct rejection reasons [`validate`] can return. One variant per rejected matrix cell;
/// each `#[error(...)]` text is verbatim what the old `Result<_, String>` returned, so this is a
/// pure typing change — callers that interpolate the error into a message (e.g. `agent-plane`'s
/// `{reason}` in its startup `eprintln!`) see byte-identical output via the derived `Display`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaneValidationError {
    /// `open` + `serve` — permanent structural rejection (never becomes legal, even after slice 5
    /// builds `serve`).
    #[error(
        "open mode cannot run under the serve host: it executes untrusted and generated code, \
         and a shared serve tenant cannot sandbox arbitrary execution — namespaces isolate \
         files, not execution (ADR-0085). open is run-once only."
    )]
    OpenUnderServeForbidden,
    /// any other mode + `serve` — temporary, lands in slice 5.
    #[error(
        "the serve host is not implemented yet: it arrives in slice 5. index and review run \
         under run-once today (ADR-0085)."
    )]
    ServeNotYetImplemented,
}

/// The mode×host routing guard (ADR-0085 §"The mode × host matrix and its routing rules").
///
/// Returns `Ok(())` iff the pair is admissible for **this** slice. The arms encode the whole matrix
/// so the rejection reason is precise:
///
/// - **`open` + `serve` — permanent structural rejection.** `open` executes untrusted + generated
///   code; a shared `serve` tenant cannot sandbox arbitrary execution (Linux user namespaces isolate
///   *files*, not execution). This cell is *forbidden by construction*, not merely unbuilt.
/// - **`open` + `run-once` — admitted (slice 4, [ADR-0088]).** `open` routes to `run-once`
///   *unconditionally* — a security property, not a tunable. The write-capable loop, the mediated
///   PR-open egress, and the hardened sandbox Job spec are the dormant machinery this admits; no
///   trigger creates an `open` task yet, so the cell is legal but never exercised in prod.
/// - **any mode + `serve` — deferred to slice 5.** `run-once` is the default and only host today;
///   `serve` re-owns concurrency bounding / stale reclaim / (for execution) sandboxing that k8s Jobs
///   give free, so it is opt-in and gated on a measurement.
/// - **`index` | `review` + `run-once` — admitted.** Behaviour-identical to today's Jobs.
pub fn validate(mode: Mode, host: Host) -> Result<(), PlaneValidationError> {
    match (mode, host) {
        // Permanent structural rule — never becomes legal, even after slice 5 builds `serve`.
        (Mode::Open, Host::Serve) => Err(PlaneValidationError::OpenUnderServeForbidden),
        // Temporary — lands in slice 5.
        (_, Host::Serve) => Err(PlaneValidationError::ServeNotYetImplemented),
        // The admitted cells for this slice: index/review/open under run-once. `open` routing is a
        // security property (ADR-0088: open → run-once, always); its host execution is still dormant.
        (Mode::Index | Mode::Review | Mode::Open, Host::RunOnce) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admitted_cells_are_index_review_and_open_run_once() {
        assert!(validate(Mode::Index, Host::RunOnce).is_ok());
        assert!(validate(Mode::Review, Host::RunOnce).is_ok());
        // Slice 4 (ADR-0088): open now ROUTES under run-once. This is a routing admission — the host
        // execution stays dormant (see `crate::run`), but the matrix cell is legal.
        assert!(validate(Mode::Open, Host::RunOnce).is_ok());
    }

    #[test]
    fn open_under_serve_is_a_permanent_structural_rejection() {
        let err = validate(Mode::Open, Host::Serve)
            .expect_err("open+serve must be rejected")
            .to_string();
        // The message names the *structural* reason (execution isolation), not "not implemented",
        // so a future reader doesn't mistake it for a slice-5 gap that will open up.
        assert!(err.contains("run-once only"), "unexpected reason: {err}");
        assert!(err.contains("execution"), "unexpected reason: {err}");
    }

    #[test]
    fn open_run_once_is_admitted_in_slice_4() {
        // The routing cell is legal (ADR-0088: open → run-once, always). Host execution is dormant
        // and refused separately in `crate::run` — the guard's job is only the mode×host matrix.
        assert!(validate(Mode::Open, Host::RunOnce).is_ok());
    }

    #[test]
    fn serve_is_deferred_to_slice_5_for_built_modes() {
        for mode in [Mode::Index, Mode::Review] {
            let err = validate(mode, Host::Serve)
                .expect_err("serve host is unbuilt this slice")
                .to_string();
            assert!(
                err.contains("slice 5"),
                "unexpected reason for {mode:?}: {err}"
            );
        }
    }

    #[test]
    fn open_serve_stays_forbidden_while_open_run_once_is_admitted() {
        // The permanent structural rule (open+serve forbidden) must NOT relax just because slice 4
        // admitted open+run-once: the serve arm stays forever, the run-once arm is now legal.
        let serve = validate(Mode::Open, Host::Serve).unwrap_err().to_string();
        assert!(
            serve.contains("run-once only"),
            "unexpected reason: {serve}"
        );
        assert!(validate(Mode::Open, Host::RunOnce).is_ok());
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
