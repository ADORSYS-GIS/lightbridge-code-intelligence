//! [`SastConfig`] — resolved configuration for the deterministic SAST pass (ADR-0061).

use super::defaults::{
    DEFAULT_SAST_BIN, DEFAULT_SAST_MAX_FINDINGS, DEFAULT_SAST_MIN_SEVERITY, DEFAULT_SAST_RULES,
    DEFAULT_SAST_TIMEOUT_SECS,
};
use super::env::{parse_env_bool, parse_env_u64};
use super::file::FileConfig;

/// Resolved configuration for the deterministic SAST pass (ADR-0061). Absent ⇒ SAST disabled, so this is
/// produced as an `Option` like [`super::review::ReviewConfig`]. Unlike the LLM config there is no
/// fail-closed path: a half-set SAST block falls back to the defaults, because SAST is a best-effort
/// *additive* signal whose absence must never fail a review.
#[derive(Debug, Clone)]
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
    /// Resolve from the file config's `sast` block when present, else env (`SAST_*`). Returns `None`
    /// (SAST disabled) unless `enabled` is explicitly true — opt-in so the feature lights up only once
    /// the opengrep-bearing image is deployed and an operator turns it on.
    pub fn resolve(file: Option<&FileConfig>) -> Option<Self> {
        let f = file.and_then(|f| f.sast.as_ref());
        let enabled = f
            .and_then(|s| s.enabled)
            .or_else(|| parse_env_bool("SAST_ENABLED"))
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let pick = |file_val: Option<&String>, env: &str, default: &str| -> String {
            file_val
                .map(|s| s.to_string())
                .or_else(|| std::env::var(env).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Some(Self {
            bin: pick(f.and_then(|s| s.bin.as_ref()), "SAST_BIN", DEFAULT_SAST_BIN),
            rules: pick(
                f.and_then(|s| s.rules.as_ref()),
                "SAST_RULES",
                DEFAULT_SAST_RULES,
            ),
            min_severity: pick(
                f.and_then(|s| s.min_severity.as_ref()),
                "SAST_MIN_SEVERITY",
                DEFAULT_SAST_MIN_SEVERITY,
            ),
            max_findings: f
                .and_then(|s| s.max_findings)
                .or_else(|| parse_env_u64("SAST_MAX_FINDINGS").map(|n| n as usize))
                .unwrap_or(DEFAULT_SAST_MAX_FINDINGS)
                .max(1),
            timeout_secs: f
                .and_then(|s| s.timeout_secs)
                .or_else(|| parse_env_u64("SAST_TIMEOUT_SECS"))
                .unwrap_or(DEFAULT_SAST_TIMEOUT_SECS)
                .max(1),
        })
    }
}
