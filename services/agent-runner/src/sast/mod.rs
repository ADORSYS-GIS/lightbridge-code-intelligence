//! SAST (static application security testing) via opengrep (ADR-0061).
//!
//! opengrep is the LGPL fork of Semgrep CE — a rules engine that finds known-bad code patterns
//! **deterministically** (same code + rules ⇒ same findings, every run, no LLM, no tokens). We run it
//! as a best-effort subprocess over the checkout whose failure is logged, never fatal (the same
//! best-effort contract as the structural-graph step). Unlike the review agent, SAST is a *deterministic* finding source —
//! its findings are posted on their own merit and are **not** gated by the LLM (ADR-0061). They flow
//! through the existing mediated-write buffer (`add_review_comment`), so the control plane validates,
//! scopes, renders, and posts them as part of the one grouped review (ADR-0037/0059) — no second poster.
//!
//! Scope: we point opengrep only at the PR's **changed files**, so a review surfaces findings on the
//! change rather than dumping every pre-existing repo finding into the out-of-scope section.
//!
//! Split by concern (quality pass, no behaviour change): [`finding`] ([`SastFinding`] + its comment
//! rendering), [`rules`] (language-scoping the ruleset to the changed files), [`process`] (spawning the
//! `opengrep` subprocess + path safety), and [`sarif`] (parsing its SARIF output). This module keeps
//! only the public orchestration: [`scan`], [`buffer`], [`digest`].

mod finding;
mod process;
mod rules;
mod sarif;

use std::path::{Path, PathBuf};

use uuid::Uuid;

pub use finding::SastFinding;
use process::is_safe_relative;

use crate::bootstrap::config::SastConfig;
use lci_agent_clients::ControlPlaneClient;

/// Run opengrep over the PR's changed files and return the normalized findings. Best-effort: any failure
/// (binary absent, scan error, timeout, unparseable output) is an `Err` the caller logs without failing
/// the task. Returns an empty vec (not an error) when there's simply nothing to scan.
///
/// `changed_files` are repo-root-relative paths from the PR diff; we filter to the ones that still exist
/// on disk (a deleted file has nothing to scan) and pass them to opengrep as explicit targets.
pub async fn scan(
    config: &SastConfig,
    checkout: &Path,
    changed_files: &[String],
) -> anyhow::Result<Vec<SastFinding>> {
    // Only scan files that exist in the checkout: deletions appear in the diff but have no tree to scan,
    // and a missing target makes opengrep error out.
    let mut targets: Vec<String> = Vec::new();
    for f in changed_files {
        // Defense-in-depth on `Path::join`: `git diff --name-only` only ever emits repo-relative paths,
        // but an absolute `f` would make `join` silently discard `checkout` (a Rust footgun) and a `..`
        // could climb out of the tree — so reject both, keeping the scan strictly inside the checkout.
        if !is_safe_relative(f) {
            tracing::warn!(path = %f, "sast: skipping non-relative/parent-escaping changed-file path");
            continue;
        }
        if checkout.join(f).is_file() {
            targets.push(f.clone());
        }
    }
    if targets.is_empty() {
        tracing::info!("sast: no existing changed files to scan; skipping opengrep");
        return Ok(Vec::new());
    }

    // Language-scope the ruleset (perf): `opengrep scan --config <dir>` LOADS every rule under the path
    // before it matches anything, so pointing at the whole multi-language tree cost ~4 min/scan even for
    // one file (observed live). Instead, pass only the rule dirs for the languages actually present in the
    // changed files — plus `generic` (language-agnostic: hardcoded secrets etc.), which is why a docs-only
    // PR still gets a cheap secrets pass rather than a full-tree load. Same findings per file (opengrep
    // only applies rules matching a file's language anyway), a fraction of the load time. A dir that
    // doesn't exist (custom ruleset, layout drift) is filtered out; if NOTHING resolves (an operator
    // override that isn't the opengrep-rules layout), fall back to the configured path as-is.
    let rules_base = Path::new(&config.rules);
    let mut config_paths: Vec<PathBuf> = rules::rule_dir_names_for_targets(&targets)
        .into_iter()
        .map(|name| rules_base.join(name))
        .filter(|p| p.is_dir())
        .collect();
    if config_paths.is_empty() {
        config_paths.push(rules_base.to_path_buf());
    }

    let sarif = process::run_opengrep(config, checkout, &targets, &config_paths).await?;
    let findings = sarif::parse_sarif(&sarif, &config.min_severity, config.max_findings);
    tracing::info!(
        findings = findings.len(),
        files = targets.len(),
        rule_sets = config_paths.len(),
        "sast: opengrep scan complete"
    );
    Ok(findings)
}

/// Buffer each SAST finding into the control plane's review buffer via the mediated `add_review_comment`
/// action (ADR-0037) — the same channel the agent uses. The control plane validates them against the
/// diff and posts them in the grouped review. Best-effort per finding: a single buffer failure is logged
/// and skipped rather than aborting the whole set.
pub async fn buffer(client: &ControlPlaneClient, task_id: Uuid, findings: &[SastFinding]) {
    for f in findings {
        let title = f.title();
        let body = f.body();
        if let Err(error) = client
            .add_review_comment(
                task_id,
                &f.file,
                f.line as i32,
                // opengrep findings are always single-line (ADR-0071 ranges are a native-agent-only
                // concept — SAST has no notion of a multi-line evidence span).
                None,
                Some(&title),
                Some(&f.priority),
                Some("security"),
                None,
                &body,
            )
            .await
        {
            tracing::warn!(%error, file = %f.file, line = f.line, "sast: buffering finding failed (non-fatal)");
        }
    }
}

/// A compact, untrusted digest of the SAST findings for injection into the review agent's prompt
/// (ADR-0061 Phase 2): the agent is made *aware* of what opengrep already flagged so it doesn't
/// redundantly re-report those lines and can choose to *deepen* a lead. It does NOT gate posting —
/// these findings are buffered and posted regardless of what the agent does. `None` when empty.
///
/// The anchoring paragraph below exists because of a production incident
/// (ADORSYS-GIS/webank-mobile#145, run `e82f7c4b-50ec-4bc4-942f-48cfc404b603`): opengrep flagged
/// `.env.fullstack:216` (a real API key), but the agent triaged a DIFFERENT, unrelated line in the same
/// file — never reading the actual flagged line — and confidently declared it a false positive while
/// the real secret went unevaluated. `review-agent`'s `SastAnchorGate` (issue #305) enforces the same
/// rule in code (rejecting a "false positive"/rule-id verdict recorded on the wrong line before
/// `finish`); this instruction is the first line of defense so the model gets it right without needing
/// the bounce.
pub fn digest(findings: &[SastFinding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Deterministic SAST findings (opengrep)\n\n\
         A static-analysis pass already flagged the lines below, and they **will be posted** to this \
         review independently of you. Do NOT re-report them as your own findings. You MAY investigate a \
         lead further — confirm exploitability, trace a tainted input, or note if one is a false \
         positive — but spend your budget on issues opengrep cannot catch.\n\n\
         **Before writing ANY confirm/refute verdict about one of these findings** — in particular \
         before calling one a false positive — call `read_file` on the EXACT `file`/line listed below \
         (e.g. `start_line` = `end_line` = that line) and quote what it actually contains in your \
         `evidence`. Reason from that real content, never from memory or a nearby/similar-looking line. \
         A verdict recorded at a different line than the one listed is wrong by construction, even if \
         the code you actually looked at happens to look similar — go back and read the exact line \
         before you record anything about it.\n",
    );
    for f in findings {
        out.push_str(&format!(
            "- [{}] {}:{} — {} (`{}`)\n",
            f.priority,
            f.file,
            f.line,
            finding::truncate(f.message.lines().next().unwrap_or("").trim(), 140),
            f.rule_id,
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_lists_findings_or_none() {
        assert!(digest(&[]).is_none());
        let f = SastFinding {
            file: "src/a.rs".into(),
            line: 9,
            rule_id: "r.id".into(),
            message: "Tainted input reaches exec.".into(),
            priority: "P1".into(),
            help_uri: None,
        };
        let d = digest(std::slice::from_ref(&f)).expect("some");
        assert!(d.contains("will be posted"));
        assert!(d.contains("src/a.rs:9"));
        assert!(d.contains("Do NOT re-report"));
    }

    // Regression for the production incident (#305): opengrep deterministically flagged one exact line
    // (a real secret at :216) in a file that also has an unrelated, un-flagged line nearby (a dev DB
    // password at :60) — the digest must list ONLY the actually-flagged coordinate and must instruct
    // the model to read that exact line (not a nearby/similar one) before triaging it.
    #[test]
    fn digest_anchors_to_the_exact_flagged_line_and_requires_reading_it() {
        let real = SastFinding {
            file: ".env.fullstack".into(),
            line: 216,
            rule_id: "generic.secrets.security.detected-generic-api-key".into(),
            message: "Hardcoded MTN Mobile Money API key.".into(),
            priority: "P1".into(),
            help_uri: None,
        };
        let d = digest(std::slice::from_ref(&real)).expect("some");
        assert!(
            d.contains(".env.fullstack:216"),
            "the real flagged coordinate is listed: {d}"
        );
        assert!(
            !d.contains(".env.fullstack:60"),
            "an un-flagged nearby line is never fabricated into the digest: {d}"
        );
        assert!(
            d.contains("read_file") && d.contains("EXACT"),
            "the digest requires reading the exact flagged line before any verdict: {d}"
        );
        assert!(
            d.to_ascii_lowercase().contains("nearby"),
            "the digest explicitly forbids reasoning about a nearby/similar line: {d}"
        );
    }
}
