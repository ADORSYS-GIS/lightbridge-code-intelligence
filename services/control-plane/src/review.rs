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
    pub line: u32,
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
/// field unset. The block is capped at [`PRIOR_BLOCK_CHAR_CAP`] with an explicit truncation marker.
pub fn format_prior_reviews(priors: &[PriorReview]) -> Option<String> {
    // Nothing useful anywhere → no block (mirrors the old single-review empty case). A prior counts as
    // content if it has a non-empty verdict OR at least one parseable finding — an empty/`[]`/malformed
    // findings blob with a blank verdict contributes nothing.
    let has_findings = |p: &PriorReview| {
        serde_json::from_value::<Vec<Finding>>(p.findings.clone())
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    };
    let any_content = priors
        .iter()
        .any(|p| !p.summary.trim().is_empty() || has_findings(p));
    if priors.is_empty() || !any_content {
        return None;
    }

    let mut out = String::from(
        "## Prior automated reviews of this pull request (context only — NOT ground truth)\n\n\
         Earlier automated passes are listed below. They may contain **false positives** — treat every \
         prior finding as an UNVERIFIED HYPOTHESIS, not a fact. **Re-derive your review from the diff \
         first**; then reconcile against these: for each prior finding, either re-derive it from the \
         code (restate it) or **explicitly retract it** as one you could not reproduce — never inherit \
         a prior finding without re-verifying it. And do not repeat a finding already posted on this \
         PR: post only what is new or changed.\n",
    );

    // The latest review (index 0) in detail; the rest compressed to one line each.
    if let Some((latest, older)) = priors.split_first() {
        out.push_str("\n### Latest prior review");
        if let Some(rest) = format_latest_detail(latest) {
            out.push_str(&rest);
        } else {
            out.push_str(" — (no verdict or findings recorded)\n");
        }

        if !older.is_empty() {
            out.push_str("\n### Earlier prior reviews (compressed)\n");
            for p in older {
                out.push_str(&compress_prior_line(p));
            }
        }
    }

    Some(cap_block(out))
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
fn cap_block(block: String) -> String {
    if block.len() <= PRIOR_BLOCK_CHAR_CAP {
        return block;
    }
    // Cut at the last newline within budget so we never sever a line mid-way.
    let cut = block[..PRIOR_BLOCK_CHAR_CAP]
        .rfind('\n')
        .unwrap_or(PRIOR_BLOCK_CHAR_CAP);
    let mut truncated = block[..cut].to_string();
    truncated.push_str(
        "\n\n… [prior-review context truncated to keep the prompt bounded — the omitted tail is older, \
         lower-signal; re-derive from the diff if relevant] …\n",
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
    pub line: u32,
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
        let anchorable = commentable
            .get(&finding.file)
            .is_some_and(|lines| lines.contains(&finding.line));
        if anchorable && finding.line > 0 {
            review.inline.push(InlineComment {
                path: finding.file.clone(),
                line: finding.line,
                body: inline_body(&finding),
            });
        } else {
            review.deferred.push(finding);
        }
    }
    review
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

fn inline_body(finding: &Finding) -> String {
    // Standardized finding format (epic #89): badge row → titled finding → explanation → committable
    // suggestion → resources. The badges sit on their OWN line above the bold title (a single newline,
    // which GitHub renders as a line break in comments) so the level reads as a header, not a prefix
    // crowding the title.
    let mut body = format!(
        "{}\n**{}**\n\n{}",
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
    // A finding as a bullet whose first line is the badge row, with the bold title + `file:line` on the
    // next (indented) line and the body under that — so the badges never share a line with the title,
    // matching the inline rendering. The 2-space indent keeps the continuation lines inside the list
    // item (Gemini #153). Shared by the changed-files notes and the out-of-scope section.
    let render_finding = |body: &mut String, f: &Finding| {
        body.push_str(&format!(
            "\n- {}\n  **{}** — `{}:{}`\n  {}",
            f.level_badges(),
            strip_model_artifacts(&f.title),
            f.file,
            f.line,
            // Indent continuation lines so a multi-line body stays inside the list item (Gemini #153).
            strip_model_artifacts(&f.body).replace('\n', "\n  ")
        ));
        for link in f.resources.iter().filter(|r| !r.trim().is_empty()) {
            body.push_str(&format!("\n  - {link}"));
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
        // The badge row sits on its own line, with the bold title on the next line (not crowding it).
        assert!(
            body.contains(")\n**Null deref**"),
            "badges and title on separate lines: {body}"
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
        assert!(
            block.contains("do not repeat a finding already posted"),
            "dedup-awareness in the prompt: {block}"
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
    fn format_prior_reviews_truncates_with_explicit_marker() {
        // Many older reviews with long titles blow past the char cap → an explicit truncation marker.
        let big_title = "x".repeat(400);
        let mut priors = vec![prior(60, "latest", vec![finding("a.ts", 1, "leak")])];
        for i in 1..=59 {
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
            block.contains("prior-review context truncated"),
            "truncation is noted explicitly, not silent"
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
