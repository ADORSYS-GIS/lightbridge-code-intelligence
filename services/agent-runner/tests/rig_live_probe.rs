//! Live gateway probe for the rig transport-fidelity spike (ADR-0075, ticket #300).
//!
//! This is the **post-merge #300 verification** and is deliberately kept in its own test target,
//! separate from the network-free `rig_fidelity.rs` assertions, so the offline harness coverage is not
//! diluted by code that can only run against a live gateway. The single test here is `#[ignore]`d, so
//! CI (no gateway) stays green. Run it by hand against eaig, once:
//!
//! ```text
//! LLM_BASE_URL=… LLM_API_KEY=… LLM_MODEL=gemini-3-pro \
//!   cargo test -p agent-runner --test rig_live_probe -- --ignored --nocapture
//! ```
//!
//! It builds rig's OpenAI **Chat Completions** client at the eaig `base_url`, gives it one tool, and
//! drives a multi-turn prompt (`max_turns > 1`) — which forces rig to echo the assistant tool-call turn
//! back on the follow-up request. If rig has dropped `thought_signature` (per the offline verdict in
//! `rig_fidelity.rs`), a Gemini-family model rejects that follow-up with a 400, surfacing as a
//! `PromptError`. A clean run therefore means either the signature survived OR the model did not require
//! it; the error message is the diagnostic.
//!
//! NOTE (rustls): agent-runner's own binary installs no rustls provider, and this dev-dep pulls
//! `aws-lc-rs` alongside the workspace's `ring` — so the probe installs a process default up front to
//! avoid the rustls-0.23 ambiguous-provider panic (the #264 family).
//!
//! CA CAVEAT (a lineage finding in its own right): rig's OpenAI client builder is version-locked to
//! **reqwest 0.13** (`HttpClientExt` is implemented for reqwest 0.13's `Client`), so we cannot inject
//! agent-runner's own reqwest **0.12** client — the one that knows how to trust eaig's self-signed CA
//! via `LLM_CA_CERT`. For this probe, eaig's CA must therefore be trusted at the OS level (or a reqwest
//! 0.13 client built separately). Wiring a custom-CA transport into rig is follow-up work for ADR-0075.

use rig_agent::AgentBuilder;
use rig_agent::completion::Prompt;
use rig_core::client::CompletionClient;
use rig_core::providers::openai::CompletionsClient;
use rig_core::tool::PortableTool;
use serde_json::json;

#[derive(Debug)]
struct EchoError;
impl std::fmt::Display for EchoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("echo tool error")
    }
}
impl std::error::Error for EchoError {}

#[derive(serde::Deserialize)]
struct EchoArgs {
    text: String,
}

/// A trivial tool: forces a tool-call turn (and thus a signature-bearing follow-up) with no side
/// effects. Its output feeds straight back to the model.
struct EchoTool;
impl PortableTool for EchoTool {
    const NAME: &'static str = "echo";
    type Error = EchoError;
    type Args = EchoArgs;
    type Output = String;

    fn description(&self) -> String {
        "Echo the given text back verbatim.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }

    async fn call(&self, args: EchoArgs) -> Result<String, EchoError> {
        Ok(args.text)
    }
}

#[tokio::test]
#[ignore = "requires the live eaig gateway (LLM_BASE_URL/LLM_API_KEY); #300 post-merge verification"]
async fn thought_signature_survival() -> anyhow::Result<()> {
    // Two rustls providers are linked in this test binary (workspace `ring` + this dev-dep's
    // `aws-lc-rs`); install one explicitly so the first TLS handshake doesn't panic. Idempotent.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let base_url = std::env::var("LLM_BASE_URL")?;
    let api_key = std::env::var("LLM_API_KEY")?;
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gemini-3-pro".to_string());

    // Uses rig's default (reqwest 0.13) transport — see the CA CAVEAT above: eaig's self-signed CA
    // must be trusted at the OS level for this probe, as rig won't accept our reqwest-0.12 client.
    let client = CompletionsClient::builder()
        .api_key(api_key.as_str())
        .base_url(&base_url)
        .build()?;

    // rig-agent 0.42.0: the agent abstraction lives in `rig-agent`; build a completion model from
    // the client, then an agent with the echo tool.
    let model = client.completion_model(model.as_str());
    let agent = AgentBuilder::new(model)
        .preamble(
            "You are a test harness. When asked, call the `echo` tool exactly once, then reply \
             with only the echoed text.",
        )
        .tool(EchoTool)
        .build();

    // max_turns > 1 forces the follow-up request that must carry the tool call's thought_signature.
    let result = agent
        .prompt("Call the echo tool with text='ping', then reply with the echoed text.")
        .max_turns(4)
        .await;

    match result {
        Ok(answer) => {
            println!("LIVE: multi-turn tool exchange completed. Final answer: {answer:?}");
            assert!(
                answer.to_lowercase().contains("ping"),
                "expected the echoed text to survive the round-trip; got {answer:?}"
            );
        }
        Err(e) => {
            let msg = format!("{e:#}");
            println!("LIVE: multi-turn tool exchange FAILED: {msg}");
            assert!(
                !msg.to_lowercase().contains("thought_signature"),
                "LIVE CONFIRMATION of the offline verdict: rig dropped the thought_signature and \
                 the gateway rejected the follow-up turn — {msg}"
            );
            return Err(e.into());
        }
    }
    Ok(())
}
