//! Smoke/verification binary for the ACP host (RFC-0009 / ADR-0094).
//!
//! Spawns a real `opencode acp`, runs `initialize` + `session/new`, and prints the handshake — the
//! Rust equivalent of the TS fidelity probe, proving the supervisor's transport drives opencode.
//! `session/prompt` needs a configured provider, so it is attempted only when a message is given
//! (run it against the sim/eaig provider to exercise a full turn).
//!
//! Usage:
//!   OPENCODE_BIN=/path/to/opencode  acp-smoke  [cwd]  [optional prompt]

use std::path::PathBuf;

use anyhow::{Context, Result};
use lci_acp_host::{AcpClient, PermissionPolicy};

#[tokio::main]
async fn main() -> Result<()> {
    let bin = std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
    let mut args = std::env::args().skip(1);
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let prompt = args.next();

    let client = AcpClient::spawn(&bin, &cwd, PermissionPolicy::AllowFirst)
        .await
        .context("spawning opencode acp")?;

    let init = client.initialize().await.context("initialize")?;
    let version = init
        .get("agentInfo")
        .and_then(|a| a.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    let mcp_caps = init
        .get("agentCapabilities")
        .and_then(|c| c.get("mcpCapabilities"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    println!("initialize OK — agentInfo.version={version}, mcpCapabilities={mcp_caps}");

    let cwd_str = cwd.to_string_lossy();
    let session = client
        .new_session(&cwd_str, serde_json::json!([]))
        .await
        .context("session/new")?;
    println!("session/new OK — sessionId={session}");

    if let Some(text) = prompt {
        match client.prompt(&session, &text).await {
            Ok(result) => {
                let stop = result
                    .get("stopReason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<none>");
                println!("session/prompt OK — stopReason={stop}");
                println!("update kinds: {:?}", client.update_kinds().await);
            }
            Err(err) => println!("session/prompt errored (a provider is required): {err}"),
        }
    } else {
        println!("(no prompt given — skipping session/prompt, which needs a configured provider)");
    }

    client.shutdown().await?;
    println!("SMOKE OK");
    Ok(())
}
