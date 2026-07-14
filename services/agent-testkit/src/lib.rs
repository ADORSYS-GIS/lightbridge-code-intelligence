//! Deterministic support for the R1 legacy-vs-extracted agent comparison.
//!
//! Two seams: [`goldens`] owns the frozen scenario fixtures and the legacy-trace parity harness;
//! [`fakes`] owns the scripted/static/capturing/failing test doubles that stand in for the
//! agent-loop's `ModelClient`, `Tool`, `TranscriptSink`, and `StepRuntime` traits.

mod fakes;
mod goldens;

pub use fakes::{CapturingSink, FailingRuntime, ScriptedModel, StaticTool};
pub use goldens::{
    GoldenHarness, GoldenScenario, GoldenScript, GoldenSettings, LegacyTrace, ObservedCall,
    ObservedWrite,
};
