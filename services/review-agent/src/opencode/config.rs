//! Render the per-task OpenCode config for a review run.
//!
//! The review system prompt is dynamic (`prompt::build_messages` folds in the repo's agent
//! instructions, prior reviews, and memory), so the config is rendered per task by the host rather
//! than shipped as a static asset — which also avoids duplicating the authoritative reviewer prompt
//! (it lives in `ReviewConfig::system_prompt`, from the ai-helm chart).
//!
//! Two invariants the render enforces for **coverage parity** with the native loop:
//! - **All file access is mediated.** OpenCode's built-in `read`/`grep`/`glob`/`list`/`edit`/`bash`
//!   (and `task`, i.e. subagents) are disabled, so every read goes through `lightbridge_read_file` and
//!   the retrieval tools — the same single mediated path the native review uses, which is what makes
//!   the recorder-driven coverage accounting exact (a built-in `read` would be invisible to it).
//! - **Read-only.** `edit`/`bash`/`webfetch` are denied; a review never mutates the tree or egresses.
//!
//! Secrets ride `{env:…}` placeholders (like the sim config), never written into the file — the host
//! sets `LCI_EAIG_{BASE_URL,API_KEY,MODEL}` in OpenCode's environment.

use serde_json::{Value, json};

/// Absolute in-image paths to the vendored plugins (they live under `/opt`, not the checkout — see
/// `integrations/opencode/config/opencode.jsonc`).
const PLUGIN_PATHS: &[&str] = &[
    "/opt/lightbridge/opencode/plugins/recorder/src/index.ts",
    "/opt/lightbridge/opencode/plugins/gate-interlock/src/index.ts",
    "/opt/lightbridge/opencode/plugins/logger/src/index.ts",
];

/// OpenCode built-in tools disabled for review so every investigation goes through the mediated
/// `lightbridge_*` tools (unlisted MCP tools stay enabled). `task` is disabled too: native review is a
/// single loop with no subagents, and a subagent's built-in reads would escape coverage accounting.
const DISABLED_BUILTINS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "edit",
    "write",
    "patch",
    "bash",
    "webfetch",
    "websearch",
    "task",
];

/// Render the OpenCode config (`opencode.json` shape) for one review run. `system_prompt` is the
/// reviewer guidance from [`crate::flows`]/`ReviewConfig`; `fast` selects the tier (deep enables the
/// reasoning model per ADR-0069, fast does not); `temperature` is passed through to the provider when
/// set (a best-effort — fine sampling params don't all map 1:1 once OpenCode owns the loop).
#[must_use]
pub fn render_review_config(system_prompt: &str, fast: bool, temperature: Option<f64>) -> Value {
    let mut model_options = json!({});
    if let Some(temperature) = temperature {
        model_options["temperature"] = json!(temperature);
    }

    let disabled_tools: serde_json::Map<String, Value> = DISABLED_BUILTINS
        .iter()
        .map(|name| ((*name).to_string(), json!(false)))
        .collect();

    json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "eaig": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "eaig",
                "options": {
                    "baseURL": "{env:LCI_EAIG_BASE_URL}",
                    "apiKey": "{env:LCI_EAIG_API_KEY}"
                },
                "models": {
                    "reviewer": {
                        "id": "{env:LCI_EAIG_MODEL}",
                        "name": "eaig reviewer",
                        // Deep tier runs a reasoning-capable model (ADR-0069 floor); fast does not.
                        "reasoning": !fast,
                        "options": model_options
                    }
                }
            }
        },
        "model": "eaig/reviewer",
        "plugin": PLUGIN_PATHS,
        // Global read-only posture (defense-in-depth over the per-agent block below).
        "permission": {
            "edit": "deny",
            "bash": "deny",
            "webfetch": "deny"
        },
        // The review write/terminal tools over stdio MCP (ADR-0095 / #440). opencode-over-ACP honors
        // stdio MCP only via the config `mcp` block (NOT `session/new.mcpServers`), so it is wired here;
        // the host sets LCI_MCP_* in the process env the spawned server inherits.
        "mcp": {
            "lightbridge": {
                "type": "local",
                "command": ["lci-review-mcp"],
                "enabled": true
            }
        },
        "agent": {
            "review": {
                "mode": "primary",
                "description": "Lightbridge review: investigates a PR via the mediated retrieval tools \
    and records findings with lightbridge_add_review_comment, then calls lightbridge_finish with a \
    verdict (or lightbridge_abort). Read-only; never edits or runs commands.",
                "prompt": system_prompt,
                "permission": {
                    "edit": "deny",
                    "bash": "deny",
                    "webfetch": "deny"
                },
                "tools": disabled_tools
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wires_stdio_review_mcp_and_embeds_the_prompt() {
        let config = render_review_config("be a careful reviewer", false, None);
        assert_eq!(config["mcp"]["lightbridge"]["type"], "local");
        assert_eq!(config["mcp"]["lightbridge"]["command"][0], "lci-review-mcp");
        assert_eq!(config["agent"]["review"]["prompt"], "be a careful reviewer");
        assert_eq!(config["model"], "eaig/reviewer");
        // Secrets are placeholders, never inlined.
        assert_eq!(
            config["provider"]["eaig"]["options"]["apiKey"],
            "{env:LCI_EAIG_API_KEY}"
        );
    }

    #[test]
    fn disables_builtin_file_and_exec_tools_for_mediated_coverage() {
        let config = render_review_config("p", false, None);
        let tools = &config["agent"]["review"]["tools"];
        // Every built-in that could read the tree off the mediated path is off, so all reads flow
        // through lightbridge_read_file (exact coverage accounting).
        for builtin in ["read", "grep", "glob", "list", "edit", "bash", "task"] {
            assert_eq!(tools[builtin], false, "{builtin} must be disabled");
        }
        // Read-only posture.
        assert_eq!(config["agent"]["review"]["permission"]["edit"], "deny");
        assert_eq!(config["agent"]["review"]["permission"]["bash"], "deny");
    }

    #[test]
    fn deep_tier_enables_reasoning_fast_does_not() {
        let deep = render_review_config("p", false, None);
        let fast = render_review_config("p", true, None);
        assert_eq!(
            deep["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            true
        );
        assert_eq!(
            fast["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            false
        );
    }

    #[test]
    fn threads_temperature_only_when_set() {
        let with = render_review_config("p", false, Some(0.2));
        let without = render_review_config("p", false, None);
        assert_eq!(
            with["provider"]["eaig"]["models"]["reviewer"]["options"]["temperature"],
            0.2
        );
        assert!(
            without["provider"]["eaig"]["models"]["reviewer"]["options"]
                .get("temperature")
                .is_none()
        );
    }
}
