//! [`SastConfig`] — the resolved configuration value [`super::scan`] runs against (ADR-0061). Parsing
//! this from the file config / environment stays a bootstrap concern in `agent-runner`
//! (`bootstrap::config::sast::SastConfig::resolve`); this crate only owns the plain value type both the
//! runner's bootstrap and the `run_sast` tool need.

/// Resolved configuration for a SAST scan (ADR-0061).
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
