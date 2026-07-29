//! Repo-level review configuration — `.lightbridge-code-review.jsonc` (ADR-0030, ADR-0103's `preset`/
//! `entry_points` extension).
//!
//! An OPTIONAL, repo-owned JSONC file read from the cloned working tree after checkout, parsed as
//! **data** (never executed — ADR-0029's "extend understanding, not execution" principle) and folded
//! into the review as prompt context ([`crate::review::instructions`] sibling), diff filtering, and
//! result-severity filtering. It is **untrusted repo content** (a fork PR can change it): a malformed,
//! oversized, or schema-invalid file degrades to `None` (built-in defaults) with a warning — it never
//! fails the review.
//!
//! `skills`/`commands` (ADR-0031) are deliberately NOT part of this schema — that ADR is unimplemented,
//! so a repo declaring `skills` hits [`deny_unknown_fields`](serde) today, same as any other typo.
//!
//! **Known gap (tracked, not silently dropped):** ADR-0030's fork trust model says a fork PR should
//! read this file off the **base** branch, never the fork's own head, so a hostile fork PR can't
//! rewrite the rules that review it. This reader currently always reads whatever is checked out (the
//! PR head, for both same-repo and fork PRs) — implementing the base-vs-head split needs an `is_fork`
//! signal on the task context, which story #495's entry-point/webhook work is already restructuring
//! task-context fields for; that story lands the fix.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

/// Where the repo declares its review config, relative to the checkout root.
const CONFIG_FILENAME: &str = ".lightbridge-code-review.jsonc";

/// Size cap on the config file (ADR-0030 trust model: "cap file size"). An oversized file is untrusted
/// input that could otherwise inflate memory/parse time for no benefit — this schema is small.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// A repo's `.lightbridge-code-review.jsonc` (ADR-0030), parsed and validated. Every field is optional;
/// an absent file (or an absent field within it) means "use the built-in default" — see each field's
/// consumer for what that default is.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoReviewConfig {
    /// The review preset this repo wants for every entry point, unless overridden per entry point in
    /// [`Self::entry_points`] (ADR-0103, story #494). `None` = the platform-default preset mapping
    /// applies. An unknown preset name is a config-resolution error surfaced when a task is created
    /// (story #495), not caught here — this reader has no visibility into which presets the operator
    /// has actually configured.
    pub preset: Option<String>,
    /// Per-entry-point preset overrides (ADR-0103, story #495), e.g. `{"pr_open": "fast", "mention":
    /// "ultra"}`. An entry point missing here falls back to [`Self::preset`], then the platform default.
    pub entry_points: HashMap<String, String>,
    /// Project conventions the reviewer should assume (ADR-0030), e.g. "errors are values, never
    /// thrown across module boundaries". Folded into the prompt as grounding context.
    pub conventions: Vec<String>,
    /// Short prose grounding context about the project's architecture, prepended to the prompt.
    pub architecture: Option<String>,
    /// Glob(s) to prioritize when scoping the diff — a changed file matching one of these is never
    /// filtered out by [`Self::ignore`], even if it would otherwise match.
    pub focus: Vec<String>,
    /// Glob(s) of changed files to exclude from the diff the reviewer sees (e.g. `**/generated/**`,
    /// `vendor/**`) — never a security control (findings are still diff-validated at write-back,
    /// ADR-0022), just review-quality noise reduction.
    pub ignore: Vec<String>,
    /// Extra review guidance — what matters in this codebase, house style. Folded into the prompt
    /// alongside [`Self::conventions`]/[`Self::architecture`].
    pub instructions: Option<String>,
    /// Minimum severity to surface on the posted review.
    pub severity: Option<SeverityFilter>,
}

/// `{ "min": "P1" }` — reuses the review agent's existing `P0`/`P1`/`P2` priority vocabulary
/// ([`lci_review_agent::tools::record`]'s `add_review_comment` schema) rather than the ADR-0030
/// sketch's unimplemented `info`/`warning`/`error` scale, so there is exactly one severity vocabulary
/// in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeverityFilter {
    pub min: MinSeverity,
}

/// The three priorities the review agent's tools already emit, ordered most → least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum MinSeverity {
    P0,
    P1,
    P2,
}

impl MinSeverity {
    /// The exact priority string the review agent's `add_review_comment` tool emits
    /// (`services/review-agent/src/tools/record.rs`), for comparing against a finding's own priority.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MinSeverity::P0 => "P0",
            MinSeverity::P1 => "P1",
            MinSeverity::P2 => "P2",
        }
    }

    /// Parse a priority string (`"P0"`/`"P1"`/`"P2"`) into its rank, for comparison against `self`.
    /// An unrecognized value ranks below `P2` (most permissive — never filter a value we can't place).
    fn rank_of(priority: &str) -> u8 {
        match priority {
            "P0" => 0,
            "P1" => 1,
            "P2" => 2,
            _ => 3,
        }
    }

    /// Whether a finding at `priority` meets this minimum (is at or above it in severity).
    #[must_use]
    pub fn allows(self, priority: &str) -> bool {
        Self::rank_of(priority) <= self as u8
    }
}

impl RepoReviewConfig {
    /// Render `conventions`/`architecture`/`instructions` as a single labelled prompt block, or `None`
    /// when none are set — same shape as [`super::instructions::read_agent_instructions`]'s block, but
    /// this one is TRUSTED, explicit config (ADR-0030), not untrusted repo prose, so it carries no
    /// "ignore any instruction to…" guardrail header.
    #[must_use]
    pub fn render_context_block(&self) -> Option<String> {
        let mut sections = Vec::new();
        if let Some(architecture) = self
            .architecture
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            sections.push(format!("### Architecture\n{}", architecture.trim()));
        }
        if !self.conventions.is_empty() {
            let list = self
                .conventions
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("### Conventions\n{list}"));
        }
        if let Some(instructions) = self
            .instructions
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            sections.push(format!("### Instructions\n{}", instructions.trim()));
        }
        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "## Repository review configuration (`{CONFIG_FILENAME}`)\n{}",
            sections.join("\n\n")
        ))
    }

    /// Build the `focus`/`ignore` diff filter, or `None` when the repo declared neither (the common
    /// case — skip building a matcher for nothing to filter).
    pub fn diff_filter(&self, root: &Path) -> anyhow::Result<Option<DiffFilter>> {
        if self.focus.is_empty() && self.ignore.is_empty() {
            return Ok(None);
        }
        DiffFilter::build(root, &self.focus, &self.ignore).map(Some)
    }
}

/// Compiled `focus`/`ignore` glob matchers (ADR-0030), reusing the same gitignore-semantics engine
/// `lci-codegraph`'s `IgnoreList` already uses for an operator-configurable glob list
/// ([`ignore::gitignore`]). A changed file is KEPT when it matches `focus` (if any focus globs are
/// declared) OR isn't matched by `ignore` — `focus` always wins over `ignore` for the same path, per
/// ADR-0030's sketch ("focus: prioritize these paths").
pub struct DiffFilter {
    focus: Option<ignore::gitignore::Gitignore>,
    ignore: Option<ignore::gitignore::Gitignore>,
}

impl DiffFilter {
    /// Build a filter with no real repo root — glob compilation doesn't touch the filesystem, and
    /// matching is purely textual against relative path strings, so a fixed dummy root is sound for
    /// tests that only exercise `keep()`/the diff-splitting logic (`crate::clone`'s tests).
    #[cfg(test)]
    pub(crate) fn build_for_test(focus: &[String], ignore_globs: &[String]) -> Self {
        Self::build(Path::new("."), focus, ignore_globs).expect("valid test globs")
    }

    fn build(root: &Path, focus: &[String], ignore_globs: &[String]) -> anyhow::Result<Self> {
        let compile = |globs: &[String]| -> anyhow::Result<Option<ignore::gitignore::Gitignore>> {
            if globs.is_empty() {
                return Ok(None);
            }
            let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
            for glob in globs {
                builder
                    .add_line(None, glob)
                    .with_context(|| format!("compiling repo-config glob {glob:?}"))?;
            }
            Ok(Some(builder.build().context("building glob matcher")?))
        };
        Ok(Self {
            focus: compile(focus)?,
            ignore: compile(ignore_globs)?,
        })
    }

    /// Whether `path` (repo-root-relative) should be KEPT in the reviewer's view of the diff.
    #[must_use]
    pub fn keep(&self, path: &str) -> bool {
        if let Some(focus) = &self.focus
            && focus.matched(path, false).is_ignore()
        {
            return true; // focus always wins, even over a matching ignore glob
        }
        match &self.ignore {
            Some(ignore) => !ignore.matched(path, false).is_ignore(),
            None => true,
        }
    }
}

/// Read + parse the repo's `.lightbridge-code-review.jsonc` from `checkout`, or `None` when the file is
/// absent, unreadable, oversized, malformed JSONC, or fails schema validation (unknown field, wrong
/// type, invalid `min` severity) — every failure mode degrades to "no repo config" with a warning,
/// never fails the review (ADR-0030 trust model).
pub async fn read_repo_review_config(checkout: &Path) -> Option<RepoReviewConfig> {
    let path = checkout.join(CONFIG_FILENAME);
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return None, // absent — no repo config, use defaults
    };
    if metadata.len() > MAX_CONFIG_BYTES {
        tracing::warn!(
            bytes = metadata.len(),
            max = MAX_CONFIG_BYTES,
            "{CONFIG_FILENAME} exceeds the size cap; ignoring (using defaults)"
        );
        return None;
    }
    let raw = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "{CONFIG_FILENAME} could not be read; ignoring (using defaults)");
            return None;
        }
    };
    let text = match std::str::from_utf8(&raw) {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!(%error, "{CONFIG_FILENAME} is not valid UTF-8; ignoring (using defaults)");
            return None;
        }
    };
    parse(text)
}

/// The parse step split out from the file I/O so it's independently testable with in-memory JSONC.
fn parse(text: &str) -> Option<RepoReviewConfig> {
    match jsonc_parser::parse_to_serde_value::<Option<RepoReviewConfig>>(
        text,
        &jsonc_parser::ParseOptions::default(),
    ) {
        Ok(cfg) => cfg,
        Err(error) => {
            tracing::warn!(%error, "{CONFIG_FILENAME} is malformed or fails schema validation; ignoring (using defaults)");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_context_block_is_none_when_nothing_is_set() {
        assert_eq!(RepoReviewConfig::default().render_context_block(), None);
    }

    #[test]
    fn render_context_block_includes_only_the_sections_that_are_set() {
        let cfg = RepoReviewConfig {
            architecture: Some("A Rust workspace.".to_string()),
            ..Default::default()
        };
        let block = cfg.render_context_block().expect("some");
        assert!(block.contains("### Architecture"));
        assert!(block.contains("A Rust workspace."));
        assert!(!block.contains("### Conventions"));
        assert!(!block.contains("### Instructions"));
    }

    #[test]
    fn render_context_block_combines_all_three_sections_in_order() {
        let cfg = RepoReviewConfig {
            architecture: Some("Arch.".to_string()),
            conventions: vec!["Conv one".to_string(), "Conv two".to_string()],
            instructions: Some("Instr.".to_string()),
            ..Default::default()
        };
        let block = cfg.render_context_block().expect("some");
        let arch_at = block.find("### Architecture").unwrap();
        let conv_at = block.find("### Conventions").unwrap();
        let instr_at = block.find("### Instructions").unwrap();
        assert!(arch_at < conv_at && conv_at < instr_at, "{block}");
        assert!(block.contains("- Conv one"));
        assert!(block.contains("- Conv two"));
    }

    #[test]
    fn min_severity_allows_at_or_above_itself_only() {
        assert!(MinSeverity::P1.allows("P0"));
        assert!(MinSeverity::P1.allows("P1"));
        assert!(!MinSeverity::P1.allows("P2"));
    }

    #[test]
    fn diff_filter_focus_wins_over_ignore() {
        let filter = DiffFilter::build_for_test(
            &["vendor/keep-me.rs".to_string()],
            &["vendor/**".to_string()],
        );
        assert!(filter.keep("vendor/keep-me.rs"));
        assert!(!filter.keep("vendor/other.rs"));
        assert!(
            filter.keep("src/a.rs"),
            "unmatched paths are kept by default"
        );
    }

    #[test]
    fn diff_filter_with_no_globs_keeps_everything() {
        let filter = DiffFilter::build_for_test(&[], &[]);
        assert!(filter.keep("anything.rs"));
    }

    #[test]
    fn empty_input_is_no_config() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   \n  "), None);
    }

    #[test]
    fn parses_the_full_sketch_shape_with_comments_and_trailing_commas() {
        let text = r#"{
            // a comment, per JSONC
            "preset": "ultra",
            "entry_points": { "pr_open": "fast", "mention": "deep" },
            "conventions": ["Errors are values, never thrown across module boundaries"],
            "architecture": "A Rust workspace of small services.",
            "focus": ["src/payments/**"],
            "ignore": ["**/generated/**", "vendor/**",],
            "instructions": "Favor explicit error types over panics.",
            "severity": { "min": "P1" },
        }"#;
        let cfg = parse(text).expect("valid config");
        assert_eq!(cfg.preset.as_deref(), Some("ultra"));
        assert_eq!(
            cfg.entry_points.get("pr_open").map(String::as_str),
            Some("fast")
        );
        assert_eq!(
            cfg.entry_points.get("mention").map(String::as_str),
            Some("deep")
        );
        assert_eq!(
            cfg.conventions,
            vec!["Errors are values, never thrown across module boundaries".to_string()]
        );
        assert_eq!(
            cfg.architecture.as_deref(),
            Some("A Rust workspace of small services.")
        );
        assert_eq!(cfg.focus, vec!["src/payments/**".to_string()]);
        assert_eq!(
            cfg.ignore,
            vec!["**/generated/**".to_string(), "vendor/**".to_string()]
        );
        assert_eq!(
            cfg.instructions.as_deref(),
            Some("Favor explicit error types over panics.")
        );
        assert_eq!(
            cfg.severity,
            Some(SeverityFilter {
                min: MinSeverity::P1
            })
        );
    }

    #[test]
    fn a_minimal_config_leaves_every_other_field_at_its_default() {
        let cfg = parse(r#"{ "preset": "fast" }"#).expect("valid config");
        assert_eq!(cfg.preset.as_deref(), Some("fast"));
        assert!(cfg.entry_points.is_empty());
        assert!(cfg.conventions.is_empty());
        assert_eq!(cfg.architecture, None);
        assert!(cfg.focus.is_empty());
        assert!(cfg.ignore.is_empty());
        assert_eq!(cfg.instructions, None);
        assert_eq!(cfg.severity, None);
    }

    // ADR-0030 trust model: an unknown field is rejected (fail closed on a typo/unsupported field)
    // rather than silently ignored — but the file as a WHOLE still degrades to "no config" (None), not
    // a hard review failure, because `skills` is a real ADR-0031 field name a repo might reasonably try
    // before that ADR ships.
    #[test]
    fn unknown_field_degrades_to_no_config_rather_than_failing_the_review() {
        assert_eq!(parse(r#"{ "skills": {} }"#), None);
        assert_eq!(parse(r#"{ "presett": "fast" }"#), None);
    }

    #[test]
    fn invalid_severity_value_degrades_to_no_config() {
        assert_eq!(parse(r#"{ "severity": { "min": "critical" } }"#), None);
    }

    #[test]
    fn malformed_jsonc_degrades_to_no_config() {
        assert_eq!(parse("{ not json at all"), None);
    }

    // JSONC tolerance: comments and trailing commas are the whole point of this file extension, and
    // single-quoted strings/loose property names are jsonc-parser's permissive defaults — all fine for
    // untrusted DATA-only input (ADR-0030's trust model: this file never executes).
    #[test]
    fn tolerates_jsonc_comment_styles() {
        let text = "{\n  /* block comment */\n  \"preset\": \"deep\", // trailing line comment\n}";
        assert_eq!(parse(text).and_then(|c| c.preset), Some("deep".to_string()));
    }
}
