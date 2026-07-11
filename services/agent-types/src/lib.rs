//! Shared data contracts for Lightbridge agent runtimes.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

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

/// A stable name for a journaled agent step.
///
/// Completed workflow journals persist these values, so existing names must never be renamed or
/// reformatted in a patch release. Add new names instead.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct StepName(Cow<'static, str>);

impl StepName {
    /// Return the name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for StepName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

impl From<&'static str> for StepName {
    fn from(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }
}

impl From<String> for StepName {
    fn from(name: String) -> Self {
        Self(Cow::Owned(name))
    }
}

impl fmt::Display for StepName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable names for the journaled agent workflow steps.
pub mod step_names {
    use super::StepName;

    macro_rules! step_names {
        (
            constants { $( $constant:ident => $constant_value:literal ),+ $(,)? }
            formatted { $( $function:ident ( $( $argument:ident : $argument_type:ty ),* ) => $format:literal ),+ $(,)? }
        ) => {
            $( pub const $constant: &str = $constant_value; )+

            $(
                #[must_use]
                pub fn $function($( $argument: $argument_type ),*) -> StepName {
                    StepName::from(format!($format))
                }
            )+

            #[cfg(test)]
            pub(super) const STABLE_PATTERNS: &[&str] = &[
                $( $constant_value, )+
                $( $format, )+
            ];
        };
    }

    step_names! {
        constants {
            BOOTSTRAP => "bootstrap",
            FINALIZE => "finalize",
        }
        formatted {
            llm_turn(turn: usize) => "llm_turn:{turn}",
            tools(turn: usize) => "tools:{turn}",
            write_tool(turn: usize, call_id: &str) => "tool:{turn}:{call_id}",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AssistantTurn, FunctionCallReq, StepName, ToolCallReq, ToolOutcome, ToolSpec, step_names,
    };

    #[test]
    fn journaled_step_name_contract_is_stable() {
        assert_eq!(
            step_names::STABLE_PATTERNS,
            [
                "bootstrap",
                "finalize",
                "llm_turn:{turn}",
                "tools:{turn}",
                "tool:{turn}:{call_id}",
            ],
            "journaled step names are an ADR-0082 compatibility contract; add names instead of changing existing ones",
        );
    }

    #[test]
    fn formatted_step_names_include_their_stable_identifiers() {
        let bootstrap = StepName::from(step_names::BOOTSTRAP);
        assert_eq!(bootstrap.to_string(), "bootstrap");
        assert_eq!(step_names::FINALIZE, "finalize");
        assert_eq!(step_names::llm_turn(7).as_str(), "llm_turn:7");
        assert_eq!(step_names::tools(7).as_str(), "tools:7");
        assert_eq!(
            step_names::write_tool(7, "call-42").as_str(),
            "tool:7:call-42"
        );
    }

    #[test]
    fn step_name_serialization_is_transparent() {
        let encoded = serde_json::to_string(&step_names::write_tool(3, "abc")).unwrap();
        assert_eq!(encoded, r#""tool:3:abc""#);

        let decoded: StepName = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, step_names::write_tool(3, "abc"));
    }

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
