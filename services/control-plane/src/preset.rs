//! Entry-point → review preset resolution (ADR-0103, story #495).
//!
//! Resolves which named preset a task should run under, from the repo's
//! `.lightbridge-code-review.jsonc` (ADR-0030) — fetched via the platform's single-file API
//! ([`CodePlatform::get_repo_file`]), never a clone (control-plane never clones; that's the
//! agent-runner Job's job, after the task already exists) — falling back to the platform-default
//! entry-point → preset mapping when the repo declares nothing.
//!
//! **Fork-safety note:** callers pass the PR's BASE ref (never the head), so a fork PR can't rewrite
//! its own preset by editing the file on its own branch — the same ADR-0030 trust property
//! `agent-runner`'s repo-config reader still owes the rest of the schema (conventions/focus/severity),
//! documented as a known gap there. Preset resolution gets this property for free because a ref was
//! always needed here and the base ref is what's already on hand at every call site.
//!
//! Untrusted repo content: a fetch failure, absent file, oversized file, or malformed/schema-invalid
//! JSONC all degrade to the platform-default mapping — resolution never fails task creation.

use std::collections::HashMap;

use serde::Deserialize;

use crate::integrations::platform::{CodePlatform, RepoRef};

/// Which control-plane trigger is creating this task. Kept distinct from the resolved preset name
/// because a preset is operator-defined (arbitrary) and can't double as an intent signal — e.g.
/// "was this the automatic on-open pass" is answered by the entry point, never by the preset name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPoint {
    PrOpen,
    /// New commits pushed to an already-open PR/MR (epic #566): GitHub `synchronize`, GitLab MR
    /// `update`, Bitbucket `pullrequest:updated`. A distinct entry point from `PrOpen` (not a shared
    /// mapping) because it fires N times per PR — its default preset is deliberately the cheap tier.
    PrSync,
    Mention,
    A2a,
    Mcp,
}

impl EntryPoint {
    /// The `entry_points` JSON key this entry point resolves against, and the value persisted in the
    /// `tasks.entry_point` column.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryPoint::PrOpen => "pr_open",
            EntryPoint::PrSync => "pr_sync",
            EntryPoint::Mention => "mention",
            EntryPoint::A2a => "a2a",
            EntryPoint::Mcp => "mcp",
        }
    }

    /// The preset an entry point resolves to when the repo declares no override — reproduces today's
    /// ADR-0062 fast/deep split exactly, so a repo that configures nothing sees no behavior change.
    /// `pub(crate)` (not just used internally): the A2A role holds no forge credentials (see
    /// `a2a/handler.rs`'s trust-boundary doc comment) and so can never fetch repo config to resolve a
    /// preset — it uses this default directly rather than going through [`resolve_preset`].
    pub(crate) fn platform_default_preset(self) -> &'static str {
        match self {
            // `PrSync` fires once per push — same cheap tier as the initial open, deliberately not
            // `deep`: at N pushes per PR, `deep` would multiply spend by N for no proportional benefit
            // (the PR-wide dedup landed in #570 already prevents most of the re-review noise `deep`'s
            // thoroughness would otherwise exist to catch).
            EntryPoint::PrOpen | EntryPoint::PrSync => "fast",
            EntryPoint::Mention | EntryPoint::A2a | EntryPoint::Mcp => "deep",
        }
    }
}

/// The filename ADR-0030 names for repo review config.
const CONFIG_FILENAME: &str = ".lightbridge-code-review.jsonc";

/// Size cap on the fetched file — this is a webhook-path read that must stay cheap; the schema this
/// resolver cares about is tiny.
const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// The subset of `.lightbridge-code-review.jsonc` (ADR-0030) preset resolution needs. NOT the full
/// schema — `conventions`/`architecture`/`focus`/`ignore`/`instructions`/`severity` are agent-runner-
/// only prompt/diff-filtering concerns, resolved later from the actual clone
/// (`agent-runner::review::repo_config::RepoReviewConfig`). Deliberately permissive (no
/// `deny_unknown_fields`): this is an intentional partial read of a schema this crate doesn't own
/// end-to-end, so an unrelated field the full schema defines must not fail this read.
/// `pub(crate)` (not module-private): story #500's admin read endpoint
/// (`services/control-plane/src/http/admin.rs::get_preset`) reuses this exact partial-read shape to
/// show a repo's currently-configured preset, rather than re-declaring the same permissive struct.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct RepoPresetConfig {
    pub(crate) preset: Option<String>,
    pub(crate) entry_points: HashMap<String, String>,
    /// Per-repo review-behaviour settings — the FILE layer of [`crate::settings`]'s three-layer
    /// resolution. Nested under one key deliberately: the agent-runner's `RepoReviewConfig` is
    /// `deny_unknown_fields`, so every new top-level key must be mirrored there or the runner rejects
    /// the whole file. One nested object means one mirrored key, not six, and a later
    /// control-plane-only setting needs no further runner change.
    pub(crate) triggers: Option<TriggersConfig>,
}

/// The `triggers` block of `.lightbridge-code-review.jsonc`. Every field is optional — an absent field
/// means "not configured at this layer", which is what lets [`crate::settings::merge_settings`] fall
/// through to the built-in default.
///
/// `push_strategy` / `dedup_scope` are `Option<String>`, NOT typed enums, on purpose: with a typed
/// enum a single typo would fail the whole `RepoPresetConfig` parse and silently cost the repo its
/// *preset* too. They are parsed (and warned about) during merge instead.
///
/// Deliberately NOT `deny_unknown_fields`, so a key added here later can never brick a repo whose
/// runner is still on an older build.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct TriggersConfig {
    pub(crate) check_runs: Option<bool>,
    pub(crate) review_on_open: Option<bool>,
    pub(crate) review_on_push: Option<bool>,
    pub(crate) push_strategy: Option<String>,
    pub(crate) push_debounce_seconds: Option<i32>,
    pub(crate) dedup_scope: Option<String>,
}

/// Map an already-fetched config to the preset for `entry`. Pure — split out so
/// [`crate::settings::resolve_preset_and_settings`] can derive the preset from the SAME fetch it uses
/// for the settings, instead of paying a second `get_repo_file` per webhook.
pub(crate) fn preset_from_config(config: Option<&RepoPresetConfig>, entry: EntryPoint) -> String {
    resolve_from_config(config, entry)
}

fn resolve_from_config(config: Option<&RepoPresetConfig>, entry: EntryPoint) -> String {
    let Some(config) = config else {
        return entry.platform_default_preset().to_string();
    };
    if let Some(preset) = config.entry_points.get(entry.as_str()) {
        return preset.clone();
    }
    config
        .preset
        .clone()
        .unwrap_or_else(|| entry.platform_default_preset().to_string())
}

pub(crate) async fn fetch_repo_preset_config(
    platform: &dyn CodePlatform,
    repo: &RepoRef,
    ref_: &str,
) -> Option<RepoPresetConfig> {
    let text = match platform.get_repo_file(repo, ref_, CONFIG_FILENAME).await {
        Ok(Some(text)) => text,
        Ok(None) => return None, // no repo config — platform default applies
        Err(error) => {
            tracing::warn!(
                %error, repo = %repo.full_name,
                "fetching {CONFIG_FILENAME} failed; using the platform-default preset mapping"
            );
            return None;
        }
    };
    if text.len() > MAX_CONFIG_BYTES {
        tracing::warn!(
            bytes = text.len(), repo = %repo.full_name,
            "{CONFIG_FILENAME} exceeds the size cap; using the platform-default preset mapping"
        );
        return None;
    }
    match jsonc_parser::parse_to_serde_value::<Option<RepoPresetConfig>>(
        &text,
        &jsonc_parser::ParseOptions::default(),
    ) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                %error, repo = %repo.full_name,
                "{CONFIG_FILENAME} is malformed or fails schema validation; using the platform-default preset mapping"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_uses_the_platform_default_per_entry_point() {
        assert_eq!(resolve_from_config(None, EntryPoint::PrOpen), "fast");
        assert_eq!(resolve_from_config(None, EntryPoint::Mention), "deep");
        assert_eq!(resolve_from_config(None, EntryPoint::A2a), "deep");
        assert_eq!(resolve_from_config(None, EntryPoint::Mcp), "deep");
    }

    #[test]
    fn flat_preset_applies_to_every_entry_point() {
        let config = RepoPresetConfig {
            preset: Some("ultra".to_string()),
            entry_points: HashMap::new(),
            ..Default::default()
        };
        assert_eq!(
            resolve_from_config(Some(&config), EntryPoint::PrOpen),
            "ultra"
        );
        assert_eq!(
            resolve_from_config(Some(&config), EntryPoint::Mention),
            "ultra"
        );
    }

    #[test]
    fn per_entry_point_override_wins_over_the_flat_preset() {
        let config = RepoPresetConfig {
            preset: Some("ultra".to_string()),
            entry_points: HashMap::from([("pr_open".to_string(), "fast".to_string())]),
            ..Default::default()
        };
        assert_eq!(
            resolve_from_config(Some(&config), EntryPoint::PrOpen),
            "fast",
            "explicit pr_open override wins"
        );
        assert_eq!(
            resolve_from_config(Some(&config), EntryPoint::Mention),
            "ultra",
            "mention still falls back to the flat preset"
        );
    }

    #[test]
    fn a_declared_config_with_no_preset_at_all_still_falls_back_to_platform_default() {
        let config = RepoPresetConfig::default();
        assert_eq!(
            resolve_from_config(Some(&config), EntryPoint::PrOpen),
            "fast"
        );
    }

    #[test]
    fn parses_jsonc_with_comments_and_entry_points() {
        let text = r#"{
            // repo config
            "preset": "deep",
            "entry_points": { "pr_open": "fast" },
        }"#;
        let config: Option<RepoPresetConfig> =
            jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                .expect("valid jsonc");
        let config = config.expect("some");
        assert_eq!(config.preset.as_deref(), Some("deep"));
        assert_eq!(
            config.entry_points.get("pr_open").map(String::as_str),
            Some("fast")
        );
    }

    // Unrelated ADR-0030 fields (agent-runner's concern, not this crate's) must not fail this partial
    // read — no `deny_unknown_fields` here, unlike the full schema in agent-runner.
    #[test]
    fn unrelated_full_schema_fields_are_ignored_not_rejected() {
        let text = r#"{"preset":"ultra","conventions":["use tabs"],"severity":{"min":"P1"}}"#;
        let config: Option<RepoPresetConfig> =
            jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                .expect("valid jsonc — unknown fields ignored");
        assert_eq!(config.unwrap().preset.as_deref(), Some("ultra"));
    }
}
