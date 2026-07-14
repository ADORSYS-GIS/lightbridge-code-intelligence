//! Wire-format contracts for tool calls, tool specs, and assistant turns.

use serde::{Deserialize, Serialize};

/// A tool call requested by an assistant turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallReq {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCallReq,
    /// Provider-specific state that must be echoed verbatim on the next model turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_content: Option<serde_json::Value>,
}

/// The function name and JSON-encoded arguments within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionCallReq {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

fn function_kind() -> String {
    "function".to_string()
}

/// A tool advertised to a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

/// The function metadata and JSON Schema within a [`ToolSpec`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    /// Build a function-type tool specification.
    #[must_use]
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }

    /// Return the stable dispatched name of this tool.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.function.name
    }
}

/// The model-facing result of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolOutcome {
    Continue(String),
    Finish,
    Abort(String),
}

/// The model-visible portion of one assistant completion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
}

/// Compatibility aliases retained while the current Job loop moves in R1d/R1e.
pub type ToolCall = ToolCallReq;
pub type FunctionCall = FunctionCallReq;
pub type ToolDef = ToolSpec;
pub type FunctionDef = FunctionSpec;

#[cfg(test)]
mod tests {
    use super::{AssistantTurn, FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};

    #[test]
    fn tool_contracts_preserve_the_openai_wire_shape() {
        let spec = ToolSpec::function(
            "search",
            "Search code",
            serde_json::json!({"type": "object"}),
        );
        assert_eq!(spec.name(), "search");
        assert_eq!(serde_json::to_value(&spec).unwrap()["type"], "function");

        let call: ToolCallReq = serde_json::from_value(serde_json::json!({
            "id": "call-1",
            "function": {"name": "search"}
        }))
        .unwrap();
        assert_eq!(call.kind, "function");
        assert_eq!(call.function.arguments, "");

        let round_trip: ToolCallReq =
            serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert_eq!(round_trip, call);
    }

    #[test]
    fn assistant_turn_and_outcomes_are_typed_and_serializable() {
        let turn = AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: "finish".into(),
                    arguments: r#"{"summary":"done"}"#.into(),
                },
                extra_content: None,
            }],
        };
        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains("finish"));
        assert_eq!(ToolOutcome::Finish, ToolOutcome::Finish);
        assert_eq!(
            ToolOutcome::Continue("ok".into()),
            serde_json::from_str(r#"{"Continue":"ok"}"#).unwrap()
        );
    }
}
