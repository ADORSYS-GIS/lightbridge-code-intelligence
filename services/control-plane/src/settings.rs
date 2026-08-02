//! Per-repo review-behaviour settings, resolved across three layers:
//!
//! ```text
//! built-in default  →  repo config file (.lightbridge-code-review.jsonc)  →  DB override (wins)
//! ```
//!
//! The repo file stays the default source of truth — versioned, reviewable, owned by the repo's own
//! developers. A `repo_settings` row (migration 0036) is the operator's escape hatch: instant, central,
//! no repo PR needed, and it beats the file.
//!
//! Posture is copied deliberately from [`crate::model::resolve_model_override`] and
//! [`crate::preset::resolve_preset_or_default`]: [`resolve_repo_settings`] does its own I/O, tolerates
//! an absent platform (the A2A path holds no forge credentials), degrades with a `tracing::warn!` on
//! every error, and returns no `Result` — **resolving settings must never fail task creation**.
//!
//! The precedence rule itself is the pure [`merge_settings`], so the whole matrix is unit-tested
//! without a database or a forge.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::RepoSettingsRow;
use crate::integrations::platform::{CodePlatform, RepoRef};
use crate::preset::TriggersConfig;

/// How a burst of pushes to one PR is handled. Configurable per repo because, in the owner's words,
/// "projects are different".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushStrategy {
    /// Cancel the in-flight review for that PR and review the newest head.
    Supersede,
    /// Wait for a quiet period so a burst collapses into a single run.
    Debounce,
    /// One full review per push.
    Every,
}

impl PushStrategy {
    /// Parse a config string, tolerating case. `None` for an unrecognised value so the caller can warn
    /// and fall back rather than failing the whole config parse.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "supersede" => Some(Self::Supersede),
            "debounce" => Some(Self::Debounce),
            "every" => Some(Self::Every),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supersede => "supersede",
            Self::Debounce => "debounce",
            Self::Every => "every",
        }
    }
}

/// Scope of ADR-0065 finding suppression on a re-review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DedupScope {
    /// Suppress a finding already reported anywhere on this PR — survives line drift between commits.
    Pr,
    /// Suppress only within the same `head_sha` (the pre-existing, pre-settings behaviour).
    Commit,
}

impl DedupScope {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pr" => Some(Self::Pr),
            "commit" => Some(Self::Commit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Commit => "commit",
        }
    }
}

/// Which layer produced a resolved value. Carried alongside every setting so the admin endpoint can
/// explain *why* a repo behaves the way it does — in a three-layer system that is the difference
/// between debuggable and not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Default,
    File,
    Db,
}

/// A resolved value plus its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Layer,
}

impl<T> Sourced<T> {
    fn new(value: T, source: Layer) -> Self {
        Self { value, source }
    }
}

/// Every per-repo setting, resolved with provenance. One type serves both the hot path
/// (`.check_run_reporting.value`) and the admin explain endpoint, so the two cannot drift.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSettings {
    pub check_run_reporting: Sourced<bool>,
    pub review_on_pr_open: Sourced<bool>,
    pub review_on_push: Sourced<bool>,
    pub push_strategy: Sourced<PushStrategy>,
    pub push_debounce: Sourced<Duration>,
    pub dedup_scope: Sourced<DedupScope>,
}

// --- Built-in defaults (the bottom layer) ---

/// Check runs shipped on for everyone (#559); defaulting to `false` here would silently switch off a
/// feature repos already see.
const DEFAULT_CHECK_RUN_REPORTING: bool = true;
/// The automatic on-open review is the product's core behaviour.
const DEFAULT_REVIEW_ON_PR_OPEN: bool = true;
/// **Off.** Enabling re-review-on-push fleet-wide by default would silently multiply every existing
/// customer's LLM bill by their push frequency. Opt-in per repo.
const DEFAULT_REVIEW_ON_PUSH: bool = false;
const DEFAULT_PUSH_STRATEGY: PushStrategy = PushStrategy::Supersede;
const DEFAULT_PUSH_DEBOUNCE: Duration = Duration::from_secs(90);
const DEFAULT_DEDUP_SCOPE: DedupScope = DedupScope::Pr;

/// Accepted range for the debounce quiet period: below ~10s it cannot coalesce anything, above 15 min
/// the review is too stale to be worth waiting for. Out-of-range values are clamped, not rejected —
/// same fail-safe posture as every other config degradation.
const DEBOUNCE_MIN_SECS: u64 = 10;
const DEBOUNCE_MAX_SECS: u64 = 900;

/// Apply the precedence rule: DB override, else file, else built-in default — **per field**, so a repo
/// can override one setting in the file and another in the DB without either clobbering the other.
///
/// Pure — no I/O — so the entire matrix is unit-tested without a database or a forge.
pub(crate) fn merge_settings(
    file: Option<&TriggersConfig>,
    db: Option<&RepoSettingsRow>,
) -> ResolvedSettings {
    fn pick_bool(
        db: Option<bool>,
        file: Option<bool>,
        default: bool,
    ) -> Sourced<bool> {
        match (db, file) {
            (Some(v), _) => Sourced::new(v, Layer::Db),
            (None, Some(v)) => Sourced::new(v, Layer::File),
            (None, None) => Sourced::new(default, Layer::Default),
        }
    }

    /// Parse an enum-ish string from whichever layer supplied it, warning and falling through on an
    /// unrecognised value rather than failing the whole resolution.
    fn pick_enum<T: Copy>(
        db: Option<&String>,
        file: Option<&String>,
        default: T,
        parse: impl Fn(&str) -> Option<T>,
        field: &str,
    ) -> Sourced<T> {
        for (raw, layer) in [(db, Layer::Db), (file, Layer::File)] {
            let Some(raw) = raw else { continue };
            match parse(raw) {
                Some(v) => return Sourced::new(v, layer),
                None => tracing::warn!(
                    field,
                    value = %raw,
                    layer = ?layer,
                    "unrecognised repo setting value; falling through to the next layer"
                ),
            }
        }
        Sourced::new(default, Layer::Default)
    }

    let db_debounce = db.and_then(|d| d.push_debounce_seconds);
    let file_debounce = file.and_then(|f| f.push_debounce_seconds);
    let push_debounce = match (db_debounce, file_debounce) {
        (Some(secs), _) => Sourced::new(clamp_debounce(secs, Layer::Db), Layer::Db),
        (None, Some(secs)) => Sourced::new(clamp_debounce(secs, Layer::File), Layer::File),
        (None, None) => Sourced::new(DEFAULT_PUSH_DEBOUNCE, Layer::Default),
    };

    ResolvedSettings {
        check_run_reporting: pick_bool(
            db.and_then(|d| d.check_run_reporting),
            file.and_then(|f| f.check_runs),
            DEFAULT_CHECK_RUN_REPORTING,
        ),
        review_on_pr_open: pick_bool(
            db.and_then(|d| d.review_on_pr_open),
            file.and_then(|f| f.review_on_open),
            DEFAULT_REVIEW_ON_PR_OPEN,
        ),
        review_on_push: pick_bool(
            db.and_then(|d| d.review_on_push),
            file.and_then(|f| f.review_on_push),
            DEFAULT_REVIEW_ON_PUSH,
        ),
        push_strategy: pick_enum(
            db.and_then(|d| d.push_strategy.as_ref()),
            file.and_then(|f| f.push_strategy.as_ref()),
            DEFAULT_PUSH_STRATEGY,
            PushStrategy::parse,
            "push_strategy",
        ),
        push_debounce,
        dedup_scope: pick_enum(
            db.and_then(|d| d.dedup_scope.as_ref()),
            file.and_then(|f| f.dedup_scope.as_ref()),
            DEFAULT_DEDUP_SCOPE,
            DedupScope::parse,
            "dedup_scope",
        ),
    }
}

/// Clamp a configured quiet period into the accepted range, warning when it had to be adjusted.
fn clamp_debounce(secs: i32, layer: Layer) -> Duration {
    let raw = secs.max(0) as u64;
    let clamped = raw.clamp(DEBOUNCE_MIN_SECS, DEBOUNCE_MAX_SECS);
    if clamped != raw {
        tracing::warn!(
            configured = raw,
            clamped,
            layer = ?layer,
            "push_debounce_seconds out of range; clamped"
        );
    }
    Duration::from_secs(clamped)
}

/// Resolve a repo's settings across all three layers.
///
/// Does its own I/O and swallows every error (with a warning) so a DB blip or an unreachable forge
/// degrades to the next layer instead of failing task creation. `platform: None` skips the file layer
/// entirely — the A2A path holds no forge credentials, exactly as
/// [`crate::preset::resolve_preset_or_default`] already handles.
pub async fn resolve_repo_settings(
    pool: &PgPool,
    platform: Option<&dyn CodePlatform>,
    repo: &RepoRef,
    ref_: &str,
    repository_id: i64,
) -> ResolvedSettings {
    let file = match platform {
        Some(platform) => crate::preset::fetch_repo_preset_config(platform, repo, ref_)
            .await
            .and_then(|c| c.triggers),
        None => None,
    };
    let db = match crate::db::get_repo_settings(pool, repository_id).await {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(
                %error, repository_id,
                "reading repo settings failed (non-fatal); falling back to file/default"
            );
            None
        }
    };
    merge_settings(file.as_ref(), db.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_all(v: bool) -> TriggersConfig {
        TriggersConfig {
            check_runs: Some(v),
            review_on_open: Some(v),
            review_on_push: Some(v),
            push_strategy: Some("every".to_string()),
            push_debounce_seconds: Some(120),
            dedup_scope: Some("commit".to_string()),
        }
    }

    fn db_all(v: bool) -> RepoSettingsRow {
        RepoSettingsRow {
            check_run_reporting: Some(v),
            review_on_pr_open: Some(v),
            review_on_push: Some(v),
            push_strategy: Some("debounce".to_string()),
            push_debounce_seconds: Some(300),
            dedup_scope: Some("pr".to_string()),
        }
    }

    // The bottom layer: nothing configured anywhere.
    #[test]
    fn defaults_apply_when_no_layer_configures_anything() {
        let s = merge_settings(None, None);
        assert!(s.check_run_reporting.value, "check runs shipped on (#559)");
        assert!(s.review_on_pr_open.value);
        assert!(
            !s.review_on_push.value,
            "review-on-push must default OFF — on would multiply every customer's bill"
        );
        assert_eq!(s.push_strategy.value, PushStrategy::Supersede);
        assert_eq!(s.push_debounce.value, Duration::from_secs(90));
        assert_eq!(s.dedup_scope.value, DedupScope::Pr);
        for source in [
            s.check_run_reporting.source,
            s.review_on_pr_open.source,
            s.review_on_push.source,
            s.push_strategy.source,
            s.push_debounce.source,
            s.dedup_scope.source,
        ] {
            assert_eq!(source, Layer::Default);
        }
    }

    #[test]
    fn file_beats_default() {
        let s = merge_settings(Some(&file_all(false)), None);
        assert!(!s.check_run_reporting.value);
        assert_eq!(s.check_run_reporting.source, Layer::File);
        assert_eq!(s.push_strategy.value, PushStrategy::Every);
        assert_eq!(s.push_debounce.value, Duration::from_secs(120));
        assert_eq!(s.dedup_scope.value, DedupScope::Commit);
    }

    // The whole point of the DB layer: the operator wins over the repo's own file.
    #[test]
    fn db_beats_file() {
        let s = merge_settings(Some(&file_all(false)), Some(&db_all(true)));
        assert!(s.check_run_reporting.value, "db override wins over file");
        assert_eq!(s.check_run_reporting.source, Layer::Db);
        assert!(s.review_on_pr_open.value);
        assert!(s.review_on_push.value);
        assert_eq!(s.push_strategy.value, PushStrategy::Debounce);
        assert_eq!(s.push_debounce.value, Duration::from_secs(300));
        assert_eq!(s.dedup_scope.value, DedupScope::Pr);
    }

    // Precedence is PER FIELD — a repo can set one setting in the file and another in the DB.
    #[test]
    fn precedence_is_per_field_not_whole_row() {
        let db = RepoSettingsRow {
            check_run_reporting: Some(false),
            ..Default::default()
        };
        let file = TriggersConfig {
            review_on_push: Some(true),
            ..Default::default()
        };
        let s = merge_settings(Some(&file), Some(&db));
        assert!(!s.check_run_reporting.value);
        assert_eq!(s.check_run_reporting.source, Layer::Db);
        assert!(s.review_on_push.value, "file value survives a partial db row");
        assert_eq!(s.review_on_push.source, Layer::File);
        assert_eq!(
            s.review_on_pr_open.source,
            Layer::Default,
            "a field set in neither layer still falls to the default"
        );
    }

    // A typo in an enum-ish value must not take the whole repo's settings down with it — it falls
    // through to the next layer, which is why these are `Option<String>` in the config structs.
    #[test]
    fn an_unrecognised_enum_value_falls_through_instead_of_failing() {
        let db = RepoSettingsRow {
            push_strategy: Some("supercede".to_string()), // misspelled
            dedup_scope: Some("nonsense".to_string()),
            ..Default::default()
        };
        let file = TriggersConfig {
            push_strategy: Some("every".to_string()),
            ..Default::default()
        };
        let s = merge_settings(Some(&file), Some(&db));
        assert_eq!(
            s.push_strategy.value,
            PushStrategy::Every,
            "a bad db value falls through to the file layer"
        );
        assert_eq!(s.push_strategy.source, Layer::File);
        assert_eq!(
            s.dedup_scope.value,
            DEFAULT_DEDUP_SCOPE,
            "a bad value with no other layer falls to the default"
        );
        assert_eq!(s.dedup_scope.source, Layer::Default);
    }

    #[test]
    fn debounce_is_clamped_into_range() {
        let too_small = RepoSettingsRow {
            push_debounce_seconds: Some(1),
            ..Default::default()
        };
        assert_eq!(
            merge_settings(None, Some(&too_small)).push_debounce.value,
            Duration::from_secs(DEBOUNCE_MIN_SECS)
        );
        let too_big = RepoSettingsRow {
            push_debounce_seconds: Some(99_999),
            ..Default::default()
        };
        assert_eq!(
            merge_settings(None, Some(&too_big)).push_debounce.value,
            Duration::from_secs(DEBOUNCE_MAX_SECS)
        );
        let negative = RepoSettingsRow {
            push_debounce_seconds: Some(-5),
            ..Default::default()
        };
        assert_eq!(
            merge_settings(None, Some(&negative)).push_debounce.value,
            Duration::from_secs(DEBOUNCE_MIN_SECS),
            "a negative value must not wrap into a huge delay"
        );
    }

    #[test]
    fn strategy_and_scope_parse_case_insensitively_and_round_trip() {
        assert_eq!(PushStrategy::parse("SuperSede"), Some(PushStrategy::Supersede));
        assert_eq!(PushStrategy::parse(" every "), Some(PushStrategy::Every));
        assert_eq!(PushStrategy::parse("nope"), None);
        assert_eq!(DedupScope::parse("PR"), Some(DedupScope::Pr));
        assert_eq!(DedupScope::parse("nope"), None);
        for s in [
            PushStrategy::Supersede,
            PushStrategy::Debounce,
            PushStrategy::Every,
        ] {
            assert_eq!(PushStrategy::parse(s.as_str()), Some(s));
        }
        for s in [DedupScope::Pr, DedupScope::Commit] {
            assert_eq!(DedupScope::parse(s.as_str()), Some(s));
        }
    }
}
