//! Coverage gate (ADR-0062-adjacent): bounces a premature `finish` until every changed file was either
//! opened or commented on, up to `max_bounces`, then — on a real finish/exhaustion — amends the model's
//! summary with an explicit disclosure of whatever still wasn't covered, so a truncated run never reads
//! as a clean pass.
//!
//! Two accounting fixes (#306), from a production run (`ADORSYS-GIS/webank-mobile#145`) whose disclosure
//! ("examined 26 of 40 changed files") overstated the real gap on both ends:
//!
//! - **Denominator**: `changed_files` used to be the raw diff file list — lockfiles, generated l10n
//!   output, tests, and config/docs all counted as "must examine," spending bounce budget nudging the
//!   model toward a 3692-line generated file instead of the two hand-written screens that actually went
//!   unreviewed. [`crate::path_signal::classify_path`] (shared with the diff-prompt packer) now filters
//!   the denominator down to real source; everything else is tracked separately and surfaced as a summary
//!   note rather than silently dropped.
//! - **Numerator**: engagement used to require a standalone `read_file` call or an `add_review_comment`
//!   whose `file` argument named the path. A finding can legitimately rest on a *different* changed
//!   file's diff hunk it cited as evidence (e.g. `pending_p2p_repository.dart:30`) without a separate
//!   `read_file` — that file was flagged "never engaged" despite being demonstrably used. Any changed
//!   source file cited by path-and-line-number in a finding's `evidence`/`body`/`suggestion` now counts
//!   as engaged too.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState, winddown_turn};
use lci_agent_types::ToolOutcome;

use super::{arg_field, normalize_repo_path};
use crate::path_signal::{FileSignal, classify_path};
use crate::tools::{ADD_REVIEW_COMMENT, FINISH, READ_FILE};

const COVERAGE_MAX_LISTED: usize = 15;

fn coverage_file_list(uncovered: &[&str]) -> String {
    let listed = uncovered
        .iter()
        .take(COVERAGE_MAX_LISTED)
        .map(|file| format!("- {file}"))
        .collect::<Vec<_>>()
        .join("\n");
    let more = if uncovered.len() > COVERAGE_MAX_LISTED {
        format!("\n- … and {} more", uncovered.len() - COVERAGE_MAX_LISTED)
    } else {
        String::new()
    };
    format!("{listed}{more}")
}

fn coverage_nudge(uncovered: &[&str], stalled: bool) -> String {
    let listed = coverage_file_list(uncovered);
    if stalled {
        format!(
            "You called `finish` again without opening ANY of the files you were just asked to review. A summary claiming these files were reviewed would be false — do not write one. These changed files are still unexamined:\n{listed}\n\nOpen them with read_file (or record a finding on them with add_review_comment) before you finish. If a file is genuinely not reviewable in depth (a lockfile, a generated artifact), leave it unopened but name it as NOT reviewed in your final summary. Only claim work you actually did."
        )
    } else {
        format!(
            "Before you finish: these changed files don't yet have a finding and you haven't opened them:\n{listed}\n\nReview each one across all relevant dimensions — correctness, security, quality, style, performance — not only the first issue you found. Open each with read_file, record anything worth raising with add_review_comment, then call `finish`. If a file is genuinely not reviewable in depth (a lockfile, a generated artifact), you may leave it unopened — but then name it as NOT reviewed in your final summary; never claim coverage you did not do."
        )
    }
}

fn coverage_disclosure(engaged: usize, changed: usize, uncovered: &[&str]) -> String {
    format!(
        "> ⚠️ **Coverage note (automated):** this run examined {engaged} of {changed} changed files. Never opened or commented on:\n{}",
        coverage_file_list(uncovered)
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Summarize the changed files excluded from the coverage denominator (#306) — not silently invisible,
/// per a concise count-by-bucket note appended after the main disclosure.
fn low_signal_summary(low_signal: &[(String, FileSignal)]) -> Option<String> {
    if low_signal.is_empty() {
        return None;
    }
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for (_, signal) in low_signal {
        match counts
            .iter_mut()
            .find(|(label, _)| *label == signal.label())
        {
            Some((_, n)) => *n += 1,
            None => counts.push((signal.label(), 1)),
        }
    }
    counts.sort_by_key(|(label, _)| *label);
    let parts = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "> ℹ️ {} additional changed file(s) carry low review signal and aren't counted above: {parts}.",
        low_signal.len()
    ))
}

/// Whether `text` cites `path` (or its basename) with a line number — e.g. `pending_p2p_repository.dart:30`
/// in a finding's `evidence` — the signal that the model used a changed file's diff hunk without a
/// separate `read_file` call (#306).
///
/// A match only counts as a citation when it's a *complete* path/basename token: the character
/// immediately before the match must not be part of a longer path or identifier (alphanumeric, `_`,
/// `-`, `.`, or `/`), and the match must be followed by `:<digit>`. Both halves of that boundary check
/// close two real false positives PR review caught on the naive substring version of this function:
/// - Without the leading-boundary check, a citation of `remain.rs:10` would credit `main.rs`
///   (`"main.rs:"` is a literal suffix of `"remain.rs:"`).
/// - Without also rejecting a `/` immediately before a match, a full-path citation like
///   `services/a/mod.rs:42` would credit every *other* changed file also named `mod.rs`
///   (`services/b/mod.rs`) — its basename appears as a boundary-passing substring nested inside the
///   cited file's own directory prefix.
fn cites_path_with_line(text: &str, path: &str) -> bool {
    let is_citation_at = |needle: &str, idx: usize| {
        let boundary_ok = match text[..idx].chars().next_back() {
            None => true,
            Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')),
        };
        let mut after = text[idx + needle.len()..].chars();
        boundary_ok && after.next() == Some(':') && after.next().is_some_and(|c| c.is_ascii_digit())
    };
    if text
        .match_indices(path)
        .any(|(idx, _)| is_citation_at(path, idx))
    {
        return true;
    }
    let basename = path.rsplit('/').next().unwrap_or(path);
    basename != path
        && text
            .match_indices(basename)
            .any(|(idx, _)| is_citation_at(basename, idx))
}

#[derive(Clone, Default)]
pub struct CoverageState(Arc<Mutex<Option<String>>>);

impl CoverageState {
    #[must_use]
    pub fn amended_summary(&self) -> Option<String> {
        self.0.lock().expect("coverage state mutex").clone()
    }
}

pub struct CoverageGate {
    /// The gated denominator: changed files classified as [`FileSignal::Source`] only (#306).
    changed: BTreeSet<String>,
    /// Changed files excluded from `changed` (generated/test/config/lockfile), kept only to summarize in
    /// the disclosure so they're reported rather than silently invisible.
    low_signal: Vec<(String, FileSignal)>,
    engaged: BTreeSet<String>,
    max_bounces: usize,
    bounces: usize,
    engaged_at_last_bounce: usize,
    winddown: usize,
    fast: bool,
    summary: Option<String>,
    state: CoverageState,
}

impl CoverageGate {
    #[must_use]
    pub fn new(
        changed_files: impl IntoIterator<Item = impl Into<String>>,
        max_bounces: usize,
        max_turns: usize,
        fast: bool,
    ) -> (Self, CoverageState) {
        let state = CoverageState::default();
        let mut changed = BTreeSet::new();
        let mut low_signal = Vec::new();
        for file in changed_files {
            let path = normalize_repo_path(&file.into());
            match classify_path(&path) {
                FileSignal::Source => {
                    changed.insert(path);
                }
                signal => low_signal.push((path, signal)),
            }
        }
        (
            Self {
                changed,
                low_signal,
                engaged: BTreeSet::new(),
                max_bounces,
                bounces: 0,
                engaged_at_last_bounce: 0,
                winddown: winddown_turn(max_turns),
                fast,
                summary: None,
                state: state.clone(),
            },
            state,
        )
    }

    fn update(&mut self, outcome: &TurnOutcome) {
        for result in &outcome.results {
            match result.call.function.name.as_str() {
                READ_FILE => {
                    if let Some(path) = arg_field(&result.call.function.arguments, "path") {
                        self.engaged.insert(normalize_repo_path(&path));
                    }
                }
                ADD_REVIEW_COMMENT => {
                    let args = &result.call.function.arguments;
                    if let Some(path) = arg_field(args, "file") {
                        self.engaged.insert(normalize_repo_path(&path));
                    }
                    // Credit any other gated file this finding cites by path-and-line-number in its
                    // free-text fields — the agent used its diff hunk without a standalone `read_file`
                    // call (#306, e.g. an `evidence` of "pending_p2p_repository.dart:30").
                    let cited_text = [
                        arg_field(args, "evidence"),
                        arg_field(args, "body"),
                        arg_field(args, "suggestion"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n");
                    if !cited_text.is_empty() {
                        let newly_cited: Vec<String> = self
                            .changed
                            .iter()
                            .filter(|path| !self.engaged.contains(*path))
                            .filter(|path| cites_path_with_line(&cited_text, path))
                            .cloned()
                            .collect();
                        self.engaged.extend(newly_cited);
                    }
                }
                FINISH if matches!(result.outcome, ToolOutcome::Finish) => {
                    self.summary = arg_field(&result.call.function.arguments, "summary");
                }
                _ => {}
            }
        }
    }

    fn uncovered(&self) -> Vec<&str> {
        self.changed
            .difference(&self.engaged)
            .map(String::as_str)
            .collect()
    }

    fn disclose(&self) -> bool {
        let Some(summary) = self.summary.as_deref() else {
            return false;
        };
        let uncovered = self.uncovered();
        let low_signal_note = low_signal_summary(&self.low_signal);
        // Nothing to disclose: every gated file was engaged and no low-signal files were even excluded.
        if uncovered.is_empty() && low_signal_note.is_none() {
            return false;
        }
        let mut amended = summary.to_string();
        if !uncovered.is_empty() {
            amended.push_str("\n\n");
            amended.push_str(&coverage_disclosure(
                self.changed.len() - uncovered.len(),
                self.changed.len(),
                &uncovered,
            ));
        }
        // Surfaced even when source coverage is complete (#306) — a source+generated PR whose source got
        // fully reviewed must still say the generated files were excluded, not go silent about them.
        if let Some(note) = low_signal_note {
            amended.push_str(if uncovered.is_empty() {
                "\n\n"
            } else {
                "\n>\n"
            });
            amended.push_str(&note);
        }
        *self.state.0.lock().expect("coverage state mutex") = Some(amended);
        true
    }
}

impl TurnPolicy for CoverageGate {
    fn name(&self) -> &'static str {
        "coverage_gate"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }

    fn after_turn_actions(
        &mut self,
        state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        self.update(outcome);
        if self.fast
            || !outcome.finish_requested
            || state.turn >= self.winddown
            || self.bounces >= self.max_bounces
        {
            return Vec::new();
        }
        let uncovered: Vec<String> = self.uncovered().into_iter().map(str::to_string).collect();
        if uncovered.is_empty() {
            return Vec::new();
        }
        let stalled = self.bounces > 0 && self.engaged.len() == self.engaged_at_last_bounce;
        self.bounces += 1;
        self.engaged_at_last_bounce = self.engaged.len();
        let uncovered_refs: Vec<&str> = uncovered.iter().map(String::as_str).collect();
        vec![
            PolicyAction::Record {
                name: Some("coverage_bounce"),
                detail: serde_json::json!({
                    "bounce": self.bounces,
                    "uncovered": uncovered,
                    "stalled": stalled,
                }),
            },
            PolicyAction::RejectFinish(Nudge(coverage_nudge(&uncovered_refs, stalled))),
        ]
    }

    fn finish_actions(
        &mut self,
        _state: &TurnState<'_>,
        _outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        if !self.fast && self.disclose() {
            vec![PolicyAction::Record {
                name: Some("coverage_disclosure"),
                detail: serde_json::json!({}),
            }]
        } else {
            Vec::new()
        }
    }

    fn exhausted_actions(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        if !self.fast && self.disclose() {
            vec![PolicyAction::Record {
                name: Some("coverage_disclosure"),
                detail: serde_json::json!({}),
            }]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

    #[test]
    fn coverage_requires_real_engagement_before_accepting_finish() {
        let (mut gate, coverage) = CoverageGate::new(["./a.rs"], 1, 5, false);
        let early = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                "finish",
                r#"{"summary":"early"}"#,
                ToolOutcome::Finish,
            )],
            finish_requested: true,
            abort_reason: None,
        };
        assert!(
            gate.after_turn_actions(&state(0), &early)
                .iter()
                .any(|action| matches!(action, PolicyAction::RejectFinish(_)))
        );
        let read = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                READ_FILE,
                r#"{"path":"a.rs"}"#,
                ToolOutcome::Continue("source".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(1), &read);
        assert!(gate.after_turn_actions(&state(2), &early).is_empty());
        assert!(gate.finish_actions(&state(2), &early).is_empty());
        assert!(coverage.amended_summary().is_none());
    }

    /// Regression fixture reproducing the `webank-mobile#145` file mix (#306): of 15 changed files, 11
    /// are generated l10n output / tests / config-docs that must NOT count toward the denominator, and
    /// one of the remaining 3 "never `read_file`'d" source files was actually cited (with a line number)
    /// as evidence on a finding recorded against a different file — that citation must credit it as
    /// engaged even without a standalone `read_file` call.
    #[test]
    fn coverage_excludes_low_signal_files_and_credits_diff_cited_findings() {
        let changed = [
            // Reviewable product source: 2 stay genuinely unreviewed, 1 (`pending_p2p_repository.dart`)
            // is only ever cited from another finding's evidence, 1 is read directly.
            "lib/screens/send_screen.dart",
            "lib/screens/recipient_confirm_screen.dart",
            "lib/repositories/pending_p2p_repository.dart",
            "lib/repositories/other_reviewed_repo.dart",
            // Generated (Flutter gen-l10n) + l10n data.
            "lib/l10n/app_localizations.dart",
            "lib/l10n/app_localizations_en.dart",
            "lib/l10n/app_localizations_fr.dart",
            "lib/l10n/app_en.arb",
            "lib/l10n/app_fr.arb",
            // Tests.
            "payments/service_test.go",
            "pendingp2p/service_test.go",
            "lib/models/pending_transfer_model_test.dart",
            "lib/features/referral_capture_test.dart",
            // Config/docs.
            "docker/fineract-config/base-config.yml",
            "pendingp2p/README.md",
        ];
        let (mut gate, coverage) = CoverageGate::new(changed, 1, 5, false);

        // Denominator: only the 4 real source files, not all 15.
        assert_eq!(gate.changed.len(), 4);
        assert_eq!(gate.low_signal.len(), 11);

        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                READ_FILE,
                r#"{"path":"lib/repositories/other_reviewed_repo.dart"}"#,
                ToolOutcome::Continue("source".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        });
        // A finding recorded against `other_reviewed_repo.dart` whose evidence cites
        // `pending_p2p_repository.dart:30` — no standalone `read_file` on that path.
        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":"lib/repositories/other_reviewed_repo.dart","line":12,"title":"t","priority":"P1","category":"correctness","body":"b","evidence":"caused by the missing check at lib/repositories/pending_p2p_repository.dart:30"}"#,
                ToolOutcome::Continue("recorded".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        });

        let uncovered = gate.uncovered();
        assert_eq!(
            uncovered,
            vec![
                "lib/screens/recipient_confirm_screen.dart",
                "lib/screens/send_screen.dart",
            ],
            "only the 2 genuinely-unreviewed screens remain — not the 11 low-signal files, and \
             pending_p2p_repository.dart is credited via its diff citation"
        );

        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call("finish", r#"{"summary":"done"}"#, ToolOutcome::Finish)],
            finish_requested: true,
            abort_reason: None,
        });
        assert!(gate.disclose());
        let disclosed = coverage.amended_summary().expect("disclosure recorded");
        assert!(
            disclosed.contains("examined 2 of 4 changed files"),
            "{disclosed}"
        );
        assert!(disclosed.contains("send_screen.dart"));
        assert!(disclosed.contains("recipient_confirm_screen.dart"));
        assert!(!disclosed.contains("pending_p2p_repository.dart"));
        assert!(!disclosed.contains("app_localizations"));
        assert!(
            disclosed.contains("11 additional changed file(s)"),
            "{disclosed}"
        );
    }

    /// Review-caught false positive (gemini-code-assist, high): a naive substring match on
    /// `"{basename}:"` matches `remain.rs:10` as a suffix hit for `main.rs`, wrongly crediting a file
    /// that was never actually cited.
    #[test]
    fn citation_does_not_match_a_suffix_of_a_longer_filename() {
        assert!(!cites_path_with_line(
            "the bug is in remain.rs:10, not here",
            "src/main.rs"
        ));
        // A real citation of `main.rs` (clean word boundary) still matches.
        assert!(cites_path_with_line(
            "the bug is in main.rs:10, see above",
            "src/main.rs"
        ));
    }

    /// Review-caught false positive (lightbridge-assistant + chatgpt-codex-connector, independently):
    /// two changed files that share a basename must not cross-credit each other when only ONE of them
    /// is cited by its full path — the basename fallback must not match a basename nested inside a
    /// *different* file's full-path citation.
    #[test]
    fn citation_of_one_files_full_path_does_not_credit_a_different_file_sharing_its_basename() {
        let text = "caused by the missing check at lib/repositories/store.rs:42";
        assert!(cites_path_with_line(text, "lib/repositories/store.rs"));
        assert!(!cites_path_with_line(text, "src/auth/store.rs"));
    }

    /// A bare basename citation (no directory prefix) still credits the one changed file that owns
    /// that basename — the boundary fix must not regress the original diff-cited-file scenario when
    /// there's no ambiguity.
    #[test]
    fn bare_basename_citation_without_a_directory_prefix_still_credits_the_file() {
        assert!(cites_path_with_line(
            "see pending_p2p_repository.dart:30 for the missing check",
            "lib/repositories/pending_p2p_repository.dart"
        ));
    }

    /// Review-caught gap (chatgpt-codex-connector): the low-signal summary used to be gated behind a
    /// non-empty `uncovered` set, so a PR whose source was fully reviewed but which also changed
    /// generated/test/config files got no disclosure at all — contradicting the "reported, not
    /// silently dropped" design. A fully-covered run with excluded files must still get a (shorter,
    /// non-alarming) info-only note.
    #[test]
    fn discloses_low_signal_exclusions_even_when_source_coverage_is_complete() {
        let changed = ["src/main.rs", "lib/l10n/app_localizations.dart"];
        let (mut gate, coverage) = CoverageGate::new(changed, 1, 5, false);
        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                READ_FILE,
                r#"{"path":"src/main.rs"}"#,
                ToolOutcome::Continue("source".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        });
        assert!(gate.uncovered().is_empty(), "the only source file was read");
        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call("finish", r#"{"summary":"done"}"#, ToolOutcome::Finish)],
            finish_requested: true,
            abort_reason: None,
        });
        assert!(
            gate.disclose(),
            "a low-signal exclusion note must still be posted"
        );
        let disclosed = coverage.amended_summary().expect("disclosure recorded");
        assert!(
            !disclosed.contains("Never opened or commented on"),
            "{disclosed}"
        );
        assert!(
            disclosed.contains("1 additional changed file(s)"),
            "{disclosed}"
        );
        assert!(disclosed.contains("generated"), "{disclosed}");
    }

    /// The fully-clean case (no uncovered source, no low-signal exclusions at all) still posts nothing
    /// — matches [`coverage_requires_real_engagement_before_accepting_finish`]'s existing expectation.
    #[test]
    fn discloses_nothing_when_fully_covered_with_no_low_signal_files() {
        let (mut gate, coverage) = CoverageGate::new(["src/main.rs"], 1, 5, false);
        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                READ_FILE,
                r#"{"path":"src/main.rs"}"#,
                ToolOutcome::Continue("source".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        });
        gate.update(&TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call("finish", r#"{"summary":"done"}"#, ToolOutcome::Finish)],
            finish_requested: true,
            abort_reason: None,
        });
        assert!(!gate.disclose());
        assert!(coverage.amended_summary().is_none());
    }
}
