//! ADR-0066 MCP knowledge tools: discover what the currently-configured MCP servers expose, and
//! dispatch calls to them through the control plane (the runner never talks to an MCP server
//! directly).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ControlPlaneClient;

/// One tool a discovered MCP server exposes (ADR-0066), as the control plane reports it — already
/// prefixed `mcp__<server>__<tool>` and ready to fold into the live tool schema.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Result of a knowledge-tool call (ADR-0066). Plain text, already size-capped control-plane-side —
/// untrusted content, framed as such before it reaches the model (see `lci_review_agent::tools::mcp`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KnowledgeToolResult {
    pub text: String,
}

impl ControlPlaneClient {
    /// `GET /internal/tasks/{id}/knowledge/tools` — discover every tool the currently-configured MCP
    /// servers expose (ADR-0066). Called once at run start (not compiled in — any server the control
    /// plane is configured with shows up with zero runner code changes). Empty when no servers are
    /// configured, never an error, so a review still runs normally with no external-knowledge tools.
    pub async fn list_knowledge_tools(&self, task_id: Uuid) -> anyhow::Result<Vec<DiscoveredTool>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/knowledge/tools", self.base_url);
        let tools = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("knowledge-tool discovery request")?
            .error_for_status()
            .context("control plane rejected knowledge-tool discovery")?
            .json()
            .await
            .context("parsing discovered knowledge tools")?;
        Ok(tools)
    }

    /// `POST /internal/tasks/{id}/knowledge/call` — dispatch a call to a previously-discovered
    /// knowledge tool (ADR-0066). `tool` is the prefixed name from `list_knowledge_tools`
    /// (`mcp__<server>__<tool>`); `arguments` is forwarded verbatim.
    pub async fn call_knowledge_tool(
        &self,
        task_id: Uuid,
        tool: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<String> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/knowledge/call", self.base_url);
        let body: KnowledgeToolResult = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "tool": tool, "arguments": arguments }))
            .send()
            .await
            .context("knowledge-tool call request")?
            .error_for_status()
            .context("control plane rejected the knowledge-tool call")?
            .json()
            .await
            .context("parsing knowledge-tool result")?;
        Ok(body.text)
    }
}
