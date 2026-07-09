//! Pure mapping helpers for the A2A surface (RFC-0006 Phase 1).
//!
//! Everything here is a pure function of its inputs — no DB, no network — so the load-bearing
//! translation logic (our task state ↔ A2A wire state; parsing a `review` skill request off a
//! message; shaping review artifacts) is unit-tested in isolation, and the SDK-serving glue in
//! [`super::handler`] stays thin.

use a2a::{Artifact, Part, Task, TaskState, TaskStatus};
use serde_json::Value;

use crate::integrations::platform::Platform;

/// Map a Lightbridge `tasks.status` string onto the A2A wire [`TaskState`] (RFC-0006 state table):
///
/// | Lightbridge | A2A |
/// |---|---|
/// | `received`, `queued`, `waiting_for_index` | `TASK_STATE_SUBMITTED` |
/// | `running`, `posting_result` | `TASK_STATE_WORKING` |
/// | `succeeded` | `TASK_STATE_COMPLETED` |
/// | `failed`, `timed_out` | `TASK_STATE_FAILED` |
/// | `cancelled` | `TASK_STATE_CANCELED` |
///
/// An unrecognised status is treated as in-progress (`WORKING`) rather than a terminal guess, so a
/// new status literal never makes a poller believe a still-running review has finished.
pub fn task_state_from_status(status: &str) -> TaskState {
    match status {
        "received" | "queued" | "waiting_for_index" => TaskState::Submitted,
        "running" | "posting_result" => TaskState::Working,
        "succeeded" => TaskState::Completed,
        "failed" | "timed_out" => TaskState::Failed,
        "cancelled" => TaskState::Canceled,
        _ => TaskState::Working,
    }
}

/// Why a `review` submission could not be parsed off the inbound message. Distinct variants so the
/// handler can answer a malformed request with a precise `INVALID_PARAMS`.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No structured `data` part on the message (Phase 1 takes structured input, not free text).
    NoDataPart,
    /// The requested skill is not `review` (the only Phase-1 skill).
    UnsupportedSkill(String),
    /// A required field was missing or malformed. Carries the field name for the error detail.
    BadField(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoDataPart => {
                write!(
                    f,
                    "message has no structured `data` part with the review request"
                )
            }
            ParseError::UnsupportedSkill(s) => {
                write!(
                    f,
                    "unsupported skill {s:?}; only `review` is available in this phase"
                )
            }
            ParseError::BadField(field) => write!(f, "missing or invalid field: {field}"),
        }
    }
}

/// The parsed input for the `review` skill. Repo identity is resolved against our DB later. Both
/// SHAs are optional *at parse time*, but the handler REQUIRES a `head_sha` before it will submit:
/// the `a2a` role holds NO forge credentials (it cannot resolve a PR head itself), and a null head
/// would fall through downstream to a review of the repo's default branch — so a submission without
/// one is rejected (RFC-0006). `base_sha` stays genuinely optional. A supplied head also lets the
/// review dedup onto a webhook-triggered run of the same head.
#[derive(Debug, PartialEq, Eq)]
pub struct ReviewInput {
    pub platform: Platform,
    pub owner: String,
    pub name: String,
    pub pr: i64,
    /// Free-text focus prompt → the task's `command_text` (defaulted when the caller omits one).
    pub prompt: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
}

/// Default command text when the caller sends no `prompt` — the runner still performs a full deep
/// review; this is just the recorded intent.
const DEFAULT_REVIEW_PROMPT: &str = "Deep review requested via A2A.";

/// Parse a `review` request out of the first structured `data` part of the message body.
///
/// Expected shape (camelCase or snake_case accepted for `headSha`/`baseSha`):
/// ```json
/// { "skill": "review", "forge": "github", "repo": "owner/name", "pr": 128,
///   "prompt": "focus on auth", "headSha": "abc123", "baseSha": "def456" }
/// ```
/// `skill` defaults to `review`; `forge` defaults to `github`. `repo` (owner/name) and `pr` are
/// required.
pub fn parse_review_request(parts: &[Part]) -> Result<ReviewInput, ParseError> {
    let data = first_data_object(parts).ok_or(ParseError::NoDataPart)?;

    // Skill: default `review`; anything else is out of scope for Phase 1.
    let skill = data
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or("review");
    if skill != "review" {
        return Err(ParseError::UnsupportedSkill(skill.to_string()));
    }

    let forge = data
        .get("forge")
        .and_then(Value::as_str)
        .unwrap_or("github");
    let platform: Platform = forge.parse().map_err(|_| ParseError::BadField("forge"))?;

    let repo = data
        .get("repo")
        .and_then(Value::as_str)
        .ok_or(ParseError::BadField("repo"))?;
    let (owner, name) = split_repo(repo).ok_or(ParseError::BadField("repo"))?;

    // `pr` may arrive as a JSON number or a numeric string; both are accepted, non-positive is not.
    let pr = data
        .get("pr")
        .and_then(json_to_i64)
        .filter(|n| *n > 0)
        .ok_or(ParseError::BadField("pr"))?;

    let prompt = data
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REVIEW_PROMPT)
        .to_string();

    Ok(ReviewInput {
        platform,
        owner: owner.to_string(),
        name: name.to_string(),
        pr,
        prompt,
        base_sha: opt_sha(data, "baseSha", "base_sha"),
        head_sha: opt_sha(data, "headSha", "head_sha"),
    })
}

/// The first `data`-part's object body, if any.
fn first_data_object(parts: &[Part]) -> Option<&serde_json::Map<String, Value>> {
    parts.iter().find_map(|part| match &part.content {
        a2a::PartContent::Data(Value::Object(map)) => Some(map),
        _ => None,
    })
}

/// Split `"owner/name"` (rejecting empty halves or extra slashes) into its two parts.
fn split_repo(repo: &str) -> Option<(&str, &str)> {
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((owner, name))
}

/// Read a SHA from either the camelCase or snake_case key, trimming and dropping blanks.
fn opt_sha(map: &serde_json::Map<String, Value>, camel: &str, snake: &str) -> Option<String> {
    map.get(camel)
        .or_else(|| map.get(snake))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Coerce a JSON number or numeric string to `i64`.
fn json_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Build the caller-scoped A2A artifacts for a COMPLETED review: a text summary part plus a
/// structured `data` part carrying the findings JSON (ADR-0032 shape). These are an ADDITIONAL
/// output to the caller — the PR review itself still posts through the existing pipeline.
pub fn review_artifacts(summary: &str, findings: &Value) -> Vec<Artifact> {
    let summary_text = if summary.trim().is_empty() {
        "Review completed with no summary.".to_string()
    } else {
        summary.to_string()
    };
    vec![Artifact {
        artifact_id: "review".to_string(),
        name: Some("review".to_string()),
        description: Some("Deep review summary and structured findings.".to_string()),
        parts: vec![
            Part::text(summary_text),
            Part::data(findings.clone()).with_media_type("application/json"),
        ],
        metadata: None,
        extensions: None,
    }]
}

/// Assemble an A2A [`Task`] view: id/context + a status carrying the mapped state, and (when
/// terminal-with-output) the artifacts. Pure — the handler feeds it the live state it derived.
pub fn build_task_view(
    a2a_task_id: &str,
    context_id: &str,
    state: TaskState,
    artifacts: Option<Vec<Artifact>>,
    metadata: Option<serde_json::Map<String, Value>>,
) -> Task {
    Task {
        id: a2a_task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts,
        history: None,
        metadata: metadata.map(|m| m.into_iter().collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data_part(v: Value) -> Part {
        Part::data(v)
    }

    #[test]
    fn status_maps_cover_every_lifecycle_state() {
        assert_eq!(task_state_from_status("received"), TaskState::Submitted);
        assert_eq!(task_state_from_status("queued"), TaskState::Submitted);
        assert_eq!(
            task_state_from_status("waiting_for_index"),
            TaskState::Submitted
        );
        assert_eq!(task_state_from_status("running"), TaskState::Working);
        assert_eq!(task_state_from_status("posting_result"), TaskState::Working);
        assert_eq!(task_state_from_status("succeeded"), TaskState::Completed);
        assert_eq!(task_state_from_status("failed"), TaskState::Failed);
        assert_eq!(task_state_from_status("timed_out"), TaskState::Failed);
        assert_eq!(task_state_from_status("cancelled"), TaskState::Canceled);
        // Terminal states are terminal; SUBMITTED/WORKING are not.
        assert!(task_state_from_status("succeeded").is_terminal());
        assert!(!task_state_from_status("queued").is_terminal());
    }

    #[test]
    fn unknown_status_is_treated_as_in_progress_not_terminal() {
        let state = task_state_from_status("some_new_status");
        assert_eq!(state, TaskState::Working);
        assert!(
            !state.is_terminal(),
            "unknown status must never read terminal"
        );
    }

    #[test]
    fn parses_a_full_review_request() {
        let parts = vec![data_part(json!({
            "skill": "review",
            "forge": "github",
            "repo": "acme/api",
            "pr": 128,
            "prompt": "  focus on auth  ",
            "headSha": "abc123",
            "baseSha": "def456"
        }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.platform, Platform::GitHub);
        assert_eq!(input.owner, "acme");
        assert_eq!(input.name, "api");
        assert_eq!(input.pr, 128);
        assert_eq!(input.prompt, "focus on auth");
        assert_eq!(input.head_sha.as_deref(), Some("abc123"));
        assert_eq!(input.base_sha.as_deref(), Some("def456"));
    }

    #[test]
    fn defaults_skill_forge_and_prompt_and_accepts_numeric_string_pr() {
        let parts = vec![data_part(json!({ "repo": "o/n", "pr": "7" }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.platform, Platform::GitHub);
        assert_eq!(input.pr, 7);
        assert_eq!(input.prompt, DEFAULT_REVIEW_PROMPT);
        assert!(input.head_sha.is_none() && input.base_sha.is_none());
    }

    #[test]
    fn accepts_snake_case_sha_keys_and_gitlab_forge() {
        let parts = vec![data_part(json!({
            "forge": "gitlab", "repo": "grp/proj", "pr": 3, "head_sha": "h", "base_sha": "b"
        }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.platform, Platform::GitLab);
        assert_eq!(input.head_sha.as_deref(), Some("h"));
        assert_eq!(input.base_sha.as_deref(), Some("b"));
    }

    #[test]
    fn rejects_missing_and_malformed_fields() {
        assert_eq!(
            parse_review_request(&[Part::text("hi")]),
            Err(ParseError::NoDataPart)
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "skill": "ask", "repo": "o/n", "pr": 1 }))]),
            Err(ParseError::UnsupportedSkill("ask".to_string()))
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "pr": 1 }))]),
            Err(ParseError::BadField("repo"))
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "repo": "no-slash", "pr": 1 }))]),
            Err(ParseError::BadField("repo"))
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "repo": "o/n" }))]),
            Err(ParseError::BadField("pr"))
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "repo": "o/n", "pr": 0 }))]),
            Err(ParseError::BadField("pr"))
        );
        assert_eq!(
            parse_review_request(&[data_part(json!({ "forge": "svn", "repo": "o/n", "pr": 1 }))]),
            Err(ParseError::BadField("forge"))
        );
    }

    #[test]
    fn review_artifacts_carry_summary_and_findings() {
        let findings = json!([{ "path": "a.rs", "severity": "P1" }]);
        let arts = review_artifacts("Looks risky", &findings);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].artifact_id, "review");
        assert_eq!(arts[0].parts.len(), 2);
        assert_eq!(arts[0].parts[0].as_text(), Some("Looks risky"));
        match &arts[0].parts[1].content {
            a2a::PartContent::Data(v) => assert_eq!(v, &findings),
            other => panic!("expected data part, got {other:?}"),
        }
    }

    #[test]
    fn empty_summary_gets_a_placeholder() {
        let arts = review_artifacts("   ", &json!([]));
        assert_eq!(
            arts[0].parts[0].as_text(),
            Some("Review completed with no summary.")
        );
    }
}
