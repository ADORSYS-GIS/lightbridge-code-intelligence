//! ACP (Agent Client Protocol) client for driving an `opencode acp` subprocess — the foundation of
//! the agent-plane supervisor ([ADR-0094](../../docs/adr/0094-opencode-acp-open-mode-host.md)).
//!
//! The supervisor spawns OpenCode as a **child process** and drives it over newline-delimited
//! JSON-RPC on stdio (the embedded-stdio decision in ADR-0094 — not a `--port` sidecar). This crate
//! is the transport + protocol layer: spawn, `initialize`, `session/new`, `session/prompt`, and a
//! background reader that correlates responses, collects `session/update` notifications, and answers
//! `session/request_permission` from a policy. Budgets, checkout prep, lifecycle reporting via the
//! mediated internal API, and mode selection are later slices that build on this.
//!
//! Scope note (slice 1): this is the transport foundation. Errors surface as `anyhow` for now; a
//! typed `AcpError` (per the repo's thiserror-per-crate convention) lands when the surface settles.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

/// How the client answers an agent-initiated `session/request_permission`. The supervisor never runs
/// interactively; permission decisions come from mode policy. Slice 1 offers the two obvious blanket
/// policies; a per-tool policy map is a later slice (it mirrors the config `permission` block).
#[derive(Clone, Copy, Debug, Default)]
pub enum PermissionPolicy {
    /// Pick the first `allow`-kind option opencode offers (fall back to the first option).
    #[default]
    AllowFirst,
    /// Cancel every permission request (deny-all).
    Cancel,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A live ACP session transport over a spawned `opencode acp` child.
pub struct AcpClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    updates: Arc<Mutex<Vec<Value>>>,
    next_id: AtomicI64,
}

impl AcpClient {
    /// Spawn `<bin> acp` in `cwd` and start the background reader. `bin` is typically `opencode`.
    pub async fn spawn(bin: &str, cwd: &Path, policy: PermissionPolicy) -> Result<Self> {
        let mut child = Command::new(bin)
            .arg("acp")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Kill the opencode child if this AcpClient is dropped without an explicit shutdown() —
            // tokio does not reap the OS process on drop otherwise, which would orphan opencode.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{bin} acp` (is it on PATH?)"))?;

        let stdin = Arc::new(Mutex::new(
            child.stdin.take().context("child stdin was not piped")?,
        ));
        let stdout = child.stdout.take().context("child stdout was not piped")?;
        let pending: Pending = Arc::default();
        let updates: Arc<Mutex<Vec<Value>>> = Arc::default();

        tokio::spawn(reader_loop(
            stdout,
            pending.clone(),
            updates.clone(),
            stdin.clone(),
            policy,
        ));

        Ok(Self {
            child,
            stdin,
            pending,
            updates,
            next_id: AtomicI64::new(1),
        })
    }

    /// Issue a JSON-RPC request and await its correlated response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(err) = send_message(
            &self.stdin,
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await
        {
            // The write failed — don't leave a dangling waiter in the pending map.
            self.pending.lock().await.remove(&id);
            return Err(err.context(format!("sending acp `{method}` request")));
        }
        let resp = rx
            .await
            .with_context(|| format!("acp `{method}` response channel closed (agent exited?)"))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("acp `{method}` returned an error: {err}"));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// ACP `initialize`. Returns the agent capabilities/info block.
    pub async fn initialize(&self) -> Result<Value> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } }
            }),
        )
        .await
    }

    /// ACP `session/new`. `mcp_servers` is passed through verbatim (probe finding: opencode honors
    /// http/sse MCP entries here; stdio MCP goes via the rendered config instead). Returns the id.
    pub async fn new_session(&self, cwd: &str, mcp_servers: Value) -> Result<String> {
        let result = self
            .request(
                "session/new",
                json!({ "cwd": cwd, "mcpServers": mcp_servers }),
            )
            .await?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .context("session/new returned no sessionId")
    }

    /// ACP `session/prompt` with a single text block. Returns the result (carries `stopReason`).
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<Value> {
        self.request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }),
        )
        .await
    }

    /// The `sessionUpdate` kinds seen so far (e.g. `agent_thought_chunk`, `tool_call`), in order.
    pub async fn update_kinds(&self) -> Vec<String> {
        self.updates
            .lock()
            .await
            .iter()
            .filter_map(|u| {
                u.get("sessionUpdate")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect()
    }

    /// Terminate the child and reap it. The supervisor calls this on budget exhaustion or task
    /// completion. `start_kill` only signals; `wait` reaps the process so it can't linger as a zombie.
    pub async fn shutdown(mut self) -> Result<()> {
        self.child
            .start_kill()
            .context("signalling opencode acp child to stop")?;
        self.child
            .wait()
            .await
            .context("waiting for opencode acp child to exit")?;
        Ok(())
    }
}

/// Reads newline-delimited JSON-RPC from the child's stdout: routes responses to their waiters,
/// collects `session/update` notifications, and answers agent-initiated requests.
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    updates: Arc<Mutex<Vec<Value>>>,
    stdin: Arc<Mutex<ChildStdin>>,
    policy: PermissionPolicy,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").and_then(Value::as_i64);
        let method = msg.get("method").and_then(Value::as_str);

        let Some(method) = method else {
            // No method → a response to one of our requests.
            let Some(id) = id else { continue };
            let waiter = pending.lock().await.remove(&id);
            if let Some(tx) = waiter {
                let _ = tx.send(msg);
            }
            continue;
        };

        match method {
            "session/update" => {
                if let Some(update) = msg.get("params").and_then(|p| p.get("update")) {
                    updates.lock().await.push(update.clone());
                }
            }
            "session/request_permission" => {
                if let Some(id) = id {
                    let result = decide_permission(&msg, policy);
                    let _ = send_message(
                        &stdin,
                        json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    )
                    .await;
                }
            }
            other => {
                // Any other agent-initiated request must be answered so opencode isn't wedged
                // (we advertised no fs capability, so these shouldn't happen in practice).
                if let Some(id) = id {
                    let _ = send_message(
                        &stdin,
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}}),
                    )
                    .await;
                }
            }
        }
    }

    // The reader is exiting — the child's stdout hit EOF (opencode exited or crashed). Drop every
    // pending sender so any in-flight `request()` wakes with a channel-closed error instead of
    // awaiting a response that will never come.
    pending.lock().await.clear();
}

fn decide_permission(msg: &Value, policy: PermissionPolicy) -> Value {
    if matches!(policy, PermissionPolicy::Cancel) {
        return json!({ "outcome": { "outcome": "cancelled" } });
    }
    let options = msg
        .get("params")
        .and_then(|p| p.get("options"))
        .and_then(Value::as_array);
    let chosen = options.and_then(|opts| {
        opts.iter()
            .find(|o| {
                o.get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|k| k.starts_with("allow"))
            })
            .or_else(|| opts.first())
            .and_then(|o| o.get("optionId"))
            .and_then(Value::as_str)
    });
    match chosen {
        Some(option_id) => json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

async fn send_message(stdin: &Arc<Mutex<ChildStdin>>, msg: Value) -> Result<()> {
    let mut line = serde_json::to_string(&msg)?;
    line.push('\n');
    let mut guard = stdin.lock().await;
    guard.write_all(line.as_bytes()).await?;
    guard.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_first_picks_the_allow_kind_option() {
        let msg = json!({"params": {"options": [
            {"optionId": "reject", "kind": "reject_once"},
            {"optionId": "ok", "kind": "allow_once"}
        ]}});
        let out = decide_permission(&msg, PermissionPolicy::AllowFirst);
        assert_eq!(out["outcome"]["optionId"], "ok");
    }

    #[test]
    fn allow_first_falls_back_to_first_option() {
        let msg = json!({"params": {"options": [{"optionId": "only", "kind": "reject_once"}]}});
        let out = decide_permission(&msg, PermissionPolicy::AllowFirst);
        assert_eq!(out["outcome"]["optionId"], "only");
    }

    #[test]
    fn cancel_policy_cancels() {
        let msg = json!({"params": {"options": [{"optionId": "ok", "kind": "allow_once"}]}});
        let out = decide_permission(&msg, PermissionPolicy::Cancel);
        assert_eq!(out["outcome"]["outcome"], "cancelled");
    }
}
