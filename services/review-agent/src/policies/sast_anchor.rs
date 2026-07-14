//! SAST anchor gate (ADR-0061 Phase 2 hardening, issue #305): the model must not triage an opengrep
//! finding — confirm or refute — without anchoring its verdict to the EXACT `file:line` opengrep
//! flagged. Production incident (ADORSYS-GIS/webank-mobile#145): opengrep deterministically flagged
//! `.env.fullstack:216` (a real MTN Mobile Money API key), but the model posted a "false positive" note
//! at `.env.fullstack:60` — an unrelated dev-DB-password line it never verified against the actual
//! finding — so the one secret that warranted a human look was never evaluated. The gate catches a
//! "false positive"-style verdict (or one citing an opengrep rule id) recorded on a file opengrep
//! flagged but at a line opengrep did NOT flag, and bounces once with the real coordinate — mirroring
//! [`super::RefuteGate`]'s one-shot re-verify pattern for the same "confidently wrong costs more than a
//! miss" reason.

use std::collections::HashMap;

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState};
use lci_agent_types::ToolOutcome;
use serde_json::Value;

use super::normalize_repo_path;
use crate::tools::{ADD_REVIEW_COMMENT, RETRACT_FINDING};

/// A string field from an already-parsed tool-call `arguments` object. `update` parses each call's JSON
/// once and reuses the [`Value`] for every field it needs, rather than re-parsing the same raw string
/// per field (`file`, `line`, `title`, `body`, `evidence`).
fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

/// One opengrep-flagged coordinate the gate anchors verdicts to. Deliberately minimal (no message/
/// priority) — the runner maps its own `sast::SastFinding` list onto these at the call boundary, so
/// `review-agent` stays independent of the runner's SAST module.
#[derive(Debug, Clone)]
pub struct SastLead {
    pub file: String,
    pub line: u32,
    pub rule_id: String,
}

/// The `(line, rule_id)` coordinates opengrep flagged in one file.
type Coords = Vec<(u32, String)>;

/// Phrases that mark a comment as a triage VERDICT about a SAST lead rather than an unrelated finding
/// that merely happens to land in the same file. Deliberately narrow (not a bare "opengrep" mention,
/// which a legitimate deepen-the-lead comment on a different — e.g. taint-source — line may use
/// honestly): a "false positive" call only makes sense anchored to the rule's own flagged coordinate, and
/// citing the rule id is an explicit claim about that specific finding.
fn is_sast_verdict(text: &str, rule_ids: &[&str]) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("false positive")
        || lower.contains("false-positive")
        || rule_ids
            .iter()
            .any(|id| lower.contains(&id.to_ascii_lowercase()))
}

fn real_coords(file: &str, coords: &[(u32, String)]) -> String {
    coords
        .iter()
        .map(|(line, rule)| format!("{file}:{line} (`{rule}`)"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct SastAnchorGate {
    /// Normalized repo-relative file → the opengrep coordinates flagged in it.
    leads: HashMap<String, Coords>,
    /// Outstanding misanchored verdicts, keyed by the (wrong) `(file, line)` they were recorded at.
    outstanding: HashMap<(String, i64), Coords>,
    fast: bool,
    bounced: bool,
}

impl SastAnchorGate {
    #[must_use]
    pub fn new(leads: impl IntoIterator<Item = SastLead>, fast: bool) -> Self {
        let mut by_file: HashMap<String, Coords> = HashMap::new();
        for lead in leads {
            by_file
                .entry(normalize_repo_path(&lead.file))
                .or_default()
                .push((lead.line, lead.rule_id));
        }
        Self {
            leads: by_file,
            outstanding: HashMap::new(),
            fast,
            bounced: false,
        }
    }

    fn update(&mut self, outcome: &TurnOutcome) {
        for result in &outcome.results {
            let args = &result.call.function.arguments;
            match result.call.function.name.as_str() {
                ADD_REVIEW_COMMENT => {
                    if !matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("recorded finding"))
                    {
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(args) else {
                        continue;
                    };
                    let Some(file) = str_field(&parsed, "file").map(|f| normalize_repo_path(&f))
                    else {
                        continue;
                    };
                    let Some(line) = parsed.get("line").and_then(Value::as_i64) else {
                        continue;
                    };
                    let Some(coords) = self.leads.get(&file) else {
                        continue;
                    };
                    let key = (file, line);
                    if coords.iter().any(|(l, _)| i64::from(*l) == line) {
                        // Anchored to the real flagged line — never a violation, whatever it says.
                        self.outstanding.remove(&key);
                        continue;
                    }
                    // `evidence` is scanned too: it's exactly where the digest instructs the model to
                    // quote the line it read (record.rs folds it into the rendered body), so a compliant
                    // model's "false positive" verdict can land there instead of `body` (#406 review).
                    let text = format!(
                        "{} {} {}",
                        str_field(&parsed, "title").unwrap_or_default(),
                        str_field(&parsed, "body").unwrap_or_default(),
                        str_field(&parsed, "evidence").unwrap_or_default(),
                    );
                    let rule_ids: Vec<&str> = coords.iter().map(|(_, id)| id.as_str()).collect();
                    if is_sast_verdict(&text, &rule_ids) {
                        self.outstanding.insert(key, coords.clone());
                    } else {
                        // Re-recording the same (file, line) with non-verdict text clears a prior flag —
                        // `add_review_comment` refines the same coordinate (ADR-0037).
                        self.outstanding.remove(&key);
                    }
                }
                RETRACT_FINDING => {
                    if !matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("retracted finding"))
                    {
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(args) else {
                        continue;
                    };
                    let (Some(file), Some(line)) = (
                        str_field(&parsed, "file").map(|f| normalize_repo_path(&f)),
                        parsed.get("line").and_then(Value::as_i64),
                    ) else {
                        continue;
                    };
                    self.outstanding.remove(&(file, line));
                }
                _ => {}
            }
        }
    }
}

impl TurnPolicy for SastAnchorGate {
    fn name(&self) -> &'static str {
        "sast_anchor"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }

    fn after_turn_actions(
        &mut self,
        _state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        if self.fast || self.leads.is_empty() {
            return Vec::new();
        }
        self.update(outcome);
        if self.bounced || self.outstanding.is_empty() || !outcome.finish_requested {
            return Vec::new();
        }
        self.bounced = true;
        // Deterministic order for a stable nudge across runs on the same violations.
        let mut violations: Vec<(&(String, i64), &Coords)> = self.outstanding.iter().collect();
        violations.sort_by(|a, b| a.0.cmp(b.0));
        let detail: Vec<_> = violations
            .iter()
            .map(|((file, line), coords)| {
                serde_json::json!({"wrong": format!("{file}:{line}"), "actual": real_coords(file, coords)})
            })
            .collect();
        let lines: Vec<String> = violations
            .iter()
            .map(|((file, line), coords)| {
                format!("- you wrote a SAST verdict at {file}:{line}, but opengrep flagged {} — not that line.", real_coords(file, coords))
            })
            .collect();
        vec![
            PolicyAction::Record {
                name: Some("sast_anchor_bounce"),
                detail: serde_json::json!({"violations": detail}),
            },
            PolicyAction::RejectFinish(Nudge(format!(
                "Before you finish: a recorded finding claims a SAST verdict (\"false positive\" or an \
                 opengrep rule id) on a line opengrep never flagged — that's an unverified guess, not a \
                 triage:\n{}\n\nFor each: call `read_file` on the EXACT flagged line named above, quote \
                 what it actually contains, and reason from that — never a nearby or similar-looking \
                 line. Then `retract_finding` the wrong-anchored one; only record a new finding at the \
                 real coordinate if you can prove your verdict against its real content.",
                lines.join("\n")
            ))),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

    fn leads() -> Vec<SastLead> {
        vec![SastLead {
            file: ".env.fullstack".into(),
            line: 216,
            rule_id: "generic.secrets.security.detected-generic-api-key".into(),
        }]
    }

    fn finish() -> TurnOutcome {
        TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![],
            finish_requested: true,
            abort_reason: None,
        }
    }

    // Reproduces the production incident: opengrep flagged .env.fullstack:216 (an MTN API key); the
    // model instead posted a "false positive" verdict at :60 (an unrelated DB password) and never
    // touched :216. The gate must reject the finish and name the real coordinate.
    #[test]
    fn bounces_a_false_positive_claim_anchored_to_the_wrong_line() {
        let mut gate = SastAnchorGate::new(leads(), false);
        let misanchored = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":60,"title":"False positive","body":"DATABASE_URL is just a dev password, not a real secret.","priority":"P2","category":"security"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:60".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        assert!(gate.after_turn_actions(&state(0), &misanchored).is_empty());
        let actions = gate.after_turn_actions(&state(1), &finish());
        let nudge = actions.iter().find_map(|action| match action {
            PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
            _ => None,
        });
        let nudge = nudge.expect("misanchored false-positive verdict must reject finish");
        assert!(
            nudge.contains(".env.fullstack:216"),
            "nudge names the real flagged line: {nudge}"
        );
        assert!(
            nudge.contains(".env.fullstack:60"),
            "nudge names the wrong line the model actually used: {nudge}"
        );

        // One-shot, like RefuteGate: a second finish attempt is not bounced again even though the
        // violation is technically still outstanding (cost control over a guarantee).
        assert!(gate.after_turn_actions(&state(2), &finish()).is_empty());
    }

    // A model following the digest's own instruction (quote the line you read into `evidence`) can put
    // the "false positive" wording there instead of `body`. `record.rs` folds `evidence` into the
    // rendered comment either way, so the gate must scan it too — not just `title`/`body` (PR #406
    // review).
    #[test]
    fn catches_a_false_positive_claim_recorded_only_in_evidence() {
        let mut gate = SastAnchorGate::new(leads(), false);
        let misanchored = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":60,"title":"Reviewed DATABASE_URL","body":"See evidence.","evidence":"Read .env.fullstack:60 — it's a dev password. False positive for the opengrep rule.","priority":"P2","category":"security"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:60".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &misanchored);
        assert!(
            gate.after_turn_actions(&state(1), &finish())
                .iter()
                .any(|action| matches!(action, PolicyAction::RejectFinish(_))),
            "a verdict hidden in `evidence` rather than `body` must still be caught"
        );
    }

    // The correctly-anchored path: the model reads the real line and records its verdict there. No
    // bounce — anchoring to the flagged coordinate is exactly what's required, whatever the verdict.
    #[test]
    fn allows_a_verdict_anchored_to_the_real_flagged_line() {
        let mut gate = SastAnchorGate::new(leads(), false);
        let anchored = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":216,"title":"False positive","body":"Read the line: it's a placeholder, not a live key.","priority":"P2","category":"security"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:216".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &anchored);
        assert!(gate.after_turn_actions(&state(1), &finish()).is_empty());
    }

    // An unrelated finding in the same file, at a different line, that never claims to be a SAST
    // verdict must not be caught — the gate targets mislabeled triage, not every comment near a lead.
    #[test]
    fn ignores_an_unrelated_finding_in_the_same_file() {
        let mut gate = SastAnchorGate::new(leads(), false);
        let unrelated = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":12,"title":"Missing trailing newline","body":"The file doesn't end with a newline.","priority":"P2","category":"style"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:12".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &unrelated);
        assert!(gate.after_turn_actions(&state(1), &finish()).is_empty());
    }

    // Retracting the misanchored finding clears the violation, so a later finish is not bounced for it.
    #[test]
    fn retracting_the_wrong_finding_clears_the_violation() {
        let mut gate = SastAnchorGate::new(leads(), false);
        let misanchored = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":60,"title":"False positive","body":"Not a real secret (rule generic.secrets.security.detected-generic-api-key).","priority":"P2","category":"security"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:60".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &misanchored);
        let retract = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                RETRACT_FINDING,
                r#"{"file":".env.fullstack","line":60}"#,
                ToolOutcome::Continue("retracted finding at .env.fullstack:60".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(1), &retract);
        assert!(gate.after_turn_actions(&state(2), &finish()).is_empty());
    }

    // FAST tier never offers `read_file`, so the "read the exact line first" requirement is moot there
    // — disabled, matching RefuteGate/CoverageGate's fast-tier opt-out.
    #[test]
    fn disabled_in_fast_tier() {
        let mut gate = SastAnchorGate::new(leads(), true);
        let misanchored = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":".env.fullstack","line":60,"title":"False positive","body":"Not a real secret.","priority":"P2","category":"security"}"#,
                ToolOutcome::Continue("recorded finding at .env.fullstack:60".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &misanchored);
        assert!(gate.after_turn_actions(&state(1), &finish()).is_empty());
    }
}
