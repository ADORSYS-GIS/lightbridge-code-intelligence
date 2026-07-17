//! [`SastConfig`] — the resolved configuration value [`super::scan`] runs against (ADR-0061). Parsing
//! this from the *runner's* file config / environment stays a bootstrap concern in `agent-runner`
//! (`bootstrap::config::sast::resolve_sast_config`); this crate only owns the plain value type both the
//! runner's bootstrap and the `run_sast` tool need.
//!
//! It ALSO owns the small env round-trip ([`SastConfig::to_env_pairs`] / [`SastConfig::from_env`]) used
//! to hand an already-resolved config across the process boundary in the OpenCode review path
//! (ADR-0097): the supervisor (`agent-runner`) resolves the config once, serializes it into the
//! `opencode acp` child's environment, and the stdio `lci-review-mcp` server — which runs the `run_sast`
//! tool in a *separate* process — reconstructs the exact same value. Keeping both halves here (with a
//! round-trip test) is what keeps the two crates' env keys from drifting.

/// Env-var prefix for the OpenCode-path config round-trip (see the module doc). The supervisor writes
/// `{PREFIX}{FIELD}` for each field; `lci-review-mcp` reads them back.
pub const ENV_PREFIX: &str = "LCI_MCP_SAST_";

/// Env var carrying the path to the newline-delimited changed-file list the `run_sast` scan is scoped
/// to in the OpenCode path (the PR diff's file set, the native `SastToolConfig::changed_files`). Written
/// by the supervisor, read by `lci-review-mcp`. Not part of [`SastConfig`] itself — it's the per-run
/// scan scope, not the tool's static config — but co-located here so the whole cross-process contract
/// lives in one module. Its presence is ALSO the signal that SAST is offered this run (the supervisor
/// only sets it when `run_sast` passed the allowlist + diff-present + enabled gate).
pub const ENV_CHANGED_FILES: &str = "LCI_MCP_SAST_CHANGED_FILES";

/// Resolved configuration for a SAST scan (ADR-0061).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SastConfig {
    /// opengrep binary name/path.
    pub bin: String,
    /// `--config` value (a vendored local rules dir by default).
    pub rules: String,
    /// Minimum SARIF level to surface (`error`|`warning`|`note`).
    pub min_severity: String,
    /// Cap on findings posted per review (excess logged, not silently dropped).
    pub max_findings: usize,
    /// Wall-clock ceiling on one scan, seconds.
    pub timeout_secs: u64,
}

impl SastConfig {
    /// Serialize into `(key, value)` env pairs for the `opencode acp` child (see the module doc). The
    /// exact inverse of [`Self::from_env`].
    #[must_use]
    pub fn to_env_pairs(&self) -> Vec<(String, String)> {
        vec![
            (format!("{ENV_PREFIX}BIN"), self.bin.clone()),
            (format!("{ENV_PREFIX}RULES"), self.rules.clone()),
            (format!("{ENV_PREFIX}MIN_SEVERITY"), self.min_severity.clone()),
            (format!("{ENV_PREFIX}MAX_FINDINGS"), self.max_findings.to_string()),
            (format!("{ENV_PREFIX}TIMEOUT_SECS"), self.timeout_secs.to_string()),
        ]
    }

    /// Reconstruct from the environment the supervisor set via [`Self::to_env_pairs`]. Returns `None`
    /// when the config isn't present (`{PREFIX}BIN`/`{PREFIX}RULES` unset) — i.e. SAST wasn't offered
    /// for this run, so `lci-review-mcp` must not register `run_sast`. The numeric fields fall back to
    /// the crate defaults if somehow present-but-unparseable (defensive; the supervisor always writes
    /// well-formed values).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let get = |field: &str| {
            std::env::var(format!("{ENV_PREFIX}{field}"))
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        // `bin` + `rules` are load-bearing (an empty rules path scans nothing) — their absence means
        // "no SAST this run", so fail closed rather than fabricate a config.
        let bin = get("BIN")?;
        let rules = get("RULES")?;
        Some(Self {
            bin,
            rules,
            min_severity: get("MIN_SEVERITY").unwrap_or_else(|| "error".to_string()),
            max_findings: get("MAX_FINDINGS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(25)
                .max(1),
            timeout_secs: get("TIMEOUT_SECS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(300)
                .max(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;

    /// `from_env` reads process-global env; serialize the tests that mutate `LCI_MCP_SAST_*` so they
    /// never race each other (Rust runs test fns in parallel by default).
    static ENV_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn all_keys() -> [String; 5] {
        [
            format!("{ENV_PREFIX}BIN"),
            format!("{ENV_PREFIX}RULES"),
            format!("{ENV_PREFIX}MIN_SEVERITY"),
            format!("{ENV_PREFIX}MAX_FINDINGS"),
            format!("{ENV_PREFIX}TIMEOUT_SECS"),
        ]
    }

    fn clear() {
        for key in all_keys() {
            unsafe { std::env::remove_var(&key) };
        }
    }

    /// `to_env_pairs` → set env → `from_env` recovers the exact config. Guards the two crates' env keys
    /// against drift (the whole point of co-locating both halves here).
    #[test]
    fn env_round_trip_recovers_the_config() {
        let _guard = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let config = SastConfig {
            bin: "/usr/local/bin/opengrep".to_string(),
            rules: "/opt/opengrep-rules".to_string(),
            min_severity: "warning".to_string(),
            max_findings: 17,
            timeout_secs: 42,
        };
        for (key, value) in config.to_env_pairs() {
            unsafe { std::env::set_var(&key, &value) };
        }
        let recovered = SastConfig::from_env();
        clear();
        assert_eq!(recovered, Some(config));
    }

    /// Fail closed: without the load-bearing `BIN`/`RULES` keys, there's no SAST config to reconstruct
    /// (SAST wasn't offered for this run), so `from_env` yields `None` rather than a fabricated config.
    #[test]
    fn from_env_is_none_when_the_config_was_not_offered() {
        let _guard = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        assert_eq!(SastConfig::from_env(), None);
    }
}
