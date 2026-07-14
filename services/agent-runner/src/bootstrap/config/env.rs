//! Small env-var parsing helpers shared across the config resolvers ([`super::runner`],
//! [`super::embeddings`], [`super::review`], [`super::sast`]). Kept crate-private plumbing, not part
//! of the public config surface.

use uuid::Uuid;

/// Require an env var to be set and non-empty, naming it in the error so a misconfigured Job is
/// diagnosable from the runner's first log line.
pub(crate) fn require(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => anyhow::bail!("{name} is required but unset/empty"),
    }
}

/// Require an env var and parse it as a UUID.
pub(crate) fn parse_required(name: &str) -> anyhow::Result<Uuid> {
    let raw = require(name)?;
    Uuid::parse_str(&raw).map_err(|_| anyhow::anyhow!("{name} is not a valid UUID: {raw:?}"))
}

/// Validate a required config field is non-empty (after `{env:}` substitution), naming the section +
/// field in the error so a misconfigured ConfigMap is diagnosable from the first log line.
pub(crate) fn require_field(section: &str, name: &str, value: &str) -> anyhow::Result<String> {
    if value.trim().is_empty() {
        anyhow::bail!("config {section}.{name} is required but empty");
    }
    Ok(value.to_string())
}

/// Parse a `u64` env var, returning `None` when unset, empty, or unparseable (the caller applies its
/// own default). A bad value is logged so a fat-fingered env is diagnosable rather than silently ignored.
pub(crate) fn parse_env_u64(name: &str) -> Option<u64> {
    match std::env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                tracing::warn!(var = name, value = %raw, "ignoring non-numeric env value; using default");
                None
            }
        },
        _ => None,
    }
}

/// Parse a boolean env var (`1`/`true`/`yes`/`on` = true; `0`/`false`/`no`/`off` = false), returning
/// `None` when unset/empty/unrecognized so the caller applies its own default.
pub(crate) fn parse_env_bool(name: &str) -> Option<bool> {
    match std::env::var(name)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
