//! Per-turn proof-of-work log lines (ADR-0060) from the loop's sink events.
//!
//! The DB run transcript was retired in favour of logs-only observability (epic #459): Loki is the
//! single observability surface. What survives here is the *logging* — each assistant turn's
//! chain-of-thought (`agent reasoning`) and visible answer (`agent content`), emitted so a native run
//! is legible from a live log tail.

use lci_agent_loop::TranscriptEvent;
use uuid::Uuid;

/// Default cap for the per-turn `agent reasoning` log line (ADR-0060). A heavy reasoner (GLM-5.2) emits
/// thousands of chars per turn; override with `REASONING_LOG_CHARS` (`0` = unbounded).
const REASONING_LOG_CHARS_DEFAULT: usize = 4000;
/// Default cap for the per-turn `agent content` log line — the model's visible answer text. Override
/// with `CONTENT_LOG_CHARS` (`0` = unbounded).
const CONTENT_LOG_CHARS_DEFAULT: usize = 4000;

/// Resolve a per-turn log cap from `var`, falling back to `default`. A non-numeric or absent value uses
/// the default; `0` means log the whole string. Read once per run (this runs a single time over all
/// events — see `append_transcript`'s call site), so no per-turn process-env lock.
fn log_cap(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

/// The slice of `text` to show on a per-turn log line, or `None` when it is blank (skip the line
/// entirely — a pure tool-call turn has no prose). `cap == 0` logs the whole string; otherwise it is
/// bounded to at most `cap` bytes on a char boundary (the full length is logged alongside as a count).
fn bounded_for_log(text: &str, cap: usize) -> Option<&str> {
    if text.trim().is_empty() {
        return None;
    }
    Some(if cap == 0 {
        text
    } else {
        truncate_on_boundary(text, cap)
    })
}

/// Emit per-turn proof-of-work log lines (ADR-0060) from the loop's sink events so a native run is
/// legible from a live log tail (Loki is the observability surface — epic #459). Each `Assistant`
/// event carries its own `telemetry` (ADR-0087: it rides the journaled `AssistantTurn`, not a
/// side-channel keyed by position — a resumed/replayed turn's telemetry is exactly what was journaled
/// with it; see #411/#417). Three lines per assistant turn: `agent turn complete` (token counts +
/// chain-of-thought length), `agent reasoning` (the model's chain-of-thought), and `agent content`
/// (its visible answer) — each bounded and skipped when empty. Both prose lines matter to an operator:
/// the reasoning shows *how* it got there, the content shows *what* it concluded. Tool and policy
/// events carry no prose, so they emit nothing here.
pub(crate) fn log_agent_turns(events: &[TranscriptEvent], task_id: Uuid) {
    // Resolved once (this fn runs a single time over all events), not per turn.
    let reasoning_cap = log_cap("REASONING_LOG_CHARS", REASONING_LOG_CHARS_DEFAULT);
    let content_cap = log_cap("CONTENT_LOG_CHARS", CONTENT_LOG_CHARS_DEFAULT);

    for event in events {
        let TranscriptEvent::Assistant {
            turn,
            message,
            telemetry,
        } = event
        else {
            // Tool results / policy events carry no model prose — nothing to log.
            continue;
        };
        let telemetry = telemetry.as_ref();
        let reasoning = telemetry.and_then(|entry| entry.reasoning.as_deref());
        let reasoning_chars = reasoning.map(|r| r.chars().count()).unwrap_or(0);
        // Proof-of-work (epic #137): one concise per-turn line, including the chain-of-thought
        // length (the reliable "how far did it think" signal even when the gateway folds reasoning
        // into `completion_tokens`).
        tracing::info!(
            task_id = %task_id,
            turn,
            model = telemetry.map(|entry| entry.model.as_str()).unwrap_or("?"),
            prompt_tokens = telemetry.and_then(|entry| entry.prompt_tokens).unwrap_or(-1),
            completion_tokens = telemetry
                .and_then(|entry| entry.completion_tokens)
                .unwrap_or(-1),
            reasoning_tokens = telemetry
                .and_then(|entry| entry.reasoning_tokens)
                .unwrap_or(-1),
            reasoning_chars,
            "agent turn complete"
        );
        // Proof-of-work (ADR-0060): log the model's chain-of-thought so a run is legible from a live
        // log tail. This is the *thinking* (`reasoning_content`), not the visible answer — present
        // even on pure tool-call turns; kept off `ChatMessage`, so it is logged here, never echoed
        // back to the model. Restores the `agent reasoning` line dropped in the god-file split
        // (#395/#423), which had degraded this signal to a bare char count.
        if let Some(shown) = reasoning.and_then(|r| bounded_for_log(r, reasoning_cap)) {
            tracing::info!(
                task_id = %task_id,
                turn,
                reasoning_chars,
                reasoning = %shown,
                "agent reasoning"
            );
        }
        // The model's visible answer for this turn (the text it would post / reason from next).
        // Distinct from reasoning and equally load-bearing for an operator — a maintainer needs to
        // see *what* the model said, not only how long it thought.
        let content = message.content.as_deref();
        let content_chars = content.map(|c| c.chars().count()).unwrap_or(0);
        if let Some(shown) = content.and_then(|c| bounded_for_log(c, content_cap)) {
            tracing::info!(
                task_id = %task_id,
                turn,
                content_chars,
                content = %shown,
                "agent content"
            );
        }
    }
}

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use lci_agent_loop::{ChatMessage, TranscriptEvent};
    use lci_agent_types::{AssistantTurn, TurnTelemetry};
    use uuid::Uuid;

    use super::{bounded_for_log, log_agent_turns};

    /// A cloneable in-memory `MakeWriter` so a scoped subscriber can capture the actual log output.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl tracing_subscriber::fmt::MakeWriter<'_> for Buf {
        type Writer = Buf;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// The whole point of this logging: a maintainer must SEE the reasoning and the content, as two
    /// separate console lines, not a bare `reasoning_chars` count. This captures the real `tracing`
    /// output of `log_agent_turns` and asserts both texts are emitted.
    #[test]
    fn emits_separate_reasoning_and_content_lines_a_maintainer_can_read() {
        let events = vec![TranscriptEvent::Assistant {
            turn: 0,
            message: ChatMessage::assistant(AssistantTurn {
                content: Some("Found a use-after-free in cleanup().".to_string()),
                ..Default::default()
            }),
            telemetry: Some(TurnTelemetry {
                model: "glm-5p2".into(),
                prompt_tokens: Some(29_358),
                completion_tokens: Some(96),
                reasoning_tokens: Some(0),
                reasoning: Some("Let me trace the free path before concluding.".into()),
            }),
        }];

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_agent_turns(&events, Uuid::nil());
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();

        // Two distinct lines, each carrying the actual text (not just a length).
        assert!(
            out.contains("agent reasoning"),
            "missing reasoning line:\n{out}"
        );
        assert!(
            out.contains("Let me trace the free path before concluding."),
            "reasoning text not logged:\n{out}"
        );
        assert!(
            out.contains("agent content"),
            "missing content line:\n{out}"
        );
        assert!(
            out.contains("Found a use-after-free in cleanup()."),
            "content text not logged:\n{out}"
        );
    }

    #[test]
    fn blank_reasoning_or_content_is_skipped() {
        // A pure tool-call turn (no prose) must not emit an empty `agent reasoning`/`agent content` line.
        assert_eq!(bounded_for_log("", 4000), None);
        assert_eq!(bounded_for_log("   \n\t ", 4000), None);
    }

    #[test]
    fn short_text_is_shown_whole() {
        assert_eq!(
            bounded_for_log("thinking about the diff", 4000),
            Some("thinking about the diff")
        );
    }

    #[test]
    fn cap_zero_logs_the_whole_string() {
        let long = "x".repeat(50_000);
        assert_eq!(bounded_for_log(&long, 0), Some(long.as_str()));
    }

    #[test]
    fn over_cap_text_is_bounded_on_a_char_boundary() {
        // 10 four-byte chars = 40 bytes; a 25-byte cap must not split a char and must stay <= cap.
        let text = "🧠".repeat(10);
        let shown = bounded_for_log(&text, 25).expect("non-blank");
        assert!(shown.len() <= 25);
        assert!(text.starts_with(shown));
        // Truncated on a char boundary → a whole number of 4-byte chars (24 bytes, 6 chars).
        assert_eq!(shown.len() % 4, 0);
    }
}
