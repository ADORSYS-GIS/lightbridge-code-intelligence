//! Compatibility bridge for the current Job loop during the R1c/R1d transition.
//!
//! Concrete review-tool behavior now lives in `lci-review-agent`; these re-exports deliberately keep
//! every existing runner import and dispatch call unchanged until R1e removes the old modules.

pub use lci_review_agent::tools::*;
