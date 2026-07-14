//! The `review-variance` subcommand (issue #420): measure whether the SAME finding across repeat
//! deep-tier reviews of an unchanged commit gets the same severity (issue #285 — deep-tier P0/P1/P2
//! classification flipped between two same-commit re-reviews with no code change).
//!
//! Rather than rebuilding the review pipeline (no codegraph/vector/SAST backing, and
//! `services/control-plane` has no `[lib]` target to reuse its `dedup_key`), this reads the reviews a
//! real deep-tier run already posts to GitHub: `fetch` shells out to `gh api` for one or more review
//! ids and writes one JSON run-file per review; `analyze` groups the findings across those run-files
//! and reports severity variance + a same-file listing for manual anchor-drift triage.
//!
//! Triggering the K repeat runs this measures needs no new tooling — it's the same `@mention`
//! re-review mechanism that produced #285's own evidence (`ADORSYS-GIS/webank-mobile#145`, reviews
//! `4643335072`/`4644473735`): re-mention the bot on the same commit, note the resulting review ids
//! from the PR timeline, then `fetch` + `analyze` them.
//!
//! Matching is intentionally two-tier, and deliberately does NOT attempt fuzzy/semantic matching
//! across a drifted anchor or a reworded title (out of scope here, same as #421's own scoping):
//! - **Exact match** — [`dedup_key`] (duplicated from `services/control-plane/src/review.rs:353-361`,
//!   ADR-0065's own definition of "the same finding": normalized `(file, line, title)`). This is the
//!   primary, fully mechanical severity-variance signal.
//! - **Same-file candidates** — findings that did NOT exact-match, grouped by file only, for a human
//!   to eyeball. #285 found the real motivating flip crossed BOTH file and line (a bug's anchor moved
//!   from the repository layer to the service layer between runs) — no mechanical key survives that,
//!   so this tool surfaces the raw material rather than overclaiming an automated match.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Fetch posted review comments for one or more review ids and write one JSON run-file per review.
    Fetch {
        /// `owner/repo`, e.g. `ADORSYS-GIS/webank-mobile`.
        #[arg(long)]
        repo: String,
        /// Pull request number the reviews were posted on.
        #[arg(long)]
        pr: u64,
        /// A review id to fetch (repeat once per re-review run, e.g. `--review 1 --review 2`).
        #[arg(long = "review", required = true)]
        reviews: Vec<u64>,
        /// Directory to write `{review_id}.json` into (created if missing).
        #[arg(long = "out-dir")]
        out_dir: PathBuf,
    },
    /// Compute severity-variance across two or more fetched run files.
    Analyze {
        /// A run JSON file written by `fetch` (repeat to compare more than one, e.g.
        /// `--input a.json --input b.json`). At least 2 are required.
        #[arg(long = "input", required = true)]
        inputs: Vec<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    Text,
    Json,
}

pub(crate) fn run(action: Action) -> anyhow::Result<()> {
    match action {
        Action::Fetch {
            repo,
            pr,
            reviews,
            out_dir,
        } => fetch(&repo, pr, &reviews, &out_dir),
        Action::Analyze { inputs, format } => analyze(&inputs, format),
    }
}

// ---------------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------------

/// One finding as GitHub actually rendered it (control-plane's `inline_body`,
/// `services/control-plane/src/review.rs:634-653`). `file`/`line` are GitHub's own structured
/// comment fields (`path`/`original_line`); `title`/`priority`/`category` are parsed from the
/// comment body's fixed badge+heading shape — see [`parse_inline_body`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunFinding {
    pub file: String,
    pub line: u32,
    pub title: String,
    pub priority: String,
    pub category: String,
}

/// One fetched run: the review's target commit + the findings it posted. ADR-0065 dedup is
/// same-`head_sha`-scoped, so `commit_id` matters for interpreting the report — see
/// [`warn_on_commit_mismatch`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Run {
    pub review_id: u64,
    pub commit_id: String,
    pub findings: Vec<RunFinding>,
}

// ---------------------------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------------------------

fn fetch(repo: &str, pr: u64, reviews: &[u64], out_dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    for &review_id in reviews {
        let run = fetch_one(repo, pr, review_id)?;
        let path = out_dir.join(format!("{review_id}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&run)?)
            .with_context(|| format!("writing {}", path.display()))?;
        println!(
            "wrote {} ({} findings, commit {})",
            path.display(),
            run.findings.len(),
            run.commit_id
        );
    }
    Ok(())
}

fn fetch_one(repo: &str, pr: u64, review_id: u64) -> anyhow::Result<Run> {
    let review = gh_api(&format!("repos/{repo}/pulls/{pr}/reviews/{review_id}"))?;
    let commit_id = review
        .get("commit_id")
        .and_then(|v| v.as_str())
        .with_context(|| format!("review {review_id} response has no `commit_id`"))?
        .to_string();

    let comments = gh_api_array(&format!(
        "repos/{repo}/pulls/{pr}/reviews/{review_id}/comments",
        // Reviews carry at most a few dozen inline findings — one page comfortably covers it
        // without needing to handle `gh api --paginate`'s multi-page output shape.
    ))?;
    let total = comments.len();
    let findings: Vec<RunFinding> = comments.iter().filter_map(parse_comment).collect();
    let skipped = total - findings.len();
    if skipped > 0 {
        eprintln!(
            "review {review_id}: skipped {skipped} comment(s) that weren't a Lightbridge finding \
             (human replies, other bots, etc.)"
        );
    }
    Ok(Run {
        review_id,
        commit_id,
        findings,
    })
}

/// Parse one GitHub review-comment API object into a [`RunFinding`], or `None` when it isn't a
/// Lightbridge finding comment (a human reply, a different bot, etc.) — `file`/`line` come from
/// GitHub's own structured fields (`original_line` — the position AT REVIEW TIME — falling back to
/// `line`, which GitHub nulls out once a later commit makes the comment's line stale). On an aged PR
/// GitHub can null out BOTH (verified live against `webank-mobile#145`'s own review comments — enough
/// later commits landed that neither survived), in which case [`line_from_diff_hunk`] reconstructs it
/// from `original_position`/`position` + the comment's own `diff_hunk`, exactly as GitHub itself
/// defines `position` — so this still recovers the line AS OF the original review, not today's HEAD.
fn parse_comment(comment: &serde_json::Value) -> Option<RunFinding> {
    let file = comment.get("path")?.as_str()?.to_string();
    let line = comment
        .get("original_line")
        .and_then(|v| v.as_u64())
        .or_else(|| comment.get("line").and_then(|v| v.as_u64()))
        .or_else(|| {
            let position = comment
                .get("original_position")
                .and_then(|v| v.as_u64())
                .or_else(|| comment.get("position").and_then(|v| v.as_u64()))?;
            let hunk = comment.get("diff_hunk")?.as_str()?;
            line_from_diff_hunk(hunk, position).map(u64::from)
        })?;
    let body = comment.get("body")?.as_str()?;
    let (priority, category, title) = parse_inline_body(body)?;
    Some(RunFinding {
        file,
        line: line as u32,
        title,
        priority,
        category,
    })
}

/// Recover a file line number from a review comment's diff-relative `position` (GitHub's own
/// definition: a 1-indexed count of lines in `diff_hunk` after its `@@` header, up to and including
/// the commented line) — used only when `line`/`original_line` are both null (see [`parse_comment`]).
/// Walks the hunk exactly as GitHub counts it, so it recovers the same line GitHub itself would have
/// reported before the position went stale. Returns `None` for a position landing on a pure deletion
/// (`-` line) — a finding can only anchor to an added/context line, so that path is unreachable for a
/// real Lightbridge comment and exists only for completeness.
fn line_from_diff_hunk(diff_hunk: &str, position: u64) -> Option<u32> {
    let mut lines = diff_hunk.lines();
    let new_start = parse_hunk_new_start(lines.next()?)?;
    let mut new_line = i64::from(new_start) - 1;
    for (index, line) in lines.enumerate() {
        let position_in_hunk = (index + 1) as u64;
        match line.as_bytes().first() {
            Some(b'-') => {
                if position_in_hunk == position {
                    return None;
                }
            }
            // `\ No newline at end of file` — a boundary marker, not new-file content. GitHub's
            // `position` still counts it as one hunk line (the enumerate() above already does that
            // unconditionally), but it must not advance `new_line`, or every line after it in the
            // hunk resolves one line too high.
            Some(b'\\') => {
                if position_in_hunk == position {
                    return None;
                }
            }
            _ => {
                new_line += 1;
                if position_in_hunk == position {
                    return u32::try_from(new_line).ok();
                }
            }
        }
    }
    None
}

/// The new-file start line out of a `@@ -oldStart[,oldLines] +newStart[,newLines] @@` hunk header.
fn parse_hunk_new_start(header: &str) -> Option<u32> {
    header
        .split('+')
        .nth(1)?
        .split_whitespace()
        .next()?
        .split(',')
        .next()?
        .parse()
        .ok()
}

/// Parse control-plane's fixed `inline_body` shape (`services/control-plane/src/review.rs:634-653`):
/// a badge-alt-text line (`![P1](...) ![correctness](...)`), a blank line, then a `**Title**` line.
/// Only these three fields are pulled from free text; `file`/`line` never are (see [`parse_comment`]).
fn parse_inline_body(body: &str) -> Option<(String, String, String)> {
    let mut lines = body.lines();
    let badge_line = lines.next()?;
    let priority = badge_alt_text(badge_line, 0)?;
    let category = badge_alt_text(badge_line, 1)?;
    let title_line = lines.find(|line| !line.trim().is_empty())?;
    let title = title_line
        .trim()
        .strip_prefix("**")?
        .strip_suffix("**")?
        .to_string();
    Some((priority, category, title))
}

/// The `index`-th (0-based) `![alt](...)` image's alt text on one line.
fn badge_alt_text(line: &str, index: usize) -> Option<String> {
    line.split("![")
        .nth(index + 1)?
        .split(']')
        .next()
        .map(str::to_string)
}

fn gh_api(path: &str) -> anyhow::Result<serde_json::Value> {
    let output = Command::new("gh")
        .args(["api", path])
        .output()
        .with_context(|| {
            format!("running `gh api {path}` — is the `gh` CLI installed and authenticated?")
        })?;
    if !output.status.success() {
        bail!(
            "`gh api {path}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing JSON from `gh api {path}`"))
}

/// Fetch every page of a GitHub list endpoint and concatenate them. `gh api` alone defaults to a
/// 30-item first page — for this tool's entire purpose (comparing findings across runs), silently
/// truncating a review with more findings than that would corrupt the variance report rather than
/// error out, so this loops on `page`/`per_page=100` (the API's max) until a short page ends it.
/// (Deliberately not `gh api --paginate`: without `--slurp` it prints each page as its own top-level
/// JSON array back to back, which `serde_json::from_slice` can't parse as one document — a real fix
/// needs either that flag pair or manual paging; this takes manual paging to keep `gh_api` itself
/// simple and single-purpose.)
fn gh_api_array(path: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut items = Vec::new();
    let mut page = 1u32;
    let separator = if path.contains('?') { '&' } else { '?' };
    loop {
        let page_path = format!("{path}{separator}per_page=100&page={page}");
        let batch = match gh_api(&page_path)? {
            serde_json::Value::Array(items) => items,
            other => bail!("expected a JSON array from `gh api {page_path}`, got {other}"),
        };
        let batch_len = batch.len();
        items.extend(batch);
        if batch_len < 100 {
            break;
        }
        page += 1;
    }
    Ok(items)
}

// ---------------------------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------------------------

/// Normalized dedup key for a finding — duplicated from
/// `services/control-plane/src/review.rs::dedup_key` (ADR-0065's own definition of "the same
/// finding": normalized path, exact line, whitespace-collapsed + case-folded title). Kept as a
/// standalone pure function rather than a dependency on `control-plane` (a bin-only crate with no
/// `[lib]` target, pulling sqlx/kube/neo4rs) — must stay in sync with the source of truth above if
/// that normalization ever changes.
fn dedup_key(file: &str, line: u32, title: &str) -> (String, u32, String) {
    let file = normalize_path(file);
    let title = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (file, line, title)
}

/// Mirrors `services/control-plane/src/review.rs::normalize_path`.
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

#[derive(Debug, Clone, Serialize)]
struct Occurrence {
    run_index: usize,
    review_id: u64,
    line: u32,
    title: String,
    priority: String,
}

#[derive(Debug, Clone, Serialize)]
struct FindingGroup {
    file: String,
    occurrences: Vec<Occurrence>,
}

#[derive(Debug, Serialize)]
struct VarianceReport {
    review_ids: Vec<u64>,
    total_runs: usize,
    total_findings: usize,
    /// Findings that appear in exactly one run — new coverage, not a stability signal (the ticket's
    /// own scoping: coverage differing between runs is expected and fine).
    single_run_only: usize,
    /// Findings that exact-key-matched (>= 2 distinct runs, same normalized file/line/title).
    exact_matches: usize,
    /// Of `exact_matches`, how many had the SAME severity in every run that made them.
    stable_matches: usize,
    /// Of `exact_matches`, the ones with >1 distinct severity — the metric this ticket exists for.
    severity_flips: Vec<FindingGroup>,
    /// Findings that did NOT exact-match, grouped by file, for files touched by >= 2 distinct runs —
    /// candidates for manual anchor-drift/rewording triage (see the module doc for why this can't be
    /// automated further).
    same_file_candidates: Vec<FindingGroup>,
}

fn analyze(inputs: &[PathBuf], format: OutputFormat) -> anyhow::Result<()> {
    if inputs.len() < 2 {
        bail!(
            "analyze needs at least 2 --input run files to compare (got {})",
            inputs.len()
        );
    }
    let runs: Vec<Run> = inputs
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        })
        .collect::<anyhow::Result<_>>()?;

    warn_on_commit_mismatch(&runs);
    let report = build_report(&runs);

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => print_report(&report),
    }
    Ok(())
}

fn warn_on_commit_mismatch(runs: &[Run]) {
    let commits: BTreeSet<&str> = runs.iter().map(|r| r.commit_id.as_str()).collect();
    if commits.len() > 1 {
        eprintln!(
            "WARNING: input runs target different commits ({}) — anchor drift and severity \
             differences may be explained by real code changes, not run-to-run variance. ADR-0065 \
             dedup is scoped to a single head_sha for exactly this reason.",
            commits.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
}

fn build_report(runs: &[Run]) -> VarianceReport {
    let mut groups: BTreeMap<(String, u32, String), BTreeMap<usize, RunFinding>> = BTreeMap::new();
    let mut total_findings = 0usize;
    for (run_index, run) in runs.iter().enumerate() {
        for finding in &run.findings {
            total_findings += 1;
            let key = dedup_key(&finding.file, finding.line, &finding.title);
            groups
                .entry(key)
                .or_default()
                .insert(run_index, finding.clone());
        }
    }

    let mut single_run_only = 0usize;
    let mut stable_matches = 0usize;
    let mut severity_flips = Vec::new();
    let mut matched_instances: BTreeSet<(usize, (String, u32, String))> = BTreeSet::new();

    for (key, by_run) in &groups {
        if by_run.len() < 2 {
            single_run_only += 1;
            continue;
        }
        for &run_index in by_run.keys() {
            matched_instances.insert((run_index, key.clone()));
        }
        let distinct_priorities: BTreeSet<&str> =
            by_run.values().map(|f| f.priority.as_str()).collect();
        let group = FindingGroup {
            file: key.0.clone(),
            occurrences: by_run
                .iter()
                .map(|(&run_index, f)| Occurrence {
                    run_index,
                    review_id: runs[run_index].review_id,
                    line: f.line,
                    title: f.title.clone(),
                    priority: f.priority.clone(),
                })
                .collect(),
        };
        if distinct_priorities.len() > 1 {
            severity_flips.push(group);
        } else {
            stable_matches += 1;
        }
    }

    let mut by_file: BTreeMap<String, Vec<Occurrence>> = BTreeMap::new();
    for (run_index, run) in runs.iter().enumerate() {
        for finding in &run.findings {
            let key = dedup_key(&finding.file, finding.line, &finding.title);
            if matched_instances.contains(&(run_index, key.clone())) {
                continue;
            }
            by_file.entry(key.0).or_default().push(Occurrence {
                run_index,
                review_id: run.review_id,
                line: finding.line,
                title: finding.title.clone(),
                priority: finding.priority.clone(),
            });
        }
    }
    let same_file_candidates: Vec<FindingGroup> = by_file
        .into_iter()
        .filter(|(_, occurrences)| {
            occurrences
                .iter()
                .map(|o| o.run_index)
                .collect::<BTreeSet<_>>()
                .len()
                >= 2
        })
        .map(|(file, occurrences)| FindingGroup { file, occurrences })
        .collect();

    VarianceReport {
        review_ids: runs.iter().map(|r| r.review_id).collect(),
        total_runs: runs.len(),
        total_findings,
        single_run_only,
        exact_matches: stable_matches + severity_flips.len(),
        stable_matches,
        severity_flips,
        same_file_candidates,
    }
}

fn print_report(report: &VarianceReport) {
    println!("== Deep-tier review variance report ==");
    println!(
        "Runs analyzed: {} (reviews: {})",
        report.total_runs,
        report
            .review_ids
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    println!("Total findings across all runs: {}", report.total_findings);
    println!(
        "  appearing in exactly 1 run (new coverage, not a flip): {}",
        report.single_run_only
    );
    println!(
        "  appearing in >=2 runs at the exact same (file, line, title): {}",
        report.exact_matches
    );
    println!("    same severity every time: {}", report.stable_matches);
    println!(
        "    SEVERITY VARIANCE (flip): {}",
        report.severity_flips.len()
    );
    println!();
    print_groups(
        "SEVERITY FLIPS",
        "No severity flips found among exact-key matches.",
        &report.severity_flips,
    );
    println!();
    print_groups(
        "SAME-FILE CANDIDATES (not exact-matched — review manually for anchor drift / rewording)",
        "No same-file candidates outside the exact matches.",
        &report.same_file_candidates,
    );
}

fn print_groups(heading: &str, empty_message: &str, groups: &[FindingGroup]) {
    if groups.is_empty() {
        println!("{empty_message}");
        return;
    }
    println!("{heading}:");
    for group in groups {
        println!("  - {}", group.file);
        for occ in &group.occurrences {
            println!(
                "      run {} (review {}): {} — line {} — \"{}\"",
                occ.run_index, occ.review_id, occ.priority, occ.line, occ.title
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, line: u32, title: &str, priority: &str) -> RunFinding {
        RunFinding {
            file: file.to_string(),
            line,
            title: title.to_string(),
            priority: priority.to_string(),
            category: "correctness".to_string(),
        }
    }

    fn run(review_id: u64, commit_id: &str, findings: Vec<RunFinding>) -> Run {
        Run {
            review_id,
            commit_id: commit_id.to_string(),
            findings,
        }
    }

    // --- dedup_key -------------------------------------------------------------------------

    #[test]
    fn dedup_key_normalizes_whitespace_case_and_leading_path_segments() {
        assert_eq!(
            dedup_key("./src/store.ts", 65, "indexeddb   connection LEAK in tx()"),
            dedup_key("src/store.ts", 65, "IndexedDB connection leak in tx()"),
        );
        assert_ne!(
            dedup_key("src/store.ts", 65, "a"),
            dedup_key("src/store.ts", 66, "a"),
            "line is part of the identity, not normalized away"
        );
    }

    // --- parse_inline_body / parse_comment --------------------------------------------------

    // Captured live via `gh api repos/ADORSYS-GIS/webank-mobile/pulls/145/reviews/4643335072/comments`
    // during the #285 investigation — a real-world fixture, not a synthetic one.
    const REAL_INLINE_BODY: &str = "![P1](https://img.shields.io/badge/P1-orange) ![correctness](https://img.shields.io/badge/correctness-blue)\n**Phone hash mismatch breaks auto-claim on KYC1**\n\n`hashPhone` in repository.go computes `sha256.Sum256([]byte(phone))` on the raw string — no normalization.\n\n**Evidence:** repository.go: hashPhone = sha256.Sum256([]byte(phone)) — no normalization.\n\n> Was this useful? React \u{1F44D}/\u{1F44E} to give us feedback";

    #[test]
    fn parse_inline_body_reads_a_real_posted_finding() {
        let (priority, category, title) = parse_inline_body(REAL_INLINE_BODY).unwrap();
        assert_eq!(priority, "P1");
        assert_eq!(category, "correctness");
        assert_eq!(title, "Phone hash mismatch breaks auto-claim on KYC1");
    }

    #[test]
    fn parse_inline_body_ignores_bold_text_later_in_the_body() {
        // The real fixture's own "**Evidence:**" line proves the parser only reads the FIRST
        // non-blank line for the title, not any bold span in the body.
        let (_, _, title) = parse_inline_body(REAL_INLINE_BODY).unwrap();
        assert_ne!(title, "Evidence:");
    }

    #[test]
    fn parse_comment_falls_back_to_line_when_original_line_is_null() {
        let comment = serde_json::json!({
            "path": "a.rs",
            "line": 42,
            "original_line": null,
            "body": REAL_INLINE_BODY,
        });
        let finding = parse_comment(&comment).unwrap();
        assert_eq!(finding.line, 42);
        assert_eq!(finding.file, "a.rs");
    }

    #[test]
    fn parse_comment_prefers_original_line_over_line() {
        // GitHub nulls `line` once a later commit makes a comment's position stale, but
        // `original_line` (the position AT REVIEW TIME) survives — that's what a same-commit
        // variance comparison needs.
        let comment = serde_json::json!({
            "path": "a.rs",
            "line": serde_json::Value::Null,
            "original_line": 7,
            "body": REAL_INLINE_BODY,
        });
        assert_eq!(parse_comment(&comment).unwrap().line, 7);
    }

    #[test]
    fn parse_comment_returns_none_for_a_non_finding_comment() {
        let comment = serde_json::json!({
            "path": "a.rs",
            "line": 1,
            "body": "Thanks for the fix!",
        });
        assert!(parse_comment(&comment).is_none());
    }

    // --- line_from_diff_hunk -----------------------------------------------------------------

    #[test]
    fn line_from_diff_hunk_resolves_an_added_line_in_a_brand_new_file() {
        // A new-file hunk (`-0,0`): every hunk line is an addition, so hunk position N is new-file
        // line N (new_start = 1). Mirrors the real webank-mobile#145 case (a 999-line new file,
        // `original_position: 613` → `repository.go:613`, matched against the review's own prose).
        let hunk = "@@ -0,0 +1,5 @@\n+line 1\n+line 2\n+line 3\n+line 4\n+line 5";
        assert_eq!(line_from_diff_hunk(hunk, 3), Some(3));
        assert_eq!(line_from_diff_hunk(hunk, 5), Some(5));
    }

    #[test]
    fn line_from_diff_hunk_counts_context_lines_toward_the_new_side() {
        let hunk = "@@ -10,4 +10,5 @@\n context a\n+added\n context b\n context c";
        // position 1 = "context a" (new line 10), position 2 = "added" (new line 11),
        // position 3 = "context b" (new line 12).
        assert_eq!(line_from_diff_hunk(hunk, 1), Some(10));
        assert_eq!(line_from_diff_hunk(hunk, 2), Some(11));
        assert_eq!(line_from_diff_hunk(hunk, 3), Some(12));
    }

    #[test]
    fn line_from_diff_hunk_returns_none_for_a_pure_deletion_position() {
        let hunk = "@@ -1,2 +1,1 @@\n-removed\n context";
        assert_eq!(line_from_diff_hunk(hunk, 1), None);
    }

    #[test]
    fn line_from_diff_hunk_does_not_advance_new_line_on_the_no_newline_marker() {
        // Caught by lightbridge-assistant on PR #422: the `\ No newline at end of file` marker
        // starts with `\`, not `-`/`+`/` `, and must not count as new-file content. Without the
        // dedicated match arm, position 4 ("+new") would resolve to 12 instead of 11.
        let hunk = "@@ -10,2 +10,2 @@\n context\n-old\n\\ No newline at end of file\n+new";
        assert_eq!(line_from_diff_hunk(hunk, 1), Some(10)); // " context"
        assert_eq!(line_from_diff_hunk(hunk, 2), None); // "-old"
        assert_eq!(line_from_diff_hunk(hunk, 3), None); // "\ No newline..." — not content
        assert_eq!(line_from_diff_hunk(hunk, 4), Some(11)); // "+new" — NOT 12
    }

    #[test]
    fn parse_comment_falls_back_to_diff_hunk_when_line_and_original_line_are_both_null() {
        // The real shape observed on webank-mobile#145's own (aged) review comments: enough later
        // commits landed that GitHub nulled both position fields, leaving only `original_position` +
        // `diff_hunk` to recover the line as of the original review.
        let comment = serde_json::json!({
            "path": "repository.go",
            "line": null,
            "original_line": null,
            "position": null,
            "original_position": 3,
            "diff_hunk": "@@ -0,0 +1,5 @@\n+line 1\n+line 2\n+line 3",
            "body": REAL_INLINE_BODY,
        });
        let finding = parse_comment(&comment).unwrap();
        assert_eq!(finding.line, 3);
        assert_eq!(finding.file, "repository.go");
    }

    // --- build_report ------------------------------------------------------------------------

    #[test]
    fn build_report_flags_a_true_exact_key_severity_flip() {
        let runs = vec![
            run(
                1,
                "c1",
                vec![finding("a.rs", 10, "SQL injection via string concat", "P1")],
            ),
            run(
                2,
                "c1",
                vec![finding("a.rs", 10, "SQL injection via string concat", "P2")],
            ),
        ];
        let report = build_report(&runs);
        assert_eq!(report.severity_flips.len(), 1);
        assert_eq!(report.severity_flips[0].file, "a.rs");
        let priorities: BTreeSet<&str> = report.severity_flips[0]
            .occurrences
            .iter()
            .map(|o| o.priority.as_str())
            .collect();
        assert_eq!(priorities, BTreeSet::from(["P1", "P2"]));
        assert_eq!(report.stable_matches, 0);
        assert_eq!(report.single_run_only, 0);
    }

    #[test]
    fn build_report_treats_whitespace_and_case_drift_as_the_same_key() {
        let runs = vec![
            run(1, "c1", vec![finding("a.rs", 10, "Foo   Bar", "P1")]),
            run(2, "c1", vec![finding("a.rs", 10, "foo bar", "P2")]),
        ];
        let report = build_report(&runs);
        assert_eq!(
            report.severity_flips.len(),
            1,
            "trivial rephrasing must still exact-match, same as ADR-0065's own dedup"
        );
    }

    #[test]
    fn build_report_does_not_flag_a_stable_exact_match() {
        let runs = vec![
            run(1, "c1", vec![finding("a.rs", 10, "same issue", "P1")]),
            run(2, "c1", vec![finding("a.rs", 10, "same issue", "P1")]),
        ];
        let report = build_report(&runs);
        assert!(report.severity_flips.is_empty());
        assert_eq!(report.stable_matches, 1);
    }

    #[test]
    fn build_report_counts_single_run_findings_as_new_coverage_not_a_flip() {
        let runs = vec![
            run(
                1,
                "c1",
                vec![finding("a.rs", 10, "only run 1 found this", "P1")],
            ),
            run(2, "c1", vec![]),
        ];
        let report = build_report(&runs);
        assert_eq!(report.single_run_only, 1);
        assert!(report.severity_flips.is_empty());
        assert!(report.same_file_candidates.is_empty());
    }

    #[test]
    fn build_report_surfaces_same_file_anchor_drift_without_auto_flagging_severity() {
        // Reproduces #285's "ReconcilePendingIntents" evidence: same file, same title, DIFFERENT
        // line, SAME severity both times — exact-key matching can't see it (line is part of the
        // key), so it must land in same-file candidates, not severity_flips, and must not count
        // as a stable exact match either (it never exact-matched in the first place).
        let runs = vec![
            run(
                1,
                "c1",
                vec![finding(
                    "service.go",
                    367,
                    "ReconcilePendingIntents can falsely mark debited rows as FAILED",
                    "P1",
                )],
            ),
            run(
                2,
                "c1",
                vec![finding(
                    "service.go",
                    175,
                    "ReconcilePendingIntents can falsely mark debited rows as FAILED",
                    "P1",
                )],
            ),
        ];
        let report = build_report(&runs);
        assert!(report.severity_flips.is_empty());
        assert_eq!(report.stable_matches, 0);
        // Neither finding exact-key-matches the other (the line differs), so each independently
        // counts as single_run_only — that count is per EXACT KEY, not per real-world issue; the
        // same-file bucket below is what surfaces them together for a human.
        assert_eq!(report.single_run_only, 2);
        assert_eq!(report.same_file_candidates.len(), 1);
        assert_eq!(report.same_file_candidates[0].file, "service.go");
        assert_eq!(report.same_file_candidates[0].occurrences.len(), 2);
    }

    #[test]
    fn build_report_cannot_link_a_flip_that_also_crosses_files() {
        // The real #285 flip: "Cancel refund failure leaves orphaned row" (repository.go:312, P1)
        // vs "cancel refund failure orphan path" (service.go:691, P2) — same underlying bug class,
        // but a DIFFERENT file, line, and title in each run. No mechanical key here (exact-match or
        // same-file) can link these; this test documents that known limitation (see the module doc)
        // rather than silently gliding past it. A run's OTHER finding in service.go (the
        // ReconcilePendingIntents pair) still surfaces normally — this case just can't ride along
        // with it.
        let runs = vec![
            run(
                1,
                "c1",
                vec![
                    finding(
                        "repository.go",
                        312,
                        "Cancel refund failure leaves orphaned row",
                        "P1",
                    ),
                    finding(
                        "service.go",
                        367,
                        "ReconcilePendingIntents can falsely mark debited rows as FAILED",
                        "P1",
                    ),
                ],
            ),
            run(
                2,
                "c1",
                vec![
                    finding("service.go", 691, "cancel refund failure orphan path", "P2"),
                    finding(
                        "service.go",
                        175,
                        "ReconcilePendingIntents can falsely mark debited rows as FAILED",
                        "P1",
                    ),
                ],
            ),
        ];
        let report = build_report(&runs);
        assert!(
            report.severity_flips.is_empty(),
            "a file+line+title-crossing flip cannot exact-match by construction"
        );
        // All 4 findings have a unique exact key (no two share file+line+title), so all 4 count as
        // single_run_only — that count is per exact key, not per real-world issue. repository.go:312
        // is additionally alone in its FILE too (no other run touches repository.go at all), which is
        // exactly why it's the one entry this tool cannot surface anywhere below.
        assert_eq!(report.single_run_only, 4);
        // service.go still surfaces as one same-file group with ALL THREE unmatched occurrences —
        // a human reading it sees the P1/P1 ReconcilePendingIntents pair AND the lone P2 "orphan
        // path" entry side by side, even though the tool can't prove they're the same issue as
        // repository.go:312 on its own.
        assert_eq!(report.same_file_candidates.len(), 1);
        assert_eq!(report.same_file_candidates[0].file, "service.go");
        assert_eq!(report.same_file_candidates[0].occurrences.len(), 3);
    }

    #[test]
    fn warn_on_commit_mismatch_does_not_panic_on_matching_or_differing_commits() {
        warn_on_commit_mismatch(&[run(1, "c1", vec![]), run(2, "c1", vec![])]);
        warn_on_commit_mismatch(&[run(1, "c1", vec![]), run(2, "c2", vec![])]);
    }
}
