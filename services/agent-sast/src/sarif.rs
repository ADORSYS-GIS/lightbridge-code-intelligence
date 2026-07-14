//! SARIF parsing (pure, unit-tested): the slice of SARIF 2.1.0 opengrep emits, and turning it into
//! normalized [`super::SastFinding`]s.

use std::collections::HashMap;

use serde::Deserialize;

use super::finding::SastFinding;

/// The slice of SARIF 2.1.0 we consume. opengrep is SARIF-compatible with Semgrep: results carry a
/// `ruleId`, a `message.text`, and a physical location; severity is on the result's `level` and/or the
/// rule's `defaultConfiguration.level`, and the docs link is the rule's `helpUri`.
#[derive(Debug, Deserialize)]
struct Sarif {
    #[serde(default)]
    runs: Vec<SarifRun>,
}

#[derive(Debug, Deserialize)]
struct SarifRun {
    #[serde(default)]
    tool: SarifTool,
    #[serde(default)]
    results: Vec<SarifResult>,
}

#[derive(Debug, Default, Deserialize)]
struct SarifTool {
    #[serde(default)]
    driver: SarifDriver,
}

#[derive(Debug, Default, Deserialize)]
struct SarifDriver {
    #[serde(default)]
    rules: Vec<SarifRule>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifRule {
    #[serde(default)]
    id: String,
    #[serde(default)]
    help_uri: Option<String>,
    #[serde(default)]
    default_configuration: Option<SarifLevel>,
}

#[derive(Debug, Deserialize)]
struct SarifLevel {
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    #[serde(default)]
    rule_id: Option<String>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    message: SarifMessage,
    #[serde(default)]
    locations: Vec<SarifLocation>,
}

#[derive(Debug, Default, Deserialize)]
struct SarifMessage {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    #[serde(default)]
    physical_location: Option<SarifPhysicalLocation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    #[serde(default)]
    artifact_location: Option<SarifArtifactLocation>,
    #[serde(default)]
    region: Option<SarifRegion>,
}

#[derive(Debug, Deserialize)]
struct SarifArtifactLocation {
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    #[serde(default)]
    start_line: Option<u32>,
}

/// Parse opengrep's SARIF into normalized findings, dropping anything below `min_severity` and capping
/// at `max_findings` (logging the drop — no silent truncation). Rule metadata (helpUri, default level)
/// is keyed by rule id and joined onto each result.
pub(crate) fn parse_sarif(json: &str, min_severity: &str, max_findings: usize) -> Vec<SastFinding> {
    let sarif: Sarif = match serde_json::from_str(json) {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!(%error, "sast: parsing opengrep SARIF failed (non-fatal)");
            return Vec::new();
        }
    };
    let min = severity_rank(min_severity);
    let mut out: Vec<SastFinding> = Vec::new();
    let mut dropped_below_severity = 0usize;

    for run in &sarif.runs {
        // rule id → (helpUri, default level)
        let rules: HashMap<&str, (Option<&str>, Option<&str>)> = run
            .tool
            .driver
            .rules
            .iter()
            .map(|r| {
                (
                    r.id.as_str(),
                    (
                        r.help_uri.as_deref(),
                        r.default_configuration
                            .as_ref()
                            .and_then(|c| c.level.as_deref()),
                    ),
                )
            })
            .collect();

        for result in &run.results {
            let Some(rule_id) = result.rule_id.clone() else {
                continue;
            };
            let (help_uri, rule_level) =
                rules.get(rule_id.as_str()).copied().unwrap_or((None, None));
            // Severity: the result's own level wins, else the rule default, else "warning".
            let level = result.level.as_deref().or(rule_level).unwrap_or("warning");
            if severity_rank(level) < min {
                dropped_below_severity += 1;
                continue;
            }
            let Some((file, line)) = result.locations.iter().find_map(|loc| {
                let phys = loc.physical_location.as_ref()?;
                let uri = phys.artifact_location.as_ref()?.uri.as_deref()?;
                let line = phys.region.as_ref()?.start_line?;
                Some((normalize_path(uri), line))
            }) else {
                continue; // a finding we can't anchor to a file:line is not actionable on a PR
            };
            out.push(SastFinding {
                file,
                line: line.max(1),
                rule_id,
                message: result.message.text.trim().to_string(),
                priority: priority_for(level).to_string(),
                help_uri: help_uri.map(str::to_string),
            });
        }
    }

    if dropped_below_severity > 0 {
        tracing::info!(
            dropped = dropped_below_severity,
            min_severity,
            "sast: dropped findings below the minimum severity"
        );
    }
    if out.len() > max_findings {
        tracing::warn!(
            kept = max_findings,
            total = out.len(),
            "sast: capping findings at max_findings (some opengrep findings not posted)"
        );
        out.truncate(max_findings);
    }
    out
}

/// Rank of a SARIF level for min-severity comparison. `error` (3) > `warning` (2) > `note`/`info` (1).
/// Unknown levels rank as `warning` so an odd value isn't silently dropped.
fn severity_rank(level: &str) -> u8 {
    match level.trim().to_ascii_lowercase().as_str() {
        "error" => 3,
        "warning" | "warn" => 2,
        "note" | "info" | "information" => 1,
        _ => 2,
    }
}

/// Map a SARIF level to a triage priority (ADR-0032): `error`→P1, everything else→P2. SAST findings are
/// never P0 — that tier is reserved for "blocks compilation / must fix".
fn priority_for(level: &str) -> &'static str {
    match severity_rank(level) {
        3 => "P1",
        _ => "P2",
    }
}

/// Normalize a SARIF artifact uri toward the repo-root-relative form the control plane expects: strip a
/// `file://` scheme and any leading `./` or `/`, and use forward slashes.
fn normalize_path(uri: &str) -> String {
    uri.strip_prefix("file://")
        .unwrap_or(uri)
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SARIF: &str = r#"{
      "runs": [{
        "tool": {"driver": {"name": "opengrep", "rules": [
          {"id": "rust.security.exec", "helpUri": "https://example.com/exec",
           "defaultConfiguration": {"level": "error"}},
          {"id": "rust.style.nit", "defaultConfiguration": {"level": "note"}}
        ]}},
        "results": [
          {"ruleId": "rust.security.exec",
           "message": {"text": "Command injection via untrusted input.\nUse a parameterized API."},
           "locations": [{"physicalLocation": {
             "artifactLocation": {"uri": "src/exec.rs"}, "region": {"startLine": 42}}}]},
          {"ruleId": "rust.style.nit",
           "message": {"text": "Trivial style nit."},
           "locations": [{"physicalLocation": {
             "artifactLocation": {"uri": "src/exec.rs"}, "region": {"startLine": 7}}}]}
        ]
      }]
    }"#;

    #[test]
    fn parse_sarif_maps_severity_help_and_location() {
        // min_severity "warning" drops the note-level style nit, keeps the error-level security finding.
        let findings = parse_sarif(SARIF, "warning", 50);
        assert_eq!(
            findings.len(),
            1,
            "note-level finding dropped below warning"
        );
        let f = &findings[0];
        assert_eq!(f.file, "src/exec.rs");
        assert_eq!(f.line, 42);
        assert_eq!(f.rule_id, "rust.security.exec");
        assert_eq!(f.priority, "P1", "error level → P1");
        assert_eq!(f.help_uri.as_deref(), Some("https://example.com/exec"));
        assert!(f.message.starts_with("Command injection"));
    }

    #[test]
    fn parse_sarif_min_severity_note_keeps_everything() {
        let findings = parse_sarif(SARIF, "note", 50);
        assert_eq!(findings.len(), 2, "note threshold keeps the style nit too");
        // The note-level finding maps to P2.
        let nit = findings
            .iter()
            .find(|f| f.rule_id == "rust.style.nit")
            .unwrap();
        assert_eq!(nit.priority, "P2");
    }

    #[test]
    fn parse_sarif_caps_at_max_findings() {
        let findings = parse_sarif(SARIF, "note", 1);
        assert_eq!(findings.len(), 1, "capped at max_findings");
    }

    #[test]
    fn parse_sarif_tolerates_garbage() {
        assert!(parse_sarif("not json", "warning", 50).is_empty());
        assert!(parse_sarif("{}", "warning", 50).is_empty());
    }

    #[test]
    fn normalize_path_strips_scheme_and_prefixes() {
        assert_eq!(normalize_path("file://./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("/src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src\\a.rs"), "src/a.rs");
    }
}
