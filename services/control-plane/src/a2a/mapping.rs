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
    /// No structured `data` part carrying the review target (ADR-0078). A `text`-only message
    /// (natural-language instruction, no `data`) lands here: the role holds no forge credentials, so
    /// it cannot resolve a target from prose. Its `Display` is the *guided* error — it names the
    /// minimum precise fields the caller must supply and points at the calling guide, rather than a
    /// bare "no data part" dead-end. (Phase 4 upgrades this exact branch to an `input-required`
    /// clarify-then-confirm loop.)
    NoTargetData,
    /// The requested skill is not `review` (the only Phase-1 skill).
    UnsupportedSkill(String),
    /// A required field was missing or malformed. Carries the field name for the error detail.
    BadField(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoTargetData => {
                write!(
                    f,
                    "no structured `data` part with the review target: an A2A review needs the \
                     precise target in a `data` part — at minimum `repo` (\"owner/name\"), `pr`, \
                     and `headSha`. A natural-language `text` part sets the review's emphasis only, \
                     never its target or scope. See docs/a2a-review-skill.md."
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

/// Parse a `review` request off the message body: the **target + scope** come only from the first
/// structured `data` part; the **instruction** (`prompt`) may instead arrive as natural-language
/// `text` parts (ADR-0078).
///
/// Expected `data` shape (camelCase or snake_case accepted for `headSha`/`baseSha`):
/// ```json
/// { "skill": "review", "forge": "github", "repo": "owner/name", "pr": 128,
///   "prompt": "focus on auth", "headSha": "abc123", "baseSha": "def456" }
/// ```
/// `skill` defaults to `review`; `forge` defaults to `github`. `repo` (owner/name) and `pr` are
/// required.
///
/// **Prompt precedence (ADR-0078):** one or more non-empty `text` parts → the prompt is those parts
/// concatenated in message order, newline-joined, then trimmed — this **wins over** `data.prompt`
/// (a caller who typed a sentence meant that sentence). No text → `data.prompt` if present (today's
/// behaviour), else the default intent. `text` never sets or overrides target/scope; a `text`-only
/// message (no `data` part) is [`ParseError::NoTargetData`] (a guided error), not a silent reject.
/// `file` parts are ignored.
pub fn parse_review_request(parts: &[Part]) -> Result<ReviewInput, ParseError> {
    let data = first_data_object(parts).ok_or(ParseError::NoTargetData)?;

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
        .map(str::trim)
        .ok_or(ParseError::BadField("repo"))?;
    let (owner, name) = split_repo(repo).ok_or(ParseError::BadField("repo"))?;

    // `pr` may arrive as a JSON integer, an integral JSON float (the real A2A wire shape — see
    // `json_to_i64`), or a numeric string; all are accepted, non-positive is not.
    let pr = data
        .get("pr")
        .and_then(json_to_i64)
        .filter(|n| *n > 0)
        .ok_or(ParseError::BadField("pr"))?;

    // Prompt precedence (ADR-0078): a natural-language `text` instruction wins over `data.prompt`;
    // then `data.prompt`; then the default. Target/scope above are untouched — text steers emphasis
    // only, never what is reviewed or how widely.
    let prompt = collect_text_instruction(parts)
        .or_else(|| {
            data.get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_REVIEW_PROMPT.to_string());

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

/// The caller's natural-language instruction: every `text` part concatenated in message order,
/// newline-joined, then trimmed (ADR-0078). `None` when the message carries no text part whose
/// trimmed concatenation is non-empty — the caller for the `data.prompt`/default fallback. This is
/// the review's *emphasis* only; it never contributes to the target or scope (those are read solely
/// from the `data` part).
fn collect_text_instruction(parts: &[Part]) -> Option<String> {
    let joined = parts
        .iter()
        .filter_map(Part::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
///
/// The float arm is load-bearing on the A2A wire, not defensive padding: A2A v1.0 carries a
/// DataPart's `data` as a `google.protobuf.Struct`, whose only numeric type is `double` (f64). So a
/// caller's `"pr": 128` is coerced to the float `128.0` by the ProtoJSON round-trip before it ever
/// reaches this parser, and `serde_json::Value::as_i64` returns `None` for a float — even an
/// integral one. We therefore accept an integral, in-range float as an `i64` (rejecting a
/// fractional value like `128.5`, which is not a valid PR number). Net: `pr` parses whether it
/// arrives as a JSON integer, an integral JSON float (the real wire case), or a numeric string.
fn json_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .or_else(|| {
            value.as_f64().and_then(|f| {
                (f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64)
                    .then_some(f as i64)
            })
        })
}

/// The context echoed back on a COMPLETED review so the caller can confirm *what was reviewed* and
/// jump straight to the posted result:
///
/// - `base_sha` / `head_sha` — the base/head SHAs the run was **submitted with** (from the
///   underlying `tasks` row).
/// - **scope** is derived from whether a base was **supplied on the request**: base present →
///   `diff` (a diff-scoped review of `merge-base(base,head)..head` was *requested*), absent →
///   `whole-tree` (no base, so the whole working tree at `head` is reviewed). **This reflects the
///   *requested* scope, not a readback of the runner's decision:** the agent-runner (`pr_diff`)
///   still falls back to a whole-tree review even with a base when `base == head`, the diff is
///   empty, or the diff computation fails. So `diff` means "diff-scoping was requested" (not a
///   guarantee the run diffed); `whole-tree` is always accurate (no base ⇒ never diff-scoped). The
///   value still answers the [RFC-0006] `baseSha`-determines-scope question the caller cares about —
///   just don't read `diff` as a hard promise.
/// - `review_url` — the permalink to the review posted on the PR ([`ReviewRow::review_url`]).
///
/// Every field is optional: when the underlying run row is unavailable, repo/pr/SHAs are unknown
/// and only what survives (e.g. the persisted `review_url`) is echoed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReviewContext {
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub review_url: Option<String>,
}

impl ReviewContext {
    /// The review scope the base SHA implies. A real run always carried a head (the handler requires
    /// one at submit), so `head_sha.is_some()` distinguishes "row present, no base → whole-tree" from
    /// "row reaped → scope unknowable (`None`)".
    fn scope(&self) -> Option<&'static str> {
        if self.base_sha.is_some() {
            Some("diff")
        } else if self.head_sha.is_some() {
            Some("whole-tree")
        } else {
            None
        }
    }

    /// The context `data` part body: nulls where a field is unknown (JSON `null`), so the shape is
    /// stable for callers regardless of whether the row was reaped.
    fn to_data(&self) -> Value {
        serde_json::json!({
            "repo": self.repo,
            "pr": self.pr,
            "baseSha": self.base_sha,
            "headSha": self.head_sha,
            "scope": self.scope(),
            "reviewUrl": self.review_url,
        })
    }
}

/// Build the caller-scoped A2A artifacts for a COMPLETED review: a text summary part, a structured
/// `data` part carrying the findings JSON (ADR-0032 shape), and a `data` context part carrying the
/// effective base/head SHAs, the derived scope, and the review permalink (see [`ReviewContext`]).
/// These are an ADDITIONAL output to the caller — the PR review itself still posts through the
/// existing pipeline.
pub fn review_artifacts(summary: &str, findings: &Value, context: &ReviewContext) -> Vec<Artifact> {
    let summary_text = if summary.trim().is_empty() {
        "Review completed with no summary.".to_string()
    } else {
        summary.to_string()
    };
    vec![Artifact {
        artifact_id: "review".to_string(),
        name: Some("review".to_string()),
        description: Some(
            "Deep review summary, structured findings, and the review context (effective \
             base/head SHAs, scope, and the posted-review permalink)."
                .to_string(),
        ),
        parts: vec![
            Part::text(summary_text),
            Part::data(findings.clone()).with_media_type("application/json"),
            Part::data(context.to_data()).with_media_type("application/json"),
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
    fn trims_surrounding_whitespace_on_repo() {
        // A padded `repo` (e.g. copy-pasted with spaces) must still resolve to the same owner/name,
        // not `" acme"` / `"api "` which would miss the repository lookup.
        let parts = vec![data_part(json!({ "repo": "  acme/api  ", "pr": 1 }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.owner, "acme");
        assert_eq!(input.name, "api");
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
    fn accepts_pr_as_an_integral_float_the_real_a2a_wire_shape() {
        // Regression guard for the live `-32602 missing or invalid field: pr` (follow-up to #308).
        // On the wire an A2A DataPart's `data` is a `google.protobuf.Struct`, whose only numeric
        // type is `double`, so a caller's `"pr": 128` reaches this parser as the float `128.0` —
        // NOT a JSON integer. `json!({"pr": 128.0})` reproduces exactly that post-deserialization
        // `Value::Number(f64)`; the earlier tests used `json!({"pr": 128})` (a serde integer) and so
        // never exercised the coercion the real caller hits.
        let parts = vec![data_part(json!({ "repo": "o/n", "pr": 128.0 }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.pr, 128);
    }

    #[test]
    fn json_to_i64_accepts_int_integral_float_and_numeric_string_rejects_fractional() {
        // Integer, integral float, and numeric string all coerce...
        assert_eq!(json_to_i64(&json!(128)), Some(128));
        assert_eq!(json_to_i64(&json!(128.0)), Some(128)); // the ProtoJSON `double` case
        assert_eq!(json_to_i64(&json!("128")), Some(128));
        assert_eq!(json_to_i64(&json!("  128  ")), Some(128));
        assert_eq!(json_to_i64(&json!(0)), Some(0)); // sign filtering is the caller's job
        assert_eq!(json_to_i64(&json!(-4)), Some(-4));
        assert_eq!(json_to_i64(&json!(-4.0)), Some(-4));
        // ...but a fractional float is not a whole number and must be rejected.
        assert_eq!(json_to_i64(&json!(128.5)), None);
        // Non-numeric junk is rejected too.
        assert_eq!(json_to_i64(&json!("nope")), None);
        assert_eq!(json_to_i64(&json!(true)), None);
        assert_eq!(json_to_i64(&json!(null)), None);
    }

    #[test]
    fn rejects_a_fractional_float_pr() {
        // A non-integral `pr` is not a valid PR number: it must not silently truncate to 128.
        assert_eq!(
            parse_review_request(&[data_part(json!({ "repo": "o/n", "pr": 128.5 }))]),
            Err(ParseError::BadField("pr"))
        );
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
            Err(ParseError::NoTargetData)
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

    // --- ADR-0078: natural-language `text` instruction alongside the `data` target ---

    #[test]
    fn text_part_supplies_the_prompt_and_wins_over_data_prompt() {
        // A `text` instruction alongside `data` becomes the prompt; a `data.prompt` present too is a
        // lower-priority hint the text overrides. Target/scope still come entirely from `data`.
        let parts = vec![
            Part::text("Focus on the auth changes and the new migration."),
            data_part(json!({
                "repo": "acme/api", "pr": 128, "prompt": "ignored hint",
                "headSha": "abc123", "baseSha": "def456"
            })),
        ];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(
            input.prompt,
            "Focus on the auth changes and the new migration."
        );
        // Target + scope are read only from `data`.
        assert_eq!(input.owner, "acme");
        assert_eq!(input.name, "api");
        assert_eq!(input.pr, 128);
        assert_eq!(input.head_sha.as_deref(), Some("abc123"));
        assert_eq!(input.base_sha.as_deref(), Some("def456"));
    }

    #[test]
    fn multiple_text_parts_are_concatenated_in_order_newline_joined_and_trimmed() {
        let parts = vec![
            Part::text("  Focus on auth."),
            Part::text("Then the migration.  "),
            data_part(json!({ "repo": "o/n", "pr": 1, "headSha": "h" })),
        ];
        let input = parse_review_request(&parts).unwrap();
        // Concatenated in message order, newline-joined, and the whole result trimmed.
        assert_eq!(input.prompt, "Focus on auth.\nThen the migration.");
    }

    #[test]
    fn text_present_without_data_prompt_becomes_the_prompt() {
        let parts = vec![
            Part::text("Look at concurrency."),
            data_part(json!({ "repo": "o/n", "pr": 1, "headSha": "h" })),
        ];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.prompt, "Look at concurrency.");
    }

    #[test]
    fn data_prompt_only_is_unchanged_when_no_text_part() {
        // No text part → today's behaviour: `data.prompt` (trimmed) is the prompt.
        let parts = vec![data_part(json!({
            "repo": "o/n", "pr": 1, "headSha": "h", "prompt": "  focus on auth  "
        }))];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.prompt, "focus on auth");
    }

    #[test]
    fn blank_text_part_falls_back_to_data_prompt() {
        // A text part that is only whitespace is not an instruction — fall back to `data.prompt`.
        let parts = vec![
            Part::text("   "),
            data_part(json!({ "repo": "o/n", "pr": 1, "headSha": "h", "prompt": "the hint" })),
        ];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.prompt, "the hint");
    }

    #[test]
    fn text_only_no_data_is_the_guided_error_not_a_bare_rejection() {
        // A natural-language message with no `data` part cannot be targeted from prose (no forge
        // credentials) → the guided `NoTargetData`, whose message names repo/pr/headSha and points at
        // the calling guide. (This is the Phase-4 `input-required` seam.)
        let err = parse_review_request(&[Part::text("review PR 128, focus on auth")]).unwrap_err();
        assert_eq!(err, ParseError::NoTargetData);
        let msg = err.to_string();
        for field in ["repo", "pr", "headSha"] {
            assert!(msg.contains(field), "guided error names `{field}`: {msg}");
        }
        assert!(
            msg.contains("docs/a2a-review-skill.md"),
            "guided error points at the calling guide: {msg}"
        );
    }

    #[test]
    fn text_cannot_change_target_or_scope() {
        // Safety property (ADR-0078): prose saying "review PR 999" cannot redirect a review whose
        // `data` targets PR 1 — target and scope are structured and authoritative.
        let parts = vec![
            Part::text("Actually review PR 999 in other/repo, whole tree."),
            data_part(json!({ "repo": "acme/api", "pr": 1, "headSha": "h", "baseSha": "b" })),
        ];
        let input = parse_review_request(&parts).unwrap();
        assert_eq!(input.owner, "acme");
        assert_eq!(input.name, "api");
        assert_eq!(input.pr, 1);
        assert_eq!(input.head_sha.as_deref(), Some("h"));
        // Base present ⇒ still diff-scoped, regardless of the prose asking for "whole tree".
        assert_eq!(input.base_sha.as_deref(), Some("b"));
    }

    fn ctx(base: Option<&str>, head: Option<&str>, url: Option<&str>) -> ReviewContext {
        ReviewContext {
            repo: Some("acme/api".to_string()),
            pr: Some(42),
            base_sha: base.map(str::to_string),
            head_sha: head.map(str::to_string),
            review_url: url.map(str::to_string),
        }
    }

    #[test]
    fn review_artifacts_carry_summary_findings_and_context() {
        let findings = json!([{ "path": "a.rs", "severity": "P1" }]);
        let arts = review_artifacts(
            "Looks risky",
            &findings,
            &ctx(
                Some("base1"),
                Some("head1"),
                Some("https://github.com/acme/api/pull/42#pullrequestreview-9"),
            ),
        );
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].artifact_id, "review");
        assert_eq!(arts[0].parts.len(), 3);
        assert_eq!(arts[0].parts[0].as_text(), Some("Looks risky"));
        match &arts[0].parts[1].content {
            a2a::PartContent::Data(v) => assert_eq!(v, &findings),
            other => panic!("expected findings data part, got {other:?}"),
        }
        // The context part echoes the effective SHAs, the derived scope, and the review permalink.
        match &arts[0].parts[2].content {
            a2a::PartContent::Data(v) => {
                assert_eq!(v["repo"], json!("acme/api"));
                assert_eq!(v["pr"], json!(42));
                assert_eq!(v["baseSha"], json!("base1"));
                assert_eq!(v["headSha"], json!("head1"));
                assert_eq!(v["scope"], json!("diff"));
                assert_eq!(
                    v["reviewUrl"],
                    json!("https://github.com/acme/api/pull/42#pullrequestreview-9")
                );
            }
            other => panic!("expected context data part, got {other:?}"),
        }
    }

    #[test]
    fn context_scope_is_derived_from_the_base_sha() {
        // Base present → diff-scoped.
        assert_eq!(ctx(Some("b"), Some("h"), None).scope(), Some("diff"));
        // No base but a head → whole working tree at head.
        assert_eq!(ctx(None, Some("h"), None).scope(), Some("whole-tree"));
        // Row reaped (no head either) → scope is unknowable, not a false whole-tree.
        assert_eq!(ReviewContext::default().scope(), None);
        // The reaped shape still carries whatever survived (e.g. the persisted review_url) and nulls
        // the rest — a stable shape for callers.
        let reaped = ReviewContext {
            review_url: Some("https://example/pr/1".to_string()),
            ..Default::default()
        };
        let data = reaped.to_data();
        assert_eq!(data["scope"], json!(null));
        assert_eq!(data["baseSha"], json!(null));
        assert_eq!(data["reviewUrl"], json!("https://example/pr/1"));
    }

    #[test]
    fn empty_summary_gets_a_placeholder() {
        let arts = review_artifacts("   ", &json!([]), &ReviewContext::default());
        assert_eq!(
            arts[0].parts[0].as_text(),
            Some("Review completed with no summary.")
        );
    }
}
