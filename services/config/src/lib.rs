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

use std::path::Path;

use anyhow::Context;
use serde::de::DeserializeOwned;

/// Marker for an env reference: `{env:NAME}` or `{env:NAME:-default}`.
const OPEN: &str = "{env:";
/// Default separator inside an env reference, e.g. `{env:NAME:-fallback}`.
const DEFAULT_SEP: &str = ":-";

/// Substitute every `{env:VAR}` / `{env:VAR:-default}` in `input` using `lookup`.
///
/// Resolution order per reference: `lookup(var)` → the inline `:-default` → empty string. An
/// unterminated `{env:` (no closing `}`) is left verbatim. `lookup` is injected (not hard-wired to
/// the process env) so the behaviour is fully unit-testable.
pub fn substitute_with(input: &str, lookup: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            // No closing brace — not a real reference; keep the rest literally.
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = match body.split_once(DEFAULT_SEP) {
            Some((name, default)) => (name.trim(), Some(default)),
            None => (body.trim(), None),
        };
        let value = lookup(name)
            .or_else(|| default.map(str::to_string))
            .unwrap_or_default();
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// [`substitute_with`] bound to the process environment.
pub fn substitute_env(input: &str) -> String {
    substitute_with(input, &|name| std::env::var(name).ok())
}

/// Recursively substitute `{env:…}` in every string within a parsed JSON value.
pub fn substitute_value(value: &mut serde_json::Value, lookup: &impl Fn(&str) -> Option<String>) {
    match value {
        serde_json::Value::String(s) => *s = substitute_with(s, lookup),
        serde_json::Value::Array(items) => {
            for item in items {
                substitute_value(item, lookup);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                substitute_value(v, lookup);
            }
        }
        _ => {}
    }
}

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

/// `serde` field deserializers that accept **a number or a numeric string**. Substitution always
/// yields strings, so a numeric config field written as `"{env:DEADLINE:-3600}"` arrives as the
/// string `"3600"`; these let it deserialize anyway (while a literal JSON number still works). Empty
/// / null → `None`. Annotate numeric `Option` fields with
/// `#[serde(default, deserialize_with = "lci_config::de::opt_u64")]`.
pub mod de {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrStr {
        Int(i64),
        Str(String),
    }

    fn parse_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        match Option::<IntOrStr>::deserialize(d)? {
            None => Ok(None),
            Some(IntOrStr::Int(n)) => Ok(Some(n)),
            Some(IntOrStr::Str(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    trimmed.parse::<i64>().map(Some).map_err(D::Error::custom)
                }
            }
        }
    }

    pub fn opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        parse_opt(d)
    }

    pub fn opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Ok(parse_opt(d)?.map(|n| n.max(0) as u64))
    }

    pub fn opt_usize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
        Ok(parse_opt(d)?.map(|n| n.max(0) as usize))
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FloatOrStr {
        Float(f64),
        Str(String),
    }

    /// Like [`opt_i64`] but for fractional fields (e.g. `temperature`, `top_p`): accepts a JSON number
    /// or a numeric string (substitution always yields strings). Empty / null → `None`.
    pub fn opt_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        match Option::<FloatOrStr>::deserialize(d)? {
            None => Ok(None),
            Some(FloatOrStr::Float(n)) => Ok(Some(n)),
            Some(FloatOrStr::Str(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    trimmed.parse::<f64>().map(Some).map_err(D::Error::custom)
                }
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrStr {
        Bool(bool),
        Str(String),
    }

    /// Like [`opt_i64`] but for boolean flags (e.g. `review.stream`): accepts a JSON bool or a string
    /// (`{env:…}` substitution always yields strings), so `"true"`/`"1"`/`"yes"`/`"on"` → `true` and
    /// `"false"`/`"0"`/`"no"`/`"off"` → `false` (case-insensitive). Empty / null → `None`; any other
    /// string is an error rather than a silent default.
    pub fn opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        match Option::<BoolOrStr>::deserialize(d)? {
            None => Ok(None),
            Some(BoolOrStr::Bool(b)) => Ok(Some(b)),
            Some(BoolOrStr::Str(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                match trimmed.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => Ok(Some(true)),
                    "false" | "0" | "no" | "off" => Ok(Some(false)),
                    other => Err(D::Error::custom(format!(
                        "invalid boolean {other:?} (expected true/false/1/0/yes/no/on/off)"
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn substitutes_present_var() {
        let look = env_of(&[("FOO", "bar")]);
        assert_eq!(substitute_with("a-{env:FOO}-b", &look), "a-bar-b");
    }

    #[test]
    fn falls_back_to_inline_default_then_empty() {
        let look = env_of(&[]);
        assert_eq!(substitute_with("{env:MISSING:-def}", &look), "def");
        assert_eq!(substitute_with("x{env:MISSING}y", &look), "xy");
    }

    #[test]
    fn present_var_wins_over_default() {
        let look = env_of(&[("MODEL", "qwen")]);
        assert_eq!(substitute_with("{env:MODEL:-fallback}", &look), "qwen");
    }

    #[test]
    fn handles_multiple_and_literals_and_unterminated() {
        let look = env_of(&[("A", "1"), ("B", "2")]);
        assert_eq!(substitute_with("{env:A}/{env:B}!", &look), "1/2!");
        assert_eq!(substitute_with("no refs here", &look), "no refs here");
        // An unterminated reference is left exactly as-is.
        assert_eq!(substitute_with("oops {env:A", &look), "oops {env:A");
    }

    #[test]
    fn default_may_contain_colons_and_urls() {
        let look = env_of(&[]);
        assert_eq!(
            substitute_with("{env:URL:-https://gw:443/v1}", &look),
            "https://gw:443/v1"
        );
    }

    #[test]
    fn substitutes_through_a_json_tree() {
        let look = env_of(&[("KEY", "secret"), ("N", "5")]);
        let mut v = serde_json::json!({
            "review": { "api_key": "{env:KEY}", "model": "{env:MODEL:-default-model}" },
            "list": ["{env:N}", "plain"]
        });
        substitute_value(&mut v, &look);
        assert_eq!(v["review"]["api_key"], "secret");
        assert_eq!(v["review"]["model"], "default-model");
        assert_eq!(v["list"][0], "5");
        assert_eq!(v["list"][1], "plain");
    }

    #[test]
    fn numeric_fields_accept_env_substituted_strings() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "de::opt_u64")]
            timeout: Option<u64>,
            #[serde(default, deserialize_with = "de::opt_usize")]
            size: Option<usize>,
        }
        // A `{env:…}`-substituted numeric field arrives as a string; it must still deserialize.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{ "timeout": "{env:T:-45}", "size": 1000 }"#).unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.timeout, Some(45), "numeric string from default coerces");
        assert_eq!(cfg.size, Some(1000), "literal number still works");
    }

    #[test]
    fn float_fields_accept_env_substituted_strings() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "de::opt_f64")]
            temperature: Option<f64>,
            #[serde(default, deserialize_with = "de::opt_f64")]
            top_p: Option<f64>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // temperature is a substituted string ("0.2"); top_p is a literal JSON number.
        std::fs::write(
            &path,
            r#"{ "temperature": "{env:TEMP:-0.2}", "top_p": 0.9 }"#,
        )
        .unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.temperature, Some(0.2), "numeric string coerces to f64");
        assert_eq!(cfg.top_p, Some(0.9), "literal float still works");
    }

    #[test]
    fn bool_fields_accept_env_substituted_strings() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "de::opt_bool")]
            stream: Option<bool>,
            #[serde(default, deserialize_with = "de::opt_bool")]
            literal: Option<bool>,
            #[serde(default, deserialize_with = "de::opt_bool")]
            unset: Option<bool>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // `stream` is an env-substituted string ("true"); `literal` is a JSON bool; `unset` resolves to
        // an empty string and must become None (not an error) — the same shape as an omitted `{env:}`.
        std::fs::write(
            &path,
            r#"{ "stream": "{env:LLM_STREAM:-true}", "literal": false, "unset": "{env:NOPE}" }"#,
        )
        .unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.stream, Some(true), "bool string from default coerces");
        assert_eq!(cfg.literal, Some(false), "literal JSON bool still works");
        assert_eq!(cfg.unset, None, "empty substitution → None, not an error");
    }

    #[test]
    fn bool_field_rejects_a_non_boolean_string() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Cfg {
            // Deserialization is expected to fail, so the field is never read — the test asserts the
            // error, not the value.
            #[serde(default, deserialize_with = "de::opt_bool")]
            #[allow(dead_code)]
            stream: Option<bool>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{ "stream": "maybe" }"#).unwrap();
        // A garbage value is surfaced as an error, not silently defaulted.
        assert!(load_with::<Cfg>(&path, &env_of(&[])).is_err());
    }

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
