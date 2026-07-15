//! Shared configuration loader for the Lightbridge services.
//!
//! Both the control plane and the agent runner read a JSON config file (mounted from a Helm
//! ConfigMap) instead of a sprawl of individual env vars. String values — and the contents of any
//! template files the config points at — support **`{env:VAR:-default}`** substitution, so the chart
//! can keep secrets and per-environment values in env (e.g. secret-injected) while the config and
//! templates stay declarative.
//!
//! Design notes:
//! - **JSON** (not YAML) keeps the dependency surface at zero beyond serde; templates are *separate
//!   mounted files*, so the config itself is only scalars and paths.
//! - Loading is **best-effort by design at the call site**: a service treats a missing config path as
//!   "use built-in defaults / legacy env", so prod keeps running until the ConfigMap is mounted.
//! - Substitution is applied to every string in the parsed tree *before* typed deserialization, so a
//!   value like `"{env:LLM_API_KEY}"` resolves regardless of where it sits in the schema.
//!
//! The crate is split by responsibility: [`substitute`] owns the `{env:…}` grammar, [`loader`] owns
//! reading + deserializing config/template files, and [`de`] owns the serde field-level coercions
//! numeric/bool config fields need once substitution has turned them into strings.

mod loader;
mod substitute;

#[cfg(test)]
mod test_support;

pub mod de;

pub use loader::{load, load_template, load_with};
pub use substitute::{substitute_env, substitute_value, substitute_with};
