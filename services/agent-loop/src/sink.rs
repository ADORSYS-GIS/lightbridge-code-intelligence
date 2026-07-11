//! The transcript capture seam (ADR-0034/0060) — dynamically dispatched, host-chosen flushing.

use lci_agent_types::TranscriptEntry;

/// Records the run's transcript. The Job host buffers and submits at end of run; the durable worker
/// flushes incrementally (companion doc §3.7). Deliberately dyn-compatible (no generic methods) so
/// the engine can hold a `Box<dyn TranscriptSink>` chosen by the host.
pub trait TranscriptSink: Send {
    /// Record one entry — an assistant turn, a tool result, or a policy decision.
    fn record(&mut self, entry: TranscriptEntry);
}
