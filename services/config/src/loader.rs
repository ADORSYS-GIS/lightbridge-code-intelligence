//! Load a JSON config file — or a mounted template file — from disk, applying `{env:…}`
//! substitution ([`crate::substitute`]) before typed deserialization.

use std::path::Path;

use anyhow::Context;
use serde::de::DeserializeOwned;

use crate::substitute::{substitute_env, substitute_value};

/// Load and parse a JSON config file at `path`, substituting `{env:…}` (from the process env) in all
/// string values before deserializing into `T`.
pub fn load<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    load_with(path, &|name| std::env::var(name).ok())
}

/// [`load`] with an injectable env lookup (for tests).
pub fn load_with<T: DeserializeOwned>(
    path: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    substitute_value(&mut value, lookup);
    serde_json::from_value(value).with_context(|| format!("deserializing {}", path.display()))
}

/// Read a mounted template file and substitute `{env:…}` (process env) in its contents. Used for the
/// reviewer's system prompt and any other large templated text the config points at by path.
pub fn load_template(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading template file {}", path.display()))?;
    Ok(substitute_env(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_of;

    #[test]
    fn load_with_parses_substitutes_and_typechecks() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Cfg {
            model: String,
            timeout: u64,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{ "model": "{env:M:-m0}", "timeout": 30 }"#).unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.model, "m0");
        assert_eq!(cfg.timeout, 30);

        let cfg2: Cfg = load_with(&path, &env_of(&[("M", "real")])).unwrap();
        assert_eq!(cfg2.model, "real");
    }
}
