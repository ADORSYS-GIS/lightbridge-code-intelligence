//! A thin MCP client for calling already-deployed, in-cluster MCP servers (ADR-0066), configured
//! via [`crate::config::McpServerConfig`] — not hardcoded to any specific provider. Those servers
//! hold their own upstream provider credentials; this module only needs their in-cluster Service
//! URL, reached over plain HTTP within the cluster (no OAuth).
//!
//! One connection per call — these are occasional, on-demand review-time lookups, not a hot path,
//! so there is no session pool to manage or expire.

use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation, ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};

/// Hard ceiling on the text handed back to the agent: an upstream server is untrusted input
/// (ADR-0066) and could return an adversarially huge payload — cap it the same way `read_file` caps
/// the checkout (`services/agent-runner/src/review/native/tools.rs`).
pub const RESULT_CAP: usize = 32 * 1024;

/// Cap on graceful-shutdown time after a call. Independent of the caller's `timeout` — shutdown
/// should always be fast, so it gets its own short, fixed budget rather than reusing the call's.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// A tool a discovered MCP server exposes, as returned to the agent-runner for it to fold into the
/// live tool schema — no compile-time knowledge of any specific server's tools needed.
#[derive(Debug)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Connect to a streamable-HTTP MCP server, bounded by `timeout`.
async fn connect(
    base_url: &str,
    timeout: Duration,
) -> anyhow::Result<RunningService<RoleClient, ClientInfo>> {
    let transport = StreamableHttpClientTransport::from_uri(base_url.to_string());
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("lightbridge-control-plane", env!("CARGO_PKG_VERSION")),
    );
    // rmcp 3.x targets the MCP 2026-07-28 protocol, which removed the initialize/initialized
    // handshake in favor of stateless per-request metadata. `Auto` probes with `server/discover`
    // (preferring the modern protocol) and falls back to the legacy initialize handshake when the
    // peer is a legacy server — the right fit for calling heterogeneous in-cluster servers
    // (brave-search/context7) that may not all speak 2026-07-28 yet.
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    };
    tokio::time::timeout(timeout, client_info.serve_with_lifecycle(transport, lifecycle))
        .await
        .map_err(|_| anyhow::anyhow!("connecting to the MCP server timed out"))?
        .map_err(|error| anyhow::anyhow!("connecting to the MCP server failed: {error}"))
}

/// Bounded shutdown (see the comment at the call site in [`call_tool`] for why `close_with_timeout`
/// rather than `cancel`, which has no timeout of its own).
async fn disconnect(mut client: RunningService<RoleClient, ClientInfo>) {
    let _ = client.close_with_timeout(SHUTDOWN_TIMEOUT).await;
}

/// List the tools a remote MCP server exposes.
pub async fn list_tools(base_url: &str, timeout: Duration) -> anyhow::Result<Vec<McpTool>> {
    let client = connect(base_url, timeout).await?;
    let result = tokio::time::timeout(timeout, client.list_tools(Default::default()))
        .await
        .map_err(|_| anyhow::anyhow!("listing tools timed out"));
    disconnect(client).await;
    let result = result?.map_err(|error| anyhow::anyhow!("listing tools failed: {error}"))?;
    Ok(result
        .tools
        .into_iter()
        .map(|t| McpTool {
            name: t.name.to_string(),
            description: t.description.map(|d| d.to_string()).unwrap_or_default(),
            input_schema: serde_json::to_value(t.input_schema.as_ref())
                .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
        })
        .collect())
}

/// Call one tool on a remote streamable-HTTP MCP server and return its concatenated text content,
/// capped to [`RESULT_CAP`] bytes (valid-UTF-8 truncation, never mid-codepoint).
pub async fn call_tool(
    base_url: &str,
    tool_name: &str,
    arguments: serde_json::Value,
    timeout: Duration,
) -> anyhow::Result<String> {
    let client = connect(base_url, timeout).await?;

    let arguments = arguments.as_object().cloned().unwrap_or_default();
    let call = client
        .call_tool(CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments));
    let result = tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| anyhow::anyhow!("{tool_name} call timed out"));
    // Bounded shutdown: `cancel()` awaits the transport's close with NO timeout of its own — a
    // stalled-but-connected upstream (accepts the TCP connection, never completes/responds) can hang
    // this past `timeout` for an unbounded time (up to the OS TCP timeout). `close_with_timeout` caps
    // it explicitly; best-effort either way, so a shutdown timeout doesn't mask the actual result.
    disconnect(client).await;
    let result = result?.map_err(|error| anyhow::anyhow!("{tool_name} call failed: {error}"))?;

    if result.is_error == Some(true) {
        let text = collect_text(&result.content);
        anyhow::bail!(
            "{tool_name} returned an error: {}",
            if text.is_empty() {
                "(no detail)".to_string()
            } else {
                text
            }
        );
    }

    let text = collect_text(&result.content);
    if text.is_empty() {
        anyhow::bail!("{tool_name} returned no text content");
    }
    Ok(cap_utf8(text, RESULT_CAP))
}

fn collect_text(content: &[rmcp::model::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate to at most `cap` bytes, keeping the string valid UTF-8 (never splits a multi-byte char).
fn cap_utf8(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n… (truncated)");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_utf8_leaves_short_strings_untouched() {
        assert_eq!(cap_utf8("hello".to_string(), 100), "hello");
    }

    #[test]
    fn cap_utf8_truncates_on_a_char_boundary() {
        // "é" is 2 bytes (0xC3 0xA9); capping at byte 1 would split it if not boundary-checked.
        let s = "é".repeat(10);
        let capped = cap_utf8(s, 5);
        assert!(capped.starts_with("éé"));
        assert!(capped.contains("truncated"));
    }

    // ── Wire-level integration test ─────────────────────────────────────────────────────────────
    // rmcp 3.x targets the MCP 2026-07-28 protocol, which removed protocol-level sessions
    // (Mcp-Session-Id, initialize/initialized handshake) in favor of a stateless-per-request model.
    // That is exactly the kind of change that compiles fine and breaks at the wire level, so this
    // test spins up a REAL in-process MCP server (rmcp's own StreamableHttpService over axum) and
    // drives the control-plane client (`list_tools` / `call_tool`) against it over streamable HTTP —
    // exercising the `server/discover` + `tools/call` round-trip end to end.

    use axum::Router;
    use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
    use rmcp::model::{ServerCapabilities, ServerInfo};
    use rmcp::service::RequestContext;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use rmcp::{ErrorData, Json, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    #[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
    struct EchoArgs {
        text: String,
    }

    #[derive(Debug, Serialize, schemars::JsonSchema)]
    struct EchoResult {
        echoed: String,
    }

    struct EchoHandler {
        tool_router: ToolRouter<Self>,
    }

    impl EchoHandler {
        fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }
    }

    #[tool_router]
    impl EchoHandler {
        #[tool(name = "echo", description = "Echo the input text")]
        async fn echo_tool(
            &self,
            _context: RequestContext<RoleServer>,
            Parameters(args): Parameters<EchoArgs>,
        ) -> std::result::Result<Json<EchoResult>, ErrorData> {
            Ok(Json(EchoResult {
                echoed: args.text,
            }))
        }
    }

    #[tool_handler(router = self.tool_router)]
    impl ServerHandler for EchoHandler {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }
    }

    /// Bind an in-process MCP server to an ephemeral port and return its base URL.
    async fn spawn_mcp_server() -> String {
        let service: StreamableHttpService<EchoHandler, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(EchoHandler::new()),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default().disable_allowed_hosts(),
            );
        let router = Router::new().fallback_service(service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve mcp");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn client_lists_and_calls_tools_over_streamable_http() {
        let base_url = spawn_mcp_server().await;
        let timeout = Duration::from_secs(10);

        let tools = list_tools(&base_url, timeout)
            .await
            .expect("list_tools over the wire");
        assert!(
            tools.iter().any(|t| t.name == "echo"),
            "server should advertise the echo tool, got: {tools:?}"
        );

        let out = call_tool(
            &base_url,
            "echo",
            serde_json::json!({ "text": "hello from the client" }),
            timeout,
        )
        .await
        .expect("call_tool over the wire");
        // The `echo` tool returns `Json<EchoResult>`, which rmcp serializes into a text content
        // block — the same shape the real control-plane tools produce, and what `collect_text`
        // extracts. Assert the JSON text round-tripped intact.
        assert_eq!(out, r#"{"echoed":"hello from the client"}"#);
    }
}
