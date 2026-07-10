//! Review validation + write-back shaping (epic #5, slice 6).
//!
//! The runner submits a structured review (summary + findings). The control plane owns GitHub write
//! access (trust boundary, ADR-0002), so it validates the findings here before posting, and — since
//! this is a *pull-request* review — scopes them to the PR's change set:
//! - a finding on a changed line becomes an **inline** comment (GitHub only accepts inline comments
//!   on diff lines), carrying a committable ```suggestion block when the finding proposes a fix;
//! - a finding on a changed *file* but an unpinnable line is folded into the review **body**;
//! - a finding on a file the PR doesn't touch is **out of scope** and dropped (counted for
//!   transparency in the body), so the review stays about the change rather than the whole repo.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// One finding submitted by the runner (mirrors `agent-runner::review::ReviewFinding`). `Serialize`
/// so the control plane can persist the findings array verbatim (Milestone C review record).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Finding {
    pub file: String,
    /// The line this finding anchors to. When [`Finding::start_line`] is set and the range validates,
    /// `line` is the **LAST** line of the range (GitHub's convention — a ranged review comment always
    /// treats `line` as the end); otherwise it's the single commented line. Either way `line` **is** the
    /// anchor: every downstream consumer keys on it and it alone — cross-run dedup ([`dedup_key`]),
    /// `retract_finding`, the ADR-0035 feedback poller, and the ADR-0040 prior-review context — so
    /// `start_line` widens the *rendered span* without moving the identity. The range's end IS the
    /// anchor.
    pub line: u32,
    /// Optional first line of a multi-line span this finding describes (ADR-0071) — the complement to
    /// [`Finding::line`], which stays the span's *last* line (GitHub's convention for a ranged review
    /// comment) and remains the sole anchor everything downstream keys on. Validated in [`validate`]:
    /// the finding anchors as a GitHub **range** comment only when every line from `start_line` to
    /// `line` (inclusive) is commentable — i.e. contiguous, added/context lines inside a single diff
    /// hunk. When absent, or when the range doesn't validate, the finding falls back to today's
    /// single-line anchor at `line` (never dropped). Optional on the wire so a runner that predates
    /// ADR-0071 (and any already-stored row) still deserializes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// Triage priority `P0`|`P1`|`P2` (ADR-0032). Optional on the wire so rows that predate the
    /// priority model (and an older runner still emitting `severity`) still deserialize;
    /// [`Finding::priority`] falls back to the legacy `severity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Finding dimension — `security`|`correctness`|`quality`|`style`|`performance` (ADR-0032,
    /// extensible). Absent on legacy rows → treated as `correctness`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Legacy `error`|`warning`|`info` level (pre-ADR-0032). Read-only back-compat: still parsed from
    /// old stored rows or an older runner and mapped into a priority; new findings omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    pub title: String,
    pub body: String,
    /// Optional exact replacement for `line`; rendered as a committable GitHub ```suggestion block
    /// when the finding anchors inline.
    #[serde(default)]
    pub suggestion: Option<String>,
    /// Optional links to supporting resources (docs, CWE, RFCs) rendered as a "Resources" list
    /// (epic #89 finding format). Defaults to empty.
    #[serde(default)]
    pub resources: Vec<String>,
}

impl Finding {
    /// Effective triage priority (ADR-0032): explicit `priority`, else shimmed from the legacy
    /// `severity` (error/critical→P0, warning→P1, else→P2), else `P2`.
    pub fn priority(&self) -> &str {
        if let Some(p) = self.priority.as_deref().map(str::trim) {
            if p.eq_ignore_ascii_case("P0") {
                return "P0";
            } else if p.eq_ignore_ascii_case("P1") {
                return "P1";
            } else if p.eq_ignore_ascii_case("P2") {
                return "P2";
            }
        }
        match self.severity.as_deref().map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("error") || s.eq_ignore_ascii_case("critical") => {
                "P0"
            }
            Some(s)
                if s.eq_ignore_ascii_case("warning")
                    || s.eq_ignore_ascii_case("warn")
                    || s.eq_ignore_ascii_case("high") =>
            {
                "P1"
            }
            _ => "P2",
        }
    }

    /// Effective category; defaults to `correctness` when absent (legacy rows / unspecified).
    pub fn category(&self) -> &str {
        self.category
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or("correctness")
    }

    /// Markdown badge **images** for the finding's priority + category (ADR-0032). GitHub markdown
    /// can't colour text, so we use shields.io badges so the level actually reads in colour: priority
    /// **P0 red / P1 orange / P2 lightgrey**, and **`category: security` is always red** regardless of
    /// priority (the explicit ask). The badge label doubles as the image alt-text, so it still conveys
    /// the level if shields.io can't be reached.
    fn level_badges(&self) -> String {
        let priority = self.priority();
        let category = self.category();
        let priority_color = match priority {
            "P0" => "red",
            "P1" => "orange",
            _ => "lightgrey",
        };
        let category_color = if category.eq_ignore_ascii_case("security") {
            "red"
        } else {
            "blue"
        };
        // shields.io reads a single-dash `/badge/<message>-<color>` as a label-less coloured badge:
        // `/badge/P0-red` renders "P0" on red (verified — identical to the `/badge/-P0-red` form).
        format!(
            "![{p}](https://img.shields.io/badge/{p}-{pc}) ![{c}](https://img.shields.io/badge/{c}-{cc})",
            p = priority,
            pc = priority_color,
            c = badge_label(category),
            cc = category_color,
        )
    }
}

/// How many findings of the LATEST prior review to render in full detail (ADR-0040/0065). A bound keeps
/// the injected block small even on a PR that has accumulated many findings; the newest review's
/// findings are the ones most worth re-deriving against.
const PRIOR_FINDINGS_CAP: usize = 30;

/// Char budget for the whole prior-reviews block (ADR-0065). The block is untrusted context, not the
/// review itself — past some size it is pure prompt cost. When the assembled block exceeds this we cut
/// it on a line boundary and append an explicit truncation marker rather than silently dropping tail
/// content. Sized in the same spirit as [`PRIOR_FINDINGS_CAP`]: generous enough for the latest review's
/// detail + a handful of one-line older summaries, bounded enough not to dominate the prompt.
const PRIOR_BLOCK_CHAR_CAP: usize = 8_000;

/// One prior review of this target, as persisted (ADR-0022/0035): the run's ordinal (1 = oldest), its
/// verdict summary, and its findings JSON (an array of [`Finding`]; malformed/empty → "verdict only").
/// The control plane assembles these newest-first; [`format_prior_reviews`] renders the block.
pub struct PriorReview {
    /// 1-based chronological ordinal (1 = the first review on this PR), for a stable human reference in
    /// the compressed lines — more legible than a raw timestamp and independent of clock skew.
    pub ordinal: usize,
    pub summary: String,
    pub findings: serde_json::Value,
}

/// Format **all** prior reviews of this pull request (ADR-0040 + ADR-0065) as one compact, explicitly
/// **untrusted** context block to feed into a re-review. Deterministic (no LLM call): the LATEST review
/// keeps detail (verdict + findings, capped at [`PRIOR_FINDINGS_CAP`]); OLDER reviews are compressed to a
/// single line each (ordinal, one-line verdict, finding count + titles only).
///
/// Wording is prompt engineering (ADR-0065, Option C strengthened). ADR-0040 originally framed this as
/// "reconcile, don't contradict" — but that **anchors** the model: a prior FALSE POSITIVE gets *restated*
/// unchecked instead of retracted (the poisoning observed on vymalo-shop#303–305 and webank-mobile#112).
/// The reframing here is **re-derive-then-reconcile**: prior findings are UNVERIFIED HYPOTHESES from an
/// earlier automated pass, possibly wrong; the model must review the diff independently FIRST, then
/// reconcile — explicitly retracting anything it cannot re-derive, and never inheriting a prior finding
/// without re-verifying it against the code.
///
/// `priors` is ordered **newest-first** (index 0 = the latest review). Returns `None` when there is
/// nothing useful to inject (every prior has an empty verdict and no findings) so the caller leaves the
/// field unset.
///
/// Budgeting: the header + the LATEST review's detail get the [`PRIOR_BLOCK_CHAR_CAP`] budget first (a
/// pathological latest section is cut char-safely by [`cap_block`]); older compressed lines are then
/// appended only while the block stays under budget, and any omitted are counted in an explicit marker —
/// so the latest review's detail always survives and truncation is never silent.
pub fn format_prior_reviews(priors: &[PriorReview]) -> Option<String> {
    // Nothing useful anywhere → no block (mirrors the old single-review empty case). A prior counts as
    // content if it has a non-empty verdict OR a non-empty findings array — an empty/`[]`/malformed
    // findings blob with a blank verdict contributes nothing. (`as_array` — no clone+deserialize just to
    // test emptiness; the detailed sections parse properly below.)
    let has_findings = |p: &PriorReview| p.findings.as_array().is_some_and(|a| !a.is_empty());
    let any_content = priors
        .iter()
        .any(|p| !p.summary.trim().is_empty() || has_findings(p));
    if priors.is_empty() || !any_content {
        return None;
    }

    // Two deliberate scoping choices in this wording (both from codex review on #266):
    // - the no-repeat clause is scoped to the CURRENT COMMIT, matching the finalize dedup's same-head
    //   scope — on a new head_sha a still-valid prior finding must be RESTATED (anchored to the new
    //   diff), not suppressed as "already posted";
    // - retraction is routed to the final VERDICT TEXT, because the `retract_finding` tool only deletes
    //   findings buffered in the current run (and acks even when nothing matched) — it cannot touch an
    //   already-posted comment, so a tool-call "retraction" of a prior finding would be an invisible no-op.
    let mut out = String::from(
        "## Prior automated reviews of this pull request (context only — NOT ground truth)\n\n\
         Earlier automated passes are listed below. They may contain **false positives** — treat every \
         prior finding as an UNVERIFIED HYPOTHESIS, not a fact. **Re-derive your review from the diff \
         first**; then reconcile: restate a prior finding only if you re-derived it from the current \
         code, and **explicitly retract** any prior finding you cannot reproduce — name it in your \
         final verdict text (tools only edit this run's unposted findings; an already-posted comment \
         is retracted by saying so in the verdict). Never inherit a prior finding without re-verifying \
         it. Do not re-post a finding that already stands on the current commit — post only what is \
         new or changed; if new commits changed the code and a prior finding still holds, restate it \
         anchored to the current diff.\n",
    );

    // The latest review (index 0) in detail; the rest compressed to one line each.
    if let Some((latest, older)) = priors.split_first() {
        out.push_str("\n### Latest prior review");
        if let Some(rest) = format_latest_detail(latest) {
            out.push_str(&rest);
        } else {
            out.push_str(" — (no verdict or findings recorded)\n");
        }
        // The latest detail is budgeted FIRST: if it alone blows the cap (pathological verdict/title
        // lengths), cut it char-safely and stop — the older reviews are the lower-signal tail.
        if out.len() > PRIOR_BLOCK_CHAR_CAP {
            let mut capped = cap_block(out);
            if !older.is_empty() {
                capped.push_str(&format!(
                    "… [{} earlier automated review(s) omitted to keep this context bounded] …\n",
                    older.len(),
                ));
            }
            return Some(capped);
        }

        // Older reviews: append one-liners while the block stays under budget; count what's omitted and
        // say so explicitly (ADR-0065: never truncate silently).
        if !older.is_empty() {
            out.push_str("\n### Earlier prior reviews (compressed)\n");
            let mut omitted = 0usize;
            for (i, p) in older.iter().enumerate() {
                let line = compress_prior_line(p);
                if out.len() + line.len() > PRIOR_BLOCK_CHAR_CAP {
                    // Budget exhausted: omit this and everything older (no gaps in the sequence).
                    omitted = older.len() - i;
                    break;
                }
                out.push_str(&line);
            }
            if omitted > 0 {
                out.push_str(&format!(
                    "\n… [{omitted} earlier automated review(s) omitted to keep this context \
                     bounded] …\n",
                ));
            }
        }
    }

    Some(out)
}

/// Detail rendering for the latest prior review: verdict + up to [`PRIOR_FINDINGS_CAP`] findings, each as
/// `[priority/category] file:line — title`. Returns `None` when it has neither (so the caller can note
/// "nothing recorded" rather than emit an empty section).
fn format_latest_detail(p: &PriorReview) -> Option<String> {
    let parsed: Vec<Finding> = serde_json::from_value(p.findings.clone()).unwrap_or_default();
    let summary = p.summary.trim();
    if summary.is_empty() && parsed.is_empty() {
        return None;
    }
    let mut out = String::from("\n");
    if !summary.is_empty() {
        out.push_str("\nPrior verdict: ");
        out.push_str(summary);
        out.push('\n');
    }
    if !parsed.is_empty() {
        out.push_str("\nPrior findings (unverified — re-derive or retract):\n");
        for f in parsed.iter().take(PRIOR_FINDINGS_CAP) {
            out.push_str(&format!(
                "- [{}/{}] {}:{} — {}\n",
                f.priority(),
                f.category(),
                f.file,
                f.line,
                f.title.trim(),
            ));
        }
        if parsed.len() > PRIOR_FINDINGS_CAP {
            out.push_str(&format!(
                "- … and {} more (older/lower-priority) — re-derive from the diff if still relevant\n",
                parsed.len() - PRIOR_FINDINGS_CAP,
            ));
        }
    }
    Some(out)
}

/// One-line compression of an older prior review: ordinal, a one-line verdict, and the finding count +
/// titles only (no priority/category/line detail — the latest review carries that). Titles are joined so
/// the model still knows *what* the older pass raised without the block ballooning.
fn compress_prior_line(p: &PriorReview) -> String {
    let parsed: Vec<Finding> = serde_json::from_value(p.findings.clone()).unwrap_or_default();
    let verdict = one_line(p.summary.trim());
    let titles: Vec<String> = parsed
        .iter()
        .map(|f| f.title.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let verdict_part = if verdict.is_empty() {
        "no verdict".to_string()
    } else {
        verdict
    };
    if titles.is_empty() {
        format!("- review #{}: {verdict_part} (0 findings)\n", p.ordinal)
    } else {
        format!(
            "- review #{}: {verdict_part} ({} finding(s): {})\n",
            p.ordinal,
            titles.len(),
            titles.join("; "),
        )
    }
}

/// Collapse a possibly-multiline verdict to a single line (compressed older reviews are one line each).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cap the assembled block at [`PRIOR_BLOCK_CHAR_CAP`], cutting on a line boundary and appending an
/// explicit truncation marker (ADR-0065: note truncation, don't drop silently). No-op when under budget.
///
/// The cut is **UTF-8-safe**: `PRIOR_BLOCK_CHAR_CAP` is a byte offset, and finding titles/verdicts are
/// arbitrary text (accents, emoji, CJK), so the cap can land inside a multi-byte code point — slicing
/// there would panic and wedge the whole task-context fetch. Walk back to a char boundary first, then to
/// the last newline so no line is severed mid-way. The marker makes no claim about *what* was omitted —
/// this path can cut the latest review's own detail, not just an older tail.
fn cap_block(block: String) -> String {
    if block.len() <= PRIOR_BLOCK_CHAR_CAP {
        return block;
    }
    let mut boundary = PRIOR_BLOCK_CHAR_CAP;
    while boundary > 0 && !block.is_char_boundary(boundary) {
        boundary -= 1;
    }
    // Cut at the last newline within budget so we never sever a line mid-way.
    let cut = block[..boundary].rfind('\n').unwrap_or(boundary);
    let mut truncated = block[..cut].to_string();
    truncated.push_str(
        "\n\n… [prior-review context truncated here to stay within the prompt budget — re-derive from \
         the diff; anything omitted was context only] …\n",
    );
    truncated
}

/// Normalized dedup key for a finding (ADR-0065, Option B): repo-relative path, line, and a
/// whitespace-collapsed + case-folded title. Trivial re-phrasings/casing of the same finding on the same
/// `(file, line)` collapse to one key, so a re-review's byte-near-identical finding matches a prior one.
pub fn dedup_key(file: &str, line: u32, title: &str) -> (String, u32, String) {
    let file = normalize_path(file);
    let title = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (file, line, title)
}

/// Drop, from `findings`, any finding whose [`dedup_key`] matches one already posted on this PR by a
/// prior Lightbridge review (ADR-0065, Option B). `posted` is the set of prior findings' normalized keys
/// (from `reviews.findings` on the SAME head_sha — line numbers drift across commits, so cross-commit
/// matching is unsafe). Returns `(kept, deduped_n)`; `deduped_n` is logged/counted by the caller.
pub fn dedup_against_posted(
    findings: Vec<Finding>,
    posted: &HashSet<(String, u32, String)>,
) -> (Vec<Finding>, usize) {
    if posted.is_empty() {
        return (findings, 0);
    }
    let mut deduped_n = 0usize;
    let kept = findings
        .into_iter()
        .filter(|f| {
            let matched = posted.contains(&dedup_key(&f.file, f.line, &f.title));
            if matched {
                deduped_n += 1;
            }
            !matched
        })
        .collect();
    (kept, deduped_n)
}

/// Format the repo's previously-rejected findings (👎) as an untrusted context block (M1 memory,
/// ADR-0044) — fed into a review so the agent doesn't re-raise false positives a human already shot
/// down. `rejected` is `(file, line, title)`; returns `None` when there's nothing to inject.
pub fn format_repo_memory(rejected: &[(String, i32, String)]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Memory: findings rejected here before (👎)\n\n\
         A human marked these past findings on this repo as wrong / not useful. Do NOT raise them \
         again unless the code has materially changed and you can prove the issue now holds — treat a \
         match here as a strong signal to drop the finding.\n",
    );
    for (file, line, title) in rejected {
        out.push_str(&format!("- {file}:{line} — {}\n", title.trim()));
    }
    Some(out)
}

/// Sanitize a badge label for a shields.io URL path segment: spaces/underscores/dashes (which shields
/// treats specially) collapse to a safe token, non-alphanumerics are dropped. Our categories are
/// single ASCII words, so this is just defensive against an odd model value.
fn badge_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let token = cleaned.split_whitespace().collect::<Vec<_>>().join("_");
    if token.is_empty() {
        "finding".to_string()
    } else {
        token
    }
}

/// An inline PR review comment, shaped for the GitHub API.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineComment {
    pub path: String,
    /// The anchor line. With `start_line` set it's the **LAST** line of the range (GitHub's
    /// convention); otherwise the single commented line. This is the anchor the whole pipeline keys on
    /// — `start_line` only widens the rendered span.
    pub line: u32,
    /// `Some` when this finding's `start_line..=line` range validated (ADR-0071) — GitHub renders it
    /// as a ranged comment (`start_line` + `start_side: RIGHT` alongside `line` + `side: RIGHT`).
    /// `None` for the (overwhelming majority) single-line comment, byte-for-byte as before this ADR.
    pub start_line: Option<u32>,
    pub body: String,
}

/// The result of validating findings against the PR diff: comments to anchor inline, findings on a
/// changed file that couldn't anchor to an exact line (rendered into the body), and findings on files
/// the PR doesn't touch (out of scope for a PR review).
#[derive(Debug, Default)]
pub struct ValidatedReview {
    pub inline: Vec<InlineComment>,
    pub deferred: Vec<Finding>,
    /// Findings on files outside the PR's diff. Surfaced in a collapsible body section rather than
    /// silently dropped (ADR-0033 "no silent drops") — the body still notes the count.
    pub out_of_scope: Vec<Finding>,
}

/// The RIGHT-side (new file) line numbers that are commentable for one file's unified-diff `patch` —
/// the added (`+`) and context (` `) lines within the hunks. GitHub only accepts inline comments on
/// these lines.
pub fn commentable_lines(patch: &str) -> BTreeSet<u32> {
    let mut lines = BTreeSet::new();
    let mut new_line: u32 = 0;
    for raw in patch.lines() {
        if let Some(start) = parse_hunk_new_start(raw) {
            new_line = start;
            continue;
        }
        match raw.as_bytes().first() {
            Some(b'+') => {
                lines.insert(new_line);
                new_line += 1;
            }
            Some(b' ') => {
                lines.insert(new_line);
                new_line += 1;
            }
            Some(b'-') => { /* deleted line — no new-side number */ }
            _ => { /* "\ No newline at end of file", etc. */ }
        }
    }
    lines
}

/// Parse the new-side start line from a hunk header `@@ -a,b +c,d @@` → `c`.
fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split('+').nth(1)?; // "c,d @@ ..."
    let num = plus
        .split([',', ' '])
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit());
    num.parse().ok()
}

/// Validate findings against the PR's changed files. `commentable` maps each **changed** file path →
/// its commentable line set (from [`commentable_lines`]). Dedups by `(file, line, title)`.
///
/// Scoping (a PR review reviews the PR, not the whole repo):
/// - file is in the diff **and** the line is commentable → **inline** comment (with a ```suggestion
///   block when the finding carries one);
/// - file is in the diff but the line isn't anchorable → **deferred** to the body (still part of the
///   change, just not pinnable);
/// - file is **not** in the diff → **out of scope**, dropped (counted, not posted).
///
/// Safety valve: when `commentable` is empty we couldn't determine the change set (e.g. no patchable
/// files), so we don't know what's in scope — fall back to deferring everything rather than dropping
/// the whole review.
pub fn validate(
    findings: Vec<Finding>,
    commentable: &HashMap<String, BTreeSet<u32>>,
) -> ValidatedReview {
    let scope_known = !commentable.is_empty();
    let mut seen: HashSet<(String, u32, String)> = HashSet::new();
    let mut review = ValidatedReview::default();

    for mut finding in findings {
        // Normalize the model's path to the repo-root-relative, forward-slash form GitHub uses for
        // the `commentable` keys — otherwise `./src/x`, `/src/x` or `src\x` would miss the lookup and
        // a valid finding would be wrongly dropped as out of scope.
        finding.file = normalize_path(&finding.file);
        let key = (finding.file.clone(), finding.line, finding.title.clone());
        if !seen.insert(key) {
            continue; // duplicate
        }
        let in_changed_file = commentable.contains_key(&finding.file);
        if scope_known && !in_changed_file {
            review.out_of_scope.push(finding); // outside the PR diff — surfaced, not dropped
            continue;
        }
        let file_lines = commentable.get(&finding.file);
        let anchorable = file_lines.is_some_and(|lines| lines.contains(&finding.line));
        if anchorable && finding.line > 0 {
            let start_line = validated_range_start(&finding, file_lines);
            let body = inline_body(&finding);
            review.inline.push(InlineComment {
                path: finding.file,
                line: finding.line,
                start_line,
                body,
            });
        } else {
            review.deferred.push(finding);
        }
    }
    review
}

/// Resolve a finding's `start_line` (ADR-0071) into a validated range anchor, or `None` to fall back to
/// today's single-line comment at `line`. The range validates only when:
/// - `start_line` is present, and
/// - `start_line <= line` (GitHub always treats `line` as the range's *last* line), and
/// - every line from `start_line` to `line` (inclusive) is in the file's `commentable` set.
///
/// That last check is both the contiguity check AND the single-hunk check: [`commentable_lines`] only
/// ever inserts a line that is actually an added/context line inside some hunk, and hunks never share
/// line numbers (they're strictly increasing down the file), so a contiguous run of membership can only
/// come from one hunk. GitHub itself rejects a range that crosses a hunk boundary or starts on a
/// non-commentable line — this must be checked here, before the API is ever called (ADR-0022's
/// "validate before posting" contract, extended to ranges).
///
/// The caller has already established `finding.line` itself is anchorable; this only resolves whether
/// the *range* additionally validates.
fn validated_range_start(finding: &Finding, file_lines: Option<&BTreeSet<u32>>) -> Option<u32> {
    let start = finding.start_line?;
    if start == 0 || start > finding.line {
        return None; // not a valid range end/start ordering — fall back to single-line
    }
    let lines = file_lines?;
    (start..=finding.line)
        .all(|l| lines.contains(&l))
        .then_some(start)
}

/// Render an inline comment body: the level badges + titled finding, plus a committable GitHub
/// ```suggestion block when the finding proposes a replacement. A *present but empty* suggestion is
/// kept — on GitHub an empty suggestion block is a valid "delete this line" — so we gate on presence
/// (Some vs None), not on emptiness.
/// Strip model-internal artifacts before text is posted to GitHub (run 7c15f9bb): `<think>…</think>`
/// reasoning and tool-call control tokens (`<｜…｜>` / `<|…|>`) that some models (deepseek) leak into
/// `content` instead of the structured fields. Defensive last line — even if the gateway/model
/// misbehaves, raw reasoning / control tokens never reach a PR.
pub fn strip_model_artifacts(text: &str) -> String {
    let mut s = text.to_string();
    // Leading orphan reasoning ("reasoning… </think> answer" with no opener) → drop through the close.
    if let Some(i) = s.find("</think>") {
        if !s[..i].contains("<think>") {
            s = s[i + "</think>".len()..].to_string();
        }
    }
    s = remove_spans(&s, "<think>", "</think>"); // paired blocks (unclosed → drop remainder)
    s = remove_spans(&s, "<｜", "｜>"); // deepseek special tokens (fullwidth pipe)
    s = remove_spans(&s, "<|", "|>"); // ASCII-pipe variant
    s.replace("<think>", "")
        .replace("</think>", "")
        .trim()
        .to_string()
}

/// Remove every `open…close` span (inclusive); an unclosed `open` drops the remainder. `open`/`close`
/// are whole substrings, so the byte offsets from `find` are always on char boundaries.
fn remove_spans(input: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        match rest.find(open) {
            Some(i) => {
                out.push_str(&rest[..i]);
                let after = &rest[i + open.len()..];
                match after.find(close) {
                    Some(j) => rest = &after[j + close.len()..],
                    None => break, // unclosed → drop the remainder
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Invitation to leave a 👍/👎 reaction so the feedback poller (ADR-0035) has a signal to read. The
/// reactions were always polled, but nothing ever asked the author to leave one — so the channel sat
/// idle. Appended to every surface the poller actually reads reactions on: inline findings and the
/// answer/reply. Deliberately **not** on the review summary (a `reviews` row — GitHub exposes no
/// reactions endpoint for a PR review body, and the poller doesn't poll it) nor the failure notice
/// (don't beg feedback on an apology).
///
/// No leading `---`: the CTA is a quiet quoted line, not a section break — no horizontal rule before it.
const FEEDBACK_FOOTER: &str = "\n\n> Was this useful? React 👍/👎 to give us feedback";

// ADR-0071: a ```suggestion fence's replacement span is NOT markdown metadata — GitHub derives it from
// the *surrounding comment's* `start_line`/`line` (set on `InlineComment`/`ReviewComment` by
// `validate`'s `validated_range_start`, independent of this function). So when a finding anchors as a
// validated range, the fence rendered below is already correct for the whole `start_line..=line` span
// with NO change to its content or this function — the range is honored by posting `start_line` +
// `start_side: RIGHT` alongside it, not by anything inside the ```suggestion block itself. This
// function only ever needs the multi-line replacement text the finding already carries in
// `suggestion` (never synthesized here).
fn inline_body(finding: &Finding) -> String {
    // Standardized finding format (epic #89): badge row → titled finding → explanation → committable
    // suggestion → resources. The badges sit on their OWN line above the bold title, separated by a
    // paragraph break (`\n\n`) so the level reads as a header, not a prefix crowding the title. A
    // paragraph break is used (not a single `\n`) because GitLab's CommonMark renderer does NOT
    // treat a single newline as a line break (GitHub's comment renderer does, but that is a
    // GitHub-specific behavior) — `\n\n` renders correctly on BOTH platforms.
    let mut body = format!(
        "{}\n\n**{}**\n\n{}",
        finding.level_badges(),
        strip_model_artifacts(&finding.title),
        strip_model_artifacts(&finding.body)
    );
    if let Some(suggestion) = finding.suggestion.as_deref().map(str::trim_end) {
        body.push_str(&format!("\n\n```suggestion\n{suggestion}\n```"));
    }
    body.push_str(&resources_block(finding));
    body.push_str(FEEDBACK_FOOTER);
    body
}

/// A "Resources" markdown list for a finding's links, or empty when it has none. Shared by the inline
/// and deferred renderings so every finding looks the same (epic #89).
fn resources_block(finding: &Finding) -> String {
    let links: Vec<&String> = finding
        .resources
        .iter()
        .filter(|r| !r.trim().is_empty())
        .collect();
    if links.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n**Resources**\n");
    for link in links {
        out.push_str(&format!("- {link}\n"));
    }
    out
}

/// Normalize a model-supplied path toward the repo-root-relative, forward-slash form GitHub uses:
/// backslashes → `/`, and any leading `./` or `/` stripped.
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Render the review body in the AI-governance shape: the agent's scoped assessment, any findings on
/// changed files that couldn't be pinned to an inline line, a **collapsible** section for findings
/// outside the PR's diff (surfaced, not silently dropped — ADR-0033), and the working-agreement
/// disclosure (AI output is untrusted; a human owns the decision).
pub fn render_body(summary: &str, deferred: &[Finding], out_of_scope: &[Finding]) -> String {
    let mut body = format!(
        "## Lightbridge review\n\n{}",
        strip_model_artifacts(summary)
    );
    append_finding_sections(&mut body, deferred, out_of_scope);
    body.push_str(REVIEW_DISCLOSURE);
    body
}

/// The untrusted-output disclosure appended to every review body (the AI-governance working agreement:
/// AI output is untrusted; a human owns the decision). Shared by [`render_body`] and
/// [`render_fast_body`] so the two paths can't drift.
const REVIEW_DISCLOSURE: &str =
    "\n\n---\n_🤖 AI-generated review — treat it as untrusted, verify before acting; a human \
     owns the final decision ([AI governance](https://adorsys-gis.github.io/ai-governance/))._";

/// Append the "Notes on changed files" (deferred findings) and the collapsed out-of-scope section to a
/// review body. Factored out of [`render_body`] so the fast-pass body renders findings identically.
fn append_finding_sections(body: &mut String, deferred: &[Finding], out_of_scope: &[Finding]) {
    // A finding as a bullet whose first paragraph is the badge row, with the bold title + `file:line`
    // in a separate paragraph (indented to the list-item content column) and the body under that — so
    // the badges never share a line with the title, matching the inline rendering. A paragraph break
    // (`\n\n`) is used (not a single `\n`) because GitLab's CommonMark renderer does NOT treat a single
    // newline as a line break — `\n\n` renders correctly on BOTH platforms. The 2-space indent keeps
    // the continuation paragraphs inside the list item (Gemini #153). Shared by the changed-files
    // notes and the out-of-scope section.
    let render_finding = |body: &mut String, f: &Finding| {
        body.push_str(&format!(
            "\n- {}\n\n  **{}** — `{}:{}`\n\n  {}",
            f.level_badges(),
            strip_model_artifacts(&f.title),
            f.file,
            f.line,
            // Indent continuation lines so a multi-line body stays inside the list item (Gemini #153).
            strip_model_artifacts(&f.body).replace('\n', "\n  ")
        ));
        for link in f.resources.iter().filter(|r| !r.trim().is_empty()) {
            body.push_str(&format!("\n\n  - {link}"));
        }
    };

    if !deferred.is_empty() {
        body.push_str("\n\n### Notes on changed files\n");
        body.push_str("_Findings on this PR's changes that couldn't be pinned to a diff line._\n");
        for f in deferred {
            render_finding(body, f);
        }
    }

    if !out_of_scope.is_empty() {
        // Demoted, not dropped (ADR-0033 keeps them recoverable; Google eng-practices says file a bug
        // for pre-existing issues, don't block the CL). These are on code this PR does NOT change, so
        // they are NOT findings on it: render them **without** severity badges or bodies — just a terse
        // title + file in a collapsed section — so they read as informational pre-existing notes, not
        // the alarming P0 false-positives a human had to refute on izhub#207.
        let n = out_of_scope.len();
        body.push_str(&format!(
            "\n\n<details>\n<summary>{n} pre-existing observation(s) about code outside this PR's diff \
             (informational — not findings on this change)</summary>\n"
        ));
        for f in out_of_scope {
            body.push_str(&format!("\n- **{}** — `{}`", f.title, f.file));
        }
        body.push_str("\n</details>");
    }
}

/// Render the FAST-tier (ADR-0062) review body. Unlike [`render_body`] it is deliberately marked as a
/// **quick pass, not the authoritative review**: it leads with a blockquote banner that says what the
/// pass is (SAST + a diff-scoped look, no repo-wide retrieval) and how to get the deep review (mention
/// the GitHub App by its real handle). The handle lives only control-plane-side (`GITHUB_APP_HANDLE`),
/// which is why the fast body is composed here and not by the runner (which hardcoded the wrong handle).
/// `summary` is the model's `finish` verdict when it converged, or `None` for an exhausted/clean pass —
/// in which case the banner stands alone (inline findings still post as review comments). Findings that
/// couldn't anchor are appended exactly as in the full body.
pub fn render_fast_body(
    handle: &str,
    summary: Option<&str>,
    deferred: &[Finding],
    out_of_scope: &[Finding],
) -> String {
    let handle = handle.trim();
    let mention = if handle.is_empty() {
        "mention me on this PR".to_string()
    } else {
        format!("mention @{handle} on this PR")
    };
    let mut body = format!(
        "> 🅵 **Fast automated pass** — SAST + a quick, diff-scoped look (no repo-wide retrieval). \
         For a deeper, repo-aware review, {mention}."
    );
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str(&format!("\n\n{}", strip_model_artifacts(s)));
    }
    append_finding_sections(&mut body, deferred, out_of_scope);
    body.push_str(REVIEW_DISCLOSURE);
    body
}

/// Render an `ask` answer (ADR-0033) as a reply comment: the agent's Markdown answer verbatim under a
/// heading, plus the same untrusted-output disclosure the review body carries. No diff scoping — a
/// question gets a direct reply.
pub fn render_answer_body(answer: &str) -> String {
    format!(
        "## Lightbridge answer\n\n{}\n\n---\n_🤖 AI-generated answer — treat it as untrusted, \
         verify before acting; a human owns the final decision \
         ([AI governance](https://adorsys-gis.github.io/ai-governance/))._{}",
        strip_model_artifacts(answer),
        FEEDBACK_FOOTER
    )
}

/// The fallback notice posted on a PR when a task fails terminally **without** finalizing, so the
/// author isn't left in silence (ADR-0056). Intentionally short and actionable. The body avoids
/// "review"/"findings" because the sweep is `kind`-agnostic — a failed `ask`-on-PR gets this too
/// (ADR-0057) — so it must read true for a question as well as a review.
pub fn render_failure_notice() -> String {
    "## Lightbridge review\n\n\
     ⚠️ Something went wrong and I couldn't finish — nothing was posted.\n\n\
     Re-mention me on this PR (or push a new commit) to try again.\n\n\
     ---\n_🤖 AI-generated notice — treat it as untrusted, verify before acting; a human owns the \
     final decision ([AI governance](https://adorsys-gis.github.io/ai-governance/))._"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Explicit `\n` (no backslash-continuation) so the leading diff markers (' ', '+', '-') survive.
    const PATCH: &str =
        "@@ -1,3 +1,4 @@ fn main() {\n let a = 1;\n-    let b = 2;\n+    let b = 3;\n+    let c = 4;\n println!(\"{a}\");";

    #[test]
    fn commentable_lines_are_added_and_context() {
        let lines = commentable_lines(PATCH);
        // new side: 1 (context ' let a'), 2 (+let b), 3 (+let c), 4 (context println)
        assert_eq!(lines.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    fn finding(file: &str, line: u32, title: &str) -> Finding {
        Finding {
            file: file.into(),
            line,
            start_line: None,
            priority: Some("P1".into()),
            category: Some("correctness".into()),
            severity: None,
            title: title.into(),
            body: "b".into(),
            suggestion: None,
            resources: Vec::new(),
        }
    }

    #[test]
    fn inline_body_renders_level_badges_suggestion_and_resources() {
        let mut f = finding("a.rs", 1, "Null deref");
        f.priority = Some("P0".into());
        f.category = Some("security".into());
        f.body = "explanation".into();
        f.suggestion = Some("let x = y;".into());
        f.resources = vec![
            "https://cwe.mitre.org/data/definitions/476.html".into(),
            "  ".into(), // blank → skipped
        ];
        let body = inline_body(&f);
        // Level is a coloured shields.io badge image (ADR-0032), not text: P0 red + security red.
        assert!(
            body.starts_with("![P0](https://img.shields.io/badge/P0-red)"),
            "priority badge leads: {body}"
        );
        assert!(body.contains("![security](https://img.shields.io/badge/security-red)"));
        assert!(body.contains("**Null deref**"));
        // The badge row sits on its own paragraph, with the bold title in the next paragraph (not
        // crowding it). A paragraph break (`\n\n`) is used because GitLab's CommonMark renderer does
        // NOT treat a single `\n` as a line break (GitHub does, but that's GitHub-specific).
        assert!(
            body.contains(")\n\n**Null deref**"),
            "badges and title on separate paragraphs: {body}"
        );
        assert!(body.contains("\n\nexplanation"));
        assert!(body.contains("```suggestion\nlet x = y;\n```"));
        assert!(body.contains("**Resources**\n- https://cwe.mitre.org/data/definitions/476.html"));
    }

    /// The 👍/👎 invitation rides only on the surfaces the feedback poller actually reads reactions on
    /// (inline findings + the answer/reply) — not the review summary (no reactions endpoint, not polled)
    /// nor the failure notice (don't beg feedback on an apology).
    #[test]
    fn feedback_footer_only_on_reaction_polled_surfaces() {
        let cta = "Was this useful? React 👍/👎";
        assert!(
            inline_body(&finding("a.rs", 1, "x")).contains(cta),
            "inline finding invites a reaction"
        );
        assert!(
            render_answer_body("hi").contains(cta),
            "the answer/reply invites a reaction"
        );
        assert!(
            !render_body("verdict", &[], &[]).contains(cta),
            "the review summary is not reaction-pollable — no false invitation"
        );
        assert!(
            !render_failure_notice().contains(cta),
            "the failure notice does not solicit feedback"
        );
    }

    #[test]
    fn legacy_severity_is_shimmed_to_a_priority_badge() {
        // An old stored row (severity only, no priority/category) still renders: error → P0 red.
        let f = Finding {
            severity: Some("error".into()),
            priority: None,
            category: None,
            ..finding("a.rs", 1, "Old finding")
        };
        assert_eq!(f.priority(), "P0");
        assert_eq!(f.category(), "correctness");
        assert!(inline_body(&f).contains("https://img.shields.io/badge/P0-red"));
    }

    #[test]
    fn strip_model_artifacts_removes_reasoning_and_tool_tokens() {
        // Leading orphan reasoning + a leaked deepseek tool-call token (run 7c15f9bb).
        let leaked = "Let me check the type...</think>\n\nThe fix is correct. \
                      <｜DSML｜tool_calls｜><｜DSML｜invoke name=\"read_file\"｜>";
        let clean = strip_model_artifacts(leaked);
        assert_eq!(clean, "The fix is correct.", "got: {clean:?}");
        // Paired think block + ASCII-pipe token.
        assert_eq!(
            strip_model_artifacts("<think>noisy</think>real answer <|tool|>"),
            "real answer"
        );
        // Clean text is untouched (and a lone `<` in prose survives).
        assert_eq!(
            strip_model_artifacts("a < b is a real comparison"),
            "a < b is a real comparison"
        );
    }

    #[test]
    fn format_repo_memory_lists_rejected_or_none() {
        assert!(format_repo_memory(&[]).is_none(), "empty → no block");
        let block = format_repo_memory(&[
            ("src/a.rs".into(), 12, "Bogus null-deref".into()),
            ("src/b.rs".into(), 3, "Style nit".into()),
        ])
        .expect("some");
        assert!(block.contains("rejected here before"));
        assert!(block.contains("src/a.rs:12 — Bogus null-deref"));
        assert!(block.contains("src/b.rs:3 — Style nit"));
    }

    fn prior(ordinal: usize, summary: &str, findings: Vec<Finding>) -> PriorReview {
        PriorReview {
            ordinal,
            summary: summary.to_string(),
            findings: serde_json::to_value(findings).unwrap(),
        }
    }

    #[test]
    fn format_prior_reviews_latest_detailed_older_compressed() {
        // Newest-first: the latest review (ordinal 2) is detailed; the older (ordinal 1) is one line.
        let priors = vec![
            prior(
                2,
                "Sound change, one P1.",
                vec![
                    finding("src/store.ts", 65, "IndexedDB connection leak in tx()"),
                    finding(
                        "src/store.ts",
                        156,
                        "Non-numeric exp treated as never-expired",
                    ),
                ],
            ),
            prior(
                1,
                "Two issues on the first pass.\nsecond line of verdict.",
                vec![finding("src/a.ts", 3, "Off-by-one in loop")],
            ),
        ];
        let block = format_prior_reviews(&priors).expect("some context");

        // Untrusted framing + re-derive-then-retract wording (Option C, strengthened).
        assert!(block.contains("context only — NOT ground truth"));
        assert!(block.contains("UNVERIFIED HYPOTHESIS"));
        assert!(block.contains("Re-derive your review from the diff"));
        assert!(
            block.contains("explicitly retract"),
            "retraction framing present: {block}"
        );
        // Retraction is routed to the verdict TEXT — the retract_finding tool only edits the current
        // run's unposted buffer, so a tool-call "retraction" of a prior comment would be a no-op.
        assert!(
            block.contains("name it in your final verdict text"),
            "retractions go to the verdict, not the buffered tool: {block}"
        );
        // The no-repeat clause is commit-scoped (matches the finalize dedup's same-head scope): on a
        // new head_sha a still-valid prior finding must be restated, not suppressed.
        assert!(
            block.contains("already stands on the current commit"),
            "dedup-awareness is scoped to the current commit: {block}"
        );
        assert!(
            block.contains("restate it anchored to the current diff"),
            "still-valid findings are restated on a new commit, not suppressed: {block}"
        );

        // Latest review detailed: verdict + `[priority/category] file:line — title` findings.
        assert!(block.contains("### Latest prior review"));
        assert!(block.contains("Prior verdict: Sound change, one P1."));
        assert!(
            block.contains("[P1/correctness] src/store.ts:65 — IndexedDB connection leak in tx()")
        );
        assert!(block.contains("src/store.ts:156 — Non-numeric exp treated as never-expired"));

        // Older review compressed to one line: ordinal + one-line verdict + count + titles, no line detail.
        assert!(block.contains("### Earlier prior reviews (compressed)"));
        assert!(
            block.contains(
                "- review #1: Two issues on the first pass. second line of verdict. \
                 (1 finding(s): Off-by-one in loop)"
            ),
            "older review is a single compressed line: {block}"
        );
        assert!(
            !block.contains("[P1/correctness] src/a.ts:3"),
            "the older review is NOT rendered in per-finding detail"
        );
    }

    #[test]
    fn format_prior_reviews_truncates_with_explicit_marker_and_keeps_latest() {
        // Many older reviews with long titles blow past the char cap → the LATEST review's detail
        // always survives (it is budgeted first) and the omitted older lines are counted explicitly.
        let big_title = "x".repeat(400);
        let mut priors = vec![prior(60, "latest", vec![finding("a.ts", 1, "leak")])];
        for i in (1..=59).rev() {
            priors.push(prior(
                i,
                "older verdict here",
                vec![finding("a.ts", 1, &big_title)],
            ));
        }
        let block = format_prior_reviews(&priors).expect("some context");
        assert!(
            block.len() <= PRIOR_BLOCK_CHAR_CAP + 300,
            "block is capped near the budget: {} chars",
            block.len()
        );
        assert!(
            block.contains("[P1/correctness] a.ts:1 — leak"),
            "the latest review's detail is never sacrificed to older lines: {block}"
        );
        assert!(
            block.contains("earlier automated review(s) omitted"),
            "omission is counted explicitly, not silent"
        );
    }

    #[test]
    fn format_prior_reviews_latest_overflow_is_cut_with_neutral_marker() {
        // A pathological LATEST review that alone exceeds the cap is cut (char-safely) with a marker
        // that does NOT claim the omitted content was older/lower-signal — here it is the latest's own
        // findings — and the skipped older reviews are still counted.
        let huge_title = "🐛 mega finding ".repeat(80); // multi-byte chars in the overflowing section
        let latest_findings: Vec<Finding> = (1..=30)
            .map(|i| finding("a.ts", i, huge_title.trim()))
            .collect();
        let priors = vec![
            prior(3, "latest verdict", latest_findings),
            prior(2, "older", vec![finding("b.ts", 1, "old nit")]),
            prior(1, "oldest", vec![]),
        ];
        let block = format_prior_reviews(&priors).expect("some context");
        assert!(
            block.len() <= PRIOR_BLOCK_CHAR_CAP + 400,
            "capped near budget: {} chars",
            block.len()
        );
        assert!(
            block.contains("truncated here to stay within the prompt budget"),
            "neutral truncation marker present: {block}"
        );
        assert!(
            !block.contains("omitted tail is older"),
            "the marker must not claim the cut content was older — it can be the latest's own findings"
        );
        assert!(
            block.contains("2 earlier automated review(s) omitted"),
            "the skipped older reviews are still counted: {block}"
        );
    }

    #[test]
    fn cap_block_cut_is_utf8_safe() {
        // Regression (gemini/codex on #266): `PRIOR_BLOCK_CHAR_CAP` is a byte offset and the block is
        // arbitrary text — the cap can land INSIDE a multi-byte code point, and a naive `block[..CAP]`
        // slice panics there (wedging the whole task-context fetch). Build a block whose CAP'th byte
        // straddles a 4-byte emoji and prove the cut walks back to a char boundary instead.
        let mut s = String::from("first line\n");
        s.push_str(&"a".repeat(PRIOR_BLOCK_CHAR_CAP - s.len() - 1));
        s.push_str(&"😀".repeat(8)); // first emoji starts 1 byte before the cap → cap is mid-char
        assert!(
            !s.is_char_boundary(PRIOR_BLOCK_CHAR_CAP),
            "test setup: the cap must straddle a code point"
        );
        let capped = cap_block(s); // must not panic
        assert!(
            capped.len() < PRIOR_BLOCK_CHAR_CAP + 300,
            "cut near the budget"
        );
        assert!(
            capped.contains("truncated here to stay within the prompt budget"),
            "marker present: {capped}"
        );
    }

    #[test]
    fn format_prior_reviews_is_none_when_empty() {
        // No priors, or every prior empty (no verdict, no findings) → caller leaves the field unset.
        assert!(format_prior_reviews(&[]).is_none());
        assert!(
            format_prior_reviews(&[prior(1, "   ", vec![])]).is_none(),
            "an all-empty prior yields no block"
        );
        // A verdict alone still yields a block (a clean review legitimately has no findings).
        assert!(format_prior_reviews(&[prior(1, "No issues found.", vec![])]).is_some());
        // A malformed findings blob degrades to verdict-only rather than erroring.
        let malformed = PriorReview {
            ordinal: 1,
            summary: "verdict".into(),
            findings: serde_json::json!({"oops": true}),
        };
        let block =
            format_prior_reviews(&[malformed]).expect("verdict survives malformed findings");
        assert!(block.contains("Prior verdict: verdict"));
        assert!(!block.contains("Prior findings"));
    }

    #[test]
    fn dedup_against_posted_drops_normalized_identical_findings() {
        // A prior review posted these two findings on this head_sha.
        let posted: HashSet<(String, u32, String)> = [
            dedup_key("src/store.ts", 65, "IndexedDB connection leak in tx()"),
            dedup_key(
                "src/store.ts",
                156,
                "Non-numeric exp treated as never-expired",
            ),
        ]
        .into_iter()
        .collect();

        let current = vec![
            // Same file/line, title differs only in whitespace + casing → normalized-identical → dropped.
            finding("src/store.ts", 65, "indexeddb   connection LEAK in tx()"),
            // A `./`-prefixed path normalizes to the same key → dropped.
            finding(
                "./src/store.ts",
                156,
                "Non-numeric exp treated as never-expired",
            ),
            // Genuinely new finding → kept.
            finding("src/store.ts", 200, "New race condition"),
        ];
        let (kept, deduped_n) = dedup_against_posted(current, &posted);
        assert_eq!(deduped_n, 2, "the two re-posted findings are dropped");
        assert_eq!(kept.len(), 1, "only the genuinely-new finding survives");
        assert_eq!(kept[0].title, "New race condition");

        // Empty posted-set is a fast no-op that keeps everything.
        let (kept, n) = dedup_against_posted(vec![finding("a.ts", 1, "x")], &HashSet::new());
        assert_eq!(n, 0);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn validate_anchors_in_diff_defers_unanchored_drops_out_of_scope_and_dedups() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));

        let findings = vec![
            finding("src/main.rs", 2, "on a changed line"), // anchorable → inline
            finding("src/main.rs", 2, "on a changed line"), // duplicate → dropped
            finding("src/main.rs", 99, "changed file, line not in diff"), // deferred
            finding("other.rs", 1, "file not in PR"),       // out of scope → dropped
        ];
        let review = validate(findings, &commentable);

        assert_eq!(review.inline.len(), 1, "one anchorable, deduped");
        assert_eq!(review.inline[0].path, "src/main.rs");
        assert_eq!(review.inline[0].line, 2);
        assert!(review.inline[0].body.contains("on a changed line"));
        assert_eq!(
            review.deferred.len(),
            1,
            "unanchored finding on a changed file is kept in the body"
        );
        assert_eq!(
            review.out_of_scope.len(),
            1,
            "finding on a file the PR doesn't touch is kept (surfaced), not dropped"
        );
        assert_eq!(review.out_of_scope[0].file, "other.rs");
    }

    #[test]
    fn validate_renders_suggestion_block_for_anchored_finding() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));
        let mut f = finding("src/main.rs", 2, "Fix it");
        f.suggestion = Some("    let b = 4;".into());

        let review = validate(vec![f], &commentable);
        assert_eq!(review.inline.len(), 1);
        assert!(
            review.inline[0]
                .body
                .contains("```suggestion\n    let b = 4;\n```"),
            "anchored finding renders a committable suggestion block"
        );
    }

    // A two-hunk patch (ADR-0071 range tests): hunk 1 covers new-side lines 1-3, hunk 2 covers 20-21 —
    // lines 4..19 are NOT commentable (outside any hunk), so a range spanning the gap must fail.
    const TWO_HUNK_PATCH: &str = "@@ -1,1 +1,3 @@\n let a = 1;\n+let b = 2;\n+let c = 3;\n\
         @@ -20,1 +20,2 @@\n let x = 20;\n+let y = 21;";

    #[test]
    fn validate_anchors_ranged_finding_when_fully_commentable_within_one_hunk() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));
        let mut f = finding("src/main.rs", 3, "Whole loop is wrong");
        f.start_line = Some(2); // 2..=3 fully commentable (PATCH: {1,2,3,4})

        let review = validate(vec![f], &commentable);
        assert_eq!(review.inline.len(), 1);
        assert_eq!(review.inline[0].line, 3);
        assert_eq!(
            review.inline[0].start_line,
            Some(2),
            "the range validates and anchors start_line"
        );
    }

    #[test]
    fn validate_range_crossing_hunk_boundary_falls_back_to_single_line() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(TWO_HUNK_PATCH));
        let mut f = finding("src/main.rs", 21, "Spans two hunks");
        f.start_line = Some(2); // commentable in hunk 1, but 4..19 aren't → crosses the boundary

        let review = validate(vec![f], &commentable);
        assert_eq!(
            review.inline.len(),
            1,
            "never dropped — falls back to a single-line comment"
        );
        assert_eq!(review.inline[0].line, 21);
        assert_eq!(
            review.inline[0].start_line, None,
            "range didn't validate, so no start_line is sent"
        );
    }

    #[test]
    fn validate_range_with_uncommentable_start_falls_back_to_single_line() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(TWO_HUNK_PATCH));
        let mut f = finding("src/main.rs", 21, "start_line isn't in any hunk");
        f.start_line = Some(10); // in the gap between hunks — not commentable at all

        let review = validate(vec![f], &commentable);
        assert_eq!(review.inline.len(), 1, "never dropped");
        assert_eq!(review.inline[0].line, 21);
        assert_eq!(review.inline[0].start_line, None);
    }

    #[test]
    fn validate_range_start_after_line_falls_back_to_single_line() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));
        let mut f = finding("src/main.rs", 2, "start_line > line");
        f.start_line = Some(3); // start_line > line — invalid ordering

        let review = validate(vec![f], &commentable);
        assert_eq!(review.inline.len(), 1, "never dropped");
        assert_eq!(review.inline[0].line, 2);
        assert_eq!(
            review.inline[0].start_line, None,
            "start_line > line safely falls back"
        );
    }

    #[test]
    fn validate_ranged_finding_with_suggestion_renders_correctly() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));
        let mut f = finding("src/main.rs", 3, "Replace both lines");
        f.start_line = Some(2);
        f.suggestion = Some("    let b = 4;\n    let c = 5;".into());

        let review = validate(vec![f], &commentable);
        assert_eq!(review.inline.len(), 1);
        assert_eq!(
            review.inline[0].start_line,
            Some(2),
            "range validates, so the comment posts as a range"
        );
        assert!(
            review.inline[0]
                .body
                .contains("```suggestion\n    let b = 4;\n    let c = 5;\n```"),
            "the suggestion fence carries the caller's multi-line replacement verbatim: {}",
            review.inline[0].body
        );
    }

    #[test]
    fn validate_normalizes_path_so_dotslash_still_anchors() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));

        // The model returned a `./`-prefixed path; it must still match the diff, not be dropped.
        let review = validate(vec![finding("./src/main.rs", 2, "x")], &commentable);
        assert_eq!(review.out_of_scope.len(), 0, "normalized path is in scope");
        assert_eq!(review.inline.len(), 1);
        assert_eq!(
            review.inline[0].path, "src/main.rs",
            "posted path is normalized"
        );
    }

    #[test]
    fn validate_renders_empty_suggestion_as_a_deletion() {
        let mut commentable = HashMap::new();
        commentable.insert("src/main.rs".to_string(), commentable_lines(PATCH));
        let mut f = finding("src/main.rs", 2, "Delete this");
        f.suggestion = Some(String::new()); // intentional line deletion

        let review = validate(vec![f], &commentable);
        assert!(
            review.inline[0].body.contains("```suggestion\n\n```"),
            "an empty suggestion is kept as a delete-line block"
        );
    }

    #[test]
    fn validate_unknown_scope_defers_instead_of_dropping() {
        // Empty `commentable` = we couldn't determine the change set → defer, don't drop.
        let review = validate(vec![finding("a.rs", 1, "x")], &HashMap::new());
        assert_eq!(review.out_of_scope.len(), 0);
        assert_eq!(review.deferred.len(), 1);
    }

    #[test]
    fn render_body_includes_summary_deferred_out_of_scope_section_and_disclosure() {
        let body = render_body(
            "Looks risky.",
            &[finding("a.rs", 5, "Issue")],
            &[finding("vendor/lib.rs", 9, "Unrelated nit")],
        );
        assert!(body.contains("Looks risky."));
        assert!(body.contains("Issue"));
        assert!(body.contains("`a.rs:5`"));
        // The bullet's badge row is on its own line; the title + file:line follow on the next line.
        assert!(
            body.contains("\n  **Issue** — `a.rs:5`"),
            "badges and title on separate lines in the bullet: {body}"
        );
        // Out-of-scope findings are surfaced in a collapsible section (not dropped, ADR-0033) but
        // DEMOTED — informational header, terse title + file, and crucially NO severity badge (they
        // are pre-existing, not findings on this change).
        assert!(body.contains("<details>"), "collapsible section present");
        assert!(body.contains("1 pre-existing observation(s) about code outside this PR's diff"));
        assert!(
            body.contains("Unrelated nit") && body.contains("`vendor/lib.rs`"),
            "the out-of-scope finding's title + file are recoverable"
        );
        assert!(
            !body.contains("`vendor/lib.rs:9`"),
            "out-of-scope notes carry no line anchor (the file isn't in the diff)"
        );
        assert!(
            body.contains("AI-generated review"),
            "governance disclosure"
        );
    }

    // FAST tier (ADR-0062): the body is marked as a quick pass — a blockquote banner naming the pass and
    // pointing to the deep review via the REAL App handle. A verdict, when present, follows the banner;
    // when absent the banner stands alone. Findings render exactly as in the full body.
    #[test]
    fn render_fast_body_marks_quick_pass_with_handle_and_optional_verdict() {
        // With a verdict + an out-of-scope finding.
        let body = render_fast_body(
            "lightbridge-assistant",
            Some("Looks fine; one small nit."),
            &[],
            &[finding("vendor/lib.rs", 9, "Unrelated nit")],
        );
        assert!(
            body.starts_with("> 🅵 **Fast automated pass**"),
            "leads with the quick-pass blockquote banner: {body}"
        );
        assert!(
            body.contains("mention @lightbridge-assistant on this PR"),
            "points to the deep review via the real handle: {body}"
        );
        assert!(body.contains("Looks fine; one small nit."), "verdict shown");
        assert!(
            body.contains("<details>") && body.contains("Unrelated nit"),
            "findings render like the full body"
        );
        assert!(
            body.contains("AI-generated review"),
            "governance disclosure"
        );
        assert!(
            !body.contains("## Lightbridge review"),
            "the fast pass is visually distinct from the authoritative review heading"
        );

        // No verdict (exhausted/clean pass) → the banner stands alone, no default 'No issues' verdict.
        let empty = render_fast_body("lightbridge-assistant", None, &[], &[]);
        assert!(empty.starts_with("> 🅵 **Fast automated pass**"));
        assert!(
            !empty.contains("No issues found"),
            "an empty fast pass shows the banner, not a fabricated verdict: {empty}"
        );

        // No handle configured → a graceful generic pointer, never a bare '@'.
        let no_handle = render_fast_body("", None, &[], &[]);
        assert!(no_handle.contains("mention me on this PR"), "{no_handle}");
        assert!(!no_handle.contains("@ "), "no dangling @: {no_handle}");
    }

    #[test]
    fn render_answer_body_wraps_answer_with_heading_and_disclosure() {
        let body = render_answer_body("  Use an `RwLock` for read-heavy access.  ");
        assert!(body.starts_with("## Lightbridge answer"), "headed: {body}");
        assert!(body.contains("Use an `RwLock` for read-heavy access."));
        assert!(
            !body.contains("  Use an"),
            "answer is trimmed before rendering"
        );
        assert!(
            body.contains("AI-generated answer") && body.contains("AI governance"),
            "carries the untrusted-output disclosure"
        );
    }

    #[test]
    fn render_failure_notice_is_short_actionable_and_disclosed() {
        let body = render_failure_notice();
        assert!(body.starts_with("## Lightbridge review"), "headed: {body}");
        assert!(
            body.contains("couldn't finish") && body.to_lowercase().contains("try again"),
            "says it failed + how to retry"
        );
        assert!(body.contains("AI governance"), "carries the disclosure");
    }
}
