//! The `agent-plane` binary — one checkout-bearing executable selected by **mode × host** (ADR-0085).
//!
//! This is the additive, prod-neutral first increment of the agent-plane consolidation (RFC-0007
//! slice 2). It parses `--mode {index|review}` and `--host run-once` (flag or env), passes the
//! [`agent_runner::plane`] routing guard, and then drives the **same** `run-once` orchestration the
//! `agent-runner` binary uses ([`agent_runner::run_once`]) — so a pod behaves identically whether it
//! is launched as `agent-runner` (no flags, today's dispatcher) or `agent-plane --mode … --host
//! run-once` (the later one-line dispatcher cutover).
//!
//! Scope of this slice (see the guard in [`agent_runner::plane::validate`]):
//! - `index` / `review` under `run-once` — wired, behaviour-identical to today's Jobs.
//! - `open` mode — enum + guard scaffolding only; the toolset/sandbox land in slice 4 (ADR-0088).
//! - `serve` host — enum + guard scaffolding only; lands in slice 5.
//!
//! When `--mode` is **omitted**, the binary passes `None` to `run_once`, which infers index-vs-review
//! from the task exactly as `agent-runner` does — so this binary is a safe drop-in even before the
//! dispatcher passes the flag.

use agent_runner::plane::{Host, Mode};
use clap::{Parser, ValueEnum};

// Global allocator — static-musl images (ADR-0080), matching the `agent-runner` binary. The plane is
// allocation-heavy (clone walk, tree-sitter parse, embedding batches); mimalloc avoids musl malloc's
// multithreaded regression.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The agent-plane entrypoint (ADR-0085). Mode and host are env-or-flag; the flag wins over the env.
#[derive(Parser, Debug)]
#[command(
    name = "agent-plane",
    about = "Lightbridge agent execution plane (mode × host, ADR-0085)"
)]
struct Cli {
    /// What work to do. Omitted → inferred from the task's `command`, exactly as `agent-runner` does
    /// today (so the binary is a drop-in before the dispatcher passes the flag).
    #[arg(long, value_enum, env = "AGENT_MODE")]
    mode: Option<ModeArg>,

    /// How the plane is deployed. Only `run-once` is implemented this slice; `serve` is rejected by
    /// the guard until slice 5.
    #[arg(long, value_enum, env = "AGENT_HOST", default_value = "run-once")]
    host: HostArg,
}

/// CLI/env spelling of [`Mode`] (kebab-case tokens: `index` | `review` | `open`).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Index,
    Review,
    Open,
}

/// CLI/env spelling of [`Host`] (kebab-case tokens: `run-once` | `serve`).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum HostArg {
    RunOnce,
    Serve,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Index => Mode::Index,
            ModeArg::Review => Mode::Review,
            ModeArg::Open => Mode::Open,
        }
    }
}

impl From<HostArg> for Host {
    fn from(h: HostArg) -> Self {
        match h {
            HostArg::RunOnce => Host::RunOnce,
            HostArg::Serve => Host::Serve,
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let host: Host = cli.host.into();
    let mode: Option<Mode> = cli.mode.map(Into::into);

    // Enforce the ADR-0085 routing matrix at startup. With an explicit mode we validate the full
    // (mode, host) cell; with no mode (the drop-in default) we only need the host to be admissible —
    // `run_once` will infer the mode from the task, and only `index`/`review` are inferable.
    let guard = match mode {
        Some(mode) => agent_runner::plane::validate(mode, host),
        // No mode → the runner infers index/review from the task, and both are `run-once` only. We
        // only need the host to be admissible; `serve` is still deferred to slice 5. Delegate the
        // serve rejection to `plane::validate` (via a representative inferable mode) so the deferral
        // string lives in exactly one place instead of being duplicated here.
        None => match host {
            Host::RunOnce => Ok(()),
            Host::Serve => agent_runner::plane::validate(Mode::Index, host),
        },
    };
    if let Err(reason) = guard {
        // A misrouted plane must fail loud and non-zero — never silently fall back to a mode/host it
        // wasn't asked for. No task status is reported here: config errors happen before we touch a
        // task, so there is nothing to clobber.
        eprintln!("agent-plane: refusing to start: {reason}");
        return std::process::ExitCode::FAILURE;
    }

    // Admitted: drive the shared `run-once` host. `mode` is `Some` only for an explicit `--mode`;
    // otherwise `None` reproduces `agent-runner`'s inference.
    agent_runner::run_once(mode).await
}
