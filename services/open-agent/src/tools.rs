//! The open-mode tool assembly: a **write-and-execute** registry, sandbox-scoped (ADR-0088).
//!
//! Every write and every execution is confined to the sandbox workdir; the only mediated *forge* call
//! is the terminal [`propose_pr`], which hands a branch to the egress plane through the control plane
//! (the agent never touches a forge token). Contrast `review`, whose registry is read-only + mediated
//! comment tools and which never executes repository code.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lci_agent_clients::ControlPlaneClient;
use lci_agent_tools::{RegistryError, RuntimeCaps, ToolCx, ToolRegistry};
use lci_agent_types::ToolSpec;

pub mod apply_patch;
pub mod find_files;
pub mod grep;
pub mod propose_pr;
pub mod read_file;
pub mod run_command;
pub mod terminal;
mod walk;

pub use apply_patch::EDIT_FILE;
pub use find_files::FIND_FILES;
pub use grep::GREP;
pub use propose_pr::PROPOSE_PR;
pub use read_file::READ_FILE;
pub use run_command::RUN_COMMAND;
pub use terminal::ABORT;

/// Per-command wall-clock cap for [`run_command`] (ADR-0088 "bounded"). The turn budget (a loop
/// policy) and the pod's `activeDeadlineSeconds` (the sandbox spec) bound the *overall* run; this caps
/// any single command so one hung build can't consume the whole wall-clock budget.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Max bytes of combined stdout+stderr [`run_command`] returns to the model (each stream is truncated).
pub const DEFAULT_COMMAND_OUTPUT_CAP: usize = 32 * 1024;

/// Services the open tools share. Only the mediated control-plane client — no forge, no DB (the trust
/// boundary). `propose_pr` is the sole tool that uses it; the read/write/execute tools are pure
/// sandbox-local filesystem/process operations.
#[derive(Clone)]
pub(crate) struct OpenServices {
    pub client: Arc<ControlPlaneClient>,
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str::<T>(arguments).map_err(|error| {
        format!(
            "error: invalid arguments — {error}. Re-call with arguments matching the tool's schema."
        )
    })
}

/// Materialize the sandbox workdir root, mapping a [`Workspace`](lci_agent_tools::Workspace) failure
/// into the one model-facing error string every tool reports for it. Every tool call starts here.
pub(crate) async fn resolve_root<'a>(cx: &'a ToolCx<'a>) -> Result<&'a Path, String> {
    cx.workspace
        .root()
        .await
        .map_err(|error| format!("error: could not materialize the sandbox workdir: {error}"))
}

/// The complete built-in open surface, in a stable order (navigation → write → execute → terminal).
#[must_use]
pub fn tool_defs() -> Vec<ToolSpec> {
    vec![
        read_file::spec(),
        grep::spec(),
        find_files::spec(),
        apply_patch::spec(),
        run_command::spec(),
        propose_pr::spec(),
        terminal::abort_spec(),
    ]
}

/// The stable open tool names, in the same order as [`tool_defs`].
#[must_use]
pub fn known_tool_names() -> Vec<&'static str> {
    vec![
        READ_FILE,
        GREP,
        FIND_FILES,
        EDIT_FILE,
        RUN_COMMAND,
        PROPOSE_PR,
        ABORT,
    ]
}

/// Assemble the concrete open tools. `command_timeout` bounds each [`run_command`]; `caps` carries the
/// host's replay capabilities — [`propose_pr`] is `NeedsDedupKey`, so a replaying host must supply a
/// per-call dedup key (the registry rejects it at startup otherwise, ADR-0088 O5).
pub fn tool_registry(
    client: Arc<ControlPlaneClient>,
    command_timeout: Duration,
    output_cap: usize,
    caps: RuntimeCaps,
) -> Result<ToolRegistry, RegistryError> {
    let services = OpenServices { client };
    let mut registry = ToolRegistry::new();
    read_file::register(&mut registry, caps)?;
    grep::register(&mut registry, caps)?;
    find_files::register(&mut registry, caps)?;
    apply_patch::register(&mut registry, caps)?;
    run_command::register(&mut registry, command_timeout, output_cap, caps)?;
    propose_pr::register(&mut registry, &services, caps)?;
    terminal::register(&mut registry, caps)?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_keep_a_stable_order_and_full_schemas() {
        let specs = tool_defs();
        assert_eq!(
            specs.iter().map(ToolSpec::name).collect::<Vec<_>>(),
            known_tool_names()
        );
        assert!(
            specs
                .iter()
                .all(|spec| !spec.function.description.is_empty())
        );
        assert!(
            specs
                .iter()
                .all(|spec| spec.function.parameters.is_object())
        );
    }

    #[test]
    fn registry_assembles_the_full_open_surface() {
        let client = Arc::new(ControlPlaneClient::new("http://unused", "tok"));
        let registry = tool_registry(
            client,
            DEFAULT_COMMAND_TIMEOUT,
            DEFAULT_COMMAND_OUTPUT_CAP,
            RuntimeCaps::default(),
        )
        .unwrap();
        assert_eq!(registry.len(), known_tool_names().len());
    }
}
