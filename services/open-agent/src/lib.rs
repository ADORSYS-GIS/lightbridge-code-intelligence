//! Open-mode assembly for the Lightbridge agent loop (ADR-0088).
//!
//! `open` is the write-capable autonomous ticket→PR agent. It is the **highest-risk** surface in the
//! system: it both *writes* code (LLM-generated edits to a real working tree) and *executes* code
//! (builds/tests over untrusted repo content + its own generated code). Containment lives at the pod
//! boundary (the hardened sandbox Job spec in `control-plane/src/integrations/k8s.rs`), not here.
//!
//! This crate is `open`'s counterpart to `lci-review-agent`: it composes the shared [`lci_agent_loop`]
//! `AgentLoop` with a **write-and-execute** tool set, `open` budgets, and an `open` prompt. It reuses
//! the exact same generic loop, policy, and durability seams (`StepRuntime`) as every other mode — the
//! mode *is* the toolset, not a fork of the loop.
//!
//! **Credential-light trust boundary (the crux, ADR-0037 extended from comments to code):** the open
//! agent holds **no forge credential and no DB handle**. It edits + commits to a *local* branch in its
//! sandbox, then the terminal [`tools::propose_pr`] hands the branch to the control plane through the
//! mediated internal API ([`lci_agent_clients::ControlPlaneClient`]); the egress plane (which holds the
//! forge creds) pushes the branch and opens the PR. The dependency graph enforces this: this crate
//! depends on `lci-agent-clients` (the mediated channel) and nothing that carries a forge or database
//! credential.
//!
//! **Dormant:** the machinery here is additive and not yet driven by any binary — no trigger creates an
//! `open` task and the `run-once` host refuses `Mode::Open` (see `agent-runner/src/run.rs`). Activation
//! is gated on a security sign-off.

pub mod flows;
pub mod policies;
pub mod prompt;
pub mod tools;
pub mod workspace;

pub use workspace::{SandboxWorkspace, resolve_read, resolve_write};
