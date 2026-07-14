//! [`SastFinding`] — one opengrep finding, normalized from a SARIF result into the shape the review
//! buffer needs, plus the comment title/body it renders as.

/// One opengrep finding, normalized from a SARIF result into the shape the review buffer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SastFinding {
    /// Repo-root-relative, forward-slash path (the control plane re-normalizes, but we keep it clean).
    pub file: String,
    /// 1-based line on the new side of the diff.
    pub line: u32,
    /// The opengrep rule id, e.g. `rust.lang.security.unsafe-exec`.
    pub rule_id: String,
    /// The rule's message — what it found and why it matters.
    pub message: String,
    /// Triage priority mapped from the SARIF level (ADR-0032): `error`→P1, else→P2. opengrep findings
    /// are advisory security signals, never the P0 "blocks compilation" tier.
    pub priority: String,
    /// The rule's documentation link, when present, rendered into the finding body.
    pub help_uri: Option<String>,
}

impl SastFinding {
    /// Inline-comment title: an opengrep-attributed, single-line summary. The control plane renders the
    /// `security` badge in red (ADR-0032); the 🔍 marker + rule id make the source unmistakable so a
    /// SAST finding never masquerades as the agent's own (ADR-0061).
    pub fn title(&self) -> String {
        let summary = self
            .message
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(&self.rule_id);
        let summary = truncate(summary, 120);
        format!("🔍 opengrep: {summary}")
    }

    /// Inline-comment body: the full message, the rule attribution, and a docs link when the rule
    /// carries one. `resources` isn't on the buffer wire (the control plane sets it empty), so the
    /// reference link is folded into the body markdown here.
    pub fn body(&self) -> String {
        let mut body = self.message.trim().to_string();
        body.push_str(&format!(
            "\n\n_Detected by opengrep rule `{}` — a deterministic static-analysis match. \
             Verify before acting; suppress a false positive with an `opengrep-ignore` comment._",
            self.rule_id
        ));
        if let Some(uri) = self.help_uri.as_deref().filter(|u| !u.trim().is_empty()) {
            body.push_str(&format!("\n\n[Rule reference]({uri})"));
        }
        body
    }
}

/// Truncate to at most `max` chars (char-boundary safe), appending an ellipsis when cut. Shared by
/// [`SastFinding::title`] and [`super::digest`].
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_attributed_and_single_line() {
        let f = SastFinding {
            file: "src/a.rs".into(),
            line: 1,
            rule_id: "r.id".into(),
            message: "First line.\nSecond line.".into(),
            priority: "P1".into(),
            help_uri: None,
        };
        assert_eq!(f.title(), "🔍 opengrep: First line.");
        assert!(f.body().contains("opengrep rule `r.id`"));
    }

    #[test]
    fn body_includes_reference_when_present() {
        let f = SastFinding {
            file: "src/a.rs".into(),
            line: 1,
            rule_id: "r.id".into(),
            message: "msg".into(),
            priority: "P1".into(),
            help_uri: Some("https://docs/rule".into()),
        };
        assert!(f.body().contains("[Rule reference](https://docs/rule)"));
    }
}
