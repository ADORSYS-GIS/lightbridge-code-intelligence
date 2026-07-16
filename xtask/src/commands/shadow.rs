//! Shadow parity tooling for the OpenCode review cutover (RFC-0009 slice 4).
//!
//! The cutover's go/no-go gate is: does the OpenCode review find the same issues the native review
//! finds on the SAME PR? The dual RUN is inherently env-gated (needs eaig + the control plane + a real
//! PR) — see `integrations/opencode/sim/SHADOW.md` for the procedure. This subcommand is the ANALYSIS
//! half: feed it each engine's findings as JSON and it reports matched / only-native / only-opencode
//! plus a verdict, so parity is measured, not asserted.
//!
//! The load-bearing number is **only-native** — findings the native reviewer caught that OpenCode
//! MISSED. Those are the regression the cutover must not ship. `only-opencode` (new findings) is
//! reported but not failed: it may be signal or noise, a human call.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One inline review finding, as buffered control-plane-side. Field names are lenient (`file`/`path`,
/// optional severity/category/title) so either engine's exported JSON deserializes.
#[derive(Debug, Clone, Deserialize)]
struct Finding {
    #[serde(alias = "path")]
    file: String,
    line: i64,
    #[serde(default, alias = "severity")]
    priority: String,
    #[serde(default)]
    #[allow(dead_code)]
    category: String,
    #[serde(default)]
    title: String,
}

/// A native finding paired with the OpenCode finding that matched it (same file, line within
/// tolerance). `severity_diverged` flags a priority mismatch on an otherwise-matched issue.
#[derive(Debug)]
struct Matched {
    file: String,
    line: i64,
    native_priority: String,
    opencode_priority: String,
    severity_diverged: bool,
}

#[derive(Debug, Default)]
struct Report {
    matched: Vec<Matched>,
    only_native: Vec<Finding>,
    only_opencode: Vec<Finding>,
}

/// Two findings are "the same issue" when they anchor to the same file within `line_tol` lines. Title
/// text isn't required to match — the two engines phrase findings differently; the anchor is the
/// stable identity.
fn same_issue(a: &Finding, b: &Finding, line_tol: i64) -> bool {
    a.file == b.file && (a.line - b.line).abs() <= line_tol
}

/// Greedily pair each native finding with an as-yet-unmatched OpenCode finding at the same anchor.
/// Leftovers on each side are the divergence.
fn compare(native: &[Finding], opencode: &[Finding], line_tol: i64) -> Report {
    let mut report = Report::default();
    let mut used = vec![false; opencode.len()];
    for finding in native {
        let hit = opencode
            .iter()
            .enumerate()
            .find(|(idx, candidate)| !used[*idx] && same_issue(finding, candidate, line_tol));
        match hit {
            Some((idx, candidate)) => {
                used[idx] = true;
                report.matched.push(Matched {
                    file: finding.file.clone(),
                    line: finding.line,
                    native_priority: finding.priority.clone(),
                    opencode_priority: candidate.priority.clone(),
                    severity_diverged: !finding.priority.is_empty()
                        && !candidate.priority.is_empty()
                        && finding.priority != candidate.priority,
                });
            }
            None => report.only_native.push(finding.clone()),
        }
    }
    for (idx, finding) in opencode.iter().enumerate() {
        if !used[idx] {
            report.only_opencode.push(finding.clone());
        }
    }
    report
}

/// Accept either a bare JSON array of findings or an object with a `findings` array.
fn load_findings(path: &PathBuf) -> Result<Vec<Finding>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    let array = value.get("findings").cloned().unwrap_or(value);
    serde_json::from_value(array)
        .with_context(|| format!("{} is not a findings array", path.display()))
}

#[derive(clap::Subcommand)]
pub enum Action {
    /// Diff two engines' findings JSON and report parity (exit 1 if OpenCode missed a native finding).
    Diff {
        /// Native review's findings JSON (array, or `{findings: [...]}`).
        #[arg(long)]
        native: PathBuf,
        /// OpenCode review's findings JSON.
        #[arg(long)]
        opencode: PathBuf,
        /// Max line delta for two findings to count as the same issue.
        #[arg(long, default_value_t = 3)]
        line_tolerance: i64,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Diff {
            native,
            opencode,
            line_tolerance,
        } => {
            let native = load_findings(&native)?;
            let opencode = load_findings(&opencode)?;
            let report = compare(&native, &opencode, line_tolerance);
            print_report(&native, &opencode, &report);
            // The gate: OpenCode must not MISS a native finding.
            if report.only_native.is_empty() {
                Ok(())
            } else {
                anyhow::bail!(
                    "SHADOW FAIL: OpenCode missed {} finding(s) the native review caught",
                    report.only_native.len()
                )
            }
        }
    }
}

fn print_report(native: &[Finding], opencode: &[Finding], report: &Report) {
    let diverged = report
        .matched
        .iter()
        .filter(|m| m.severity_diverged)
        .count();
    println!("═══ OpenCode ↔ native review shadow parity ═══");
    println!(
        "native: {} findings   opencode: {} findings   matched: {}",
        native.len(),
        opencode.len(),
        report.matched.len()
    );
    println!("  severity-diverged (matched but different priority): {diverged}");
    for m in report.matched.iter().filter(|m| m.severity_diverged) {
        println!(
            "    ⚠ {}:{}  native={}  opencode={}",
            m.file, m.line, m.native_priority, m.opencode_priority
        );
    }
    println!(
        "\n  ONLY IN NATIVE (OpenCode MISSED — the regression signal): {}",
        report.only_native.len()
    );
    for f in &report.only_native {
        println!("    ✗ {}:{}  [{}] {}", f.file, f.line, f.priority, f.title);
    }
    println!(
        "\n  only in opencode (new — signal or noise, a human call): {}",
        report.only_opencode.len()
    );
    for f in &report.only_opencode {
        println!("    + {}:{}  [{}] {}", f.file, f.line, f.priority, f.title);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(file: &str, line: i64, priority: &str, title: &str) -> Finding {
        Finding {
            file: file.into(),
            line,
            priority: priority.into(),
            category: String::new(),
            title: title.into(),
        }
    }

    #[test]
    fn identical_findings_all_match() {
        let a = vec![f("a.rs", 10, "P1", "bug"), f("b.rs", 3, "P2", "nit")];
        let report = compare(&a, &a.clone(), 3);
        assert_eq!(report.matched.len(), 2);
        assert!(report.only_native.is_empty());
        assert!(report.only_opencode.is_empty());
    }

    #[test]
    fn a_missed_native_finding_is_the_regression_signal() {
        let native = vec![f("a.rs", 10, "P1", "real bug"), f("b.rs", 3, "P2", "nit")];
        let opencode = vec![f("a.rs", 11, "P1", "real bug (rephrased)")]; // within tolerance of 10
        let report = compare(&native, &opencode, 3);
        assert_eq!(
            report.matched.len(),
            1,
            "the a.rs finding matches within ±3 lines"
        );
        assert_eq!(report.only_native.len(), 1, "b.rs was missed by opencode");
        assert_eq!(report.only_native[0].file, "b.rs");
        assert!(report.only_opencode.is_empty());
    }

    #[test]
    fn line_beyond_tolerance_does_not_match() {
        let native = vec![f("a.rs", 10, "P1", "bug")];
        let opencode = vec![f("a.rs", 50, "P1", "bug")];
        let report = compare(&native, &opencode, 3);
        assert!(report.matched.is_empty());
        assert_eq!(report.only_native.len(), 1);
        assert_eq!(report.only_opencode.len(), 1);
    }

    #[test]
    fn matched_issue_flags_severity_divergence() {
        let native = vec![f("a.rs", 10, "P1", "bug")];
        let opencode = vec![f("a.rs", 10, "P2", "bug")];
        let report = compare(&native, &opencode, 3);
        assert_eq!(report.matched.len(), 1);
        assert!(report.matched[0].severity_diverged);
    }

    #[test]
    fn an_extra_opencode_finding_is_only_opencode_not_a_failure() {
        let native = vec![f("a.rs", 10, "P1", "bug")];
        let opencode = vec![f("a.rs", 10, "P1", "bug"), f("c.rs", 5, "P2", "new nit")];
        let report = compare(&native, &opencode, 3);
        assert_eq!(report.matched.len(), 1);
        assert!(report.only_native.is_empty(), "no regression");
        assert_eq!(report.only_opencode.len(), 1);
    }
}
