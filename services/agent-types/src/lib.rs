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

/// One message in the model conversation — the host-agnostic wire shape shared by every agent host.
///
/// `Eq` is deliberately *not* derived: [`ToolCallReq::extra_content`] holds an opaque
/// `serde_json::Value` (only `PartialEq`) so a provider's round-trip blob can ride along.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
    /// Set only on `role = "tool"` messages — ties a tool result back to the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// A `system` message — the reviewer guidance + output contract.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    /// A `user` message — the requested command + the diff/context.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    /// A `tool` message carrying the result of a tool call back to the model.
    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// How an agent loop ended, independent of any review-specific flavor (ADR-0082). The review
/// assembly maps this onto its own artifact-finalizing outcome (`Finished`/`Exhausted`/`Aborted`).
///
/// The tagged representation is chosen so the wire form matches the pre-extraction loop's outcome
/// JSON exactly (`{"status":"finished"}`, `{"status":"exhausted"}`, `{"status":"aborted","reason":…}`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoopOutcome {
    Finished,
    Exhausted,
    Aborted { reason: String },
}

/// One recorded event in an agent run — the ADR-0034/0060 capture seam, host-agnostic.
///
/// The Job host maps these onto the control-plane transcript DTO at end of run; the durable worker
/// flushes them incrementally. Policy decisions are first-class here — before the extraction they
/// were a test-only observer over the monolithic loop.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// One assistant completion (text and/or tool calls), with the model that produced it.
    Assistant {
        turn: usize,
        assistant: AssistantTurn,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// The result of dispatching one tool call.
    ToolResult {
        turn: usize,
        call: ToolCallReq,
        outcome: ToolOutcome,
    },
    /// A policy decision (wind-down entry, budget drop, history trim, force-finish, …).
    Policy {
        turn: usize,
        name: String,
        detail: serde_json::Value,
    },
}

/// The engine's two retry classes (ADR-0082 §error taxonomy). Everything a step, tool, or model
/// call returns is classified at the source into one of these — replacing today's implicit
/// `anyhow`-chain + string-matching knowledge.
pub mod error {
    use std::fmt;
    use std::time::Duration;

    /// A step failure, classified for the durable runtime's retry policy.
    #[derive(Debug)]
    pub enum StepError {
        /// Worth retrying: transport failures, 5xx, 429 (with an optional server hint), timeouts.
        Transient {
            source: anyhow::Error,
            retry_after: Option<Duration>,
        },
        /// Retrying cannot help: malformed args, unknown tool, refused call, exhausted budget,
        /// context overflow after trim. Maps to Restate's `TerminalError` in R2.
        Terminal { reason: String },
    }

    impl StepError {
        /// A transient failure with no server-provided retry hint.
        #[must_use]
        pub fn transient(source: impl Into<anyhow::Error>) -> Self {
            Self::Transient {
                source: source.into(),
                retry_after: None,
            }
        }

        /// A transient failure carrying the server's `Retry-After` hint.
        #[must_use]
        pub fn transient_after(source: impl Into<anyhow::Error>, retry_after: Duration) -> Self {
            Self::Transient {
                source: source.into(),
                retry_after: Some(retry_after),
            }
        }

        /// A terminal failure that retrying cannot fix.
        #[must_use]
        pub fn terminal(reason: impl Into<String>) -> Self {
            Self::Terminal {
                reason: reason.into(),
            }
        }

        /// Whether the durable runtime should retry the step that produced this error.
        #[must_use]
        pub fn is_transient(&self) -> bool {
            matches!(self, Self::Transient { .. })
        }

        /// The server-advised delay before a retry, when the failure is transient and carried one.
        #[must_use]
        pub fn retry_after(&self) -> Option<Duration> {
            match self {
                Self::Transient { retry_after, .. } => *retry_after,
                Self::Terminal { .. } => None,
            }
        }
    }

    impl fmt::Display for StepError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Transient {
                    source,
                    retry_after,
                } => {
                    write!(f, "transient step failure: {source:#}")?;
                    if let Some(after) = retry_after {
                        write!(f, " (retry after {}s)", after.as_secs())?;
                    }
                    Ok(())
                }
                Self::Terminal { reason } => write!(f, "terminal step failure: {reason}"),
            }
        }
    }

    impl std::error::Error for StepError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Transient { source, .. } => Some(source.as_ref()),
                Self::Terminal { .. } => None,
            }
        }
    }
}

pub use error::StepError;

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

    #[test]
    fn chat_message_constructors_match_the_wire_shape() {
        let system = super::ChatMessage::system("guidance");
        assert_eq!(system.role, "system");
        assert_eq!(system.content.as_deref(), Some("guidance"));
        assert!(system.tool_calls.is_empty());
        assert!(system.tool_call_id.is_none());

        let result = super::ChatMessage::tool("call-7", "output");
        assert_eq!(result.role, "tool");
        assert_eq!(result.tool_call_id.as_deref(), Some("call-7"));

        // `user`/`tool` round-trip through the OpenAI-compatible message shape.
        let encoded = serde_json::to_value(super::ChatMessage::user("hi")).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"role": "user", "content": "hi"})
        );
    }

    #[test]
    fn loop_outcome_serializes_to_the_pre_extraction_status_json() {
        use super::LoopOutcome;
        assert_eq!(
            serde_json::to_value(LoopOutcome::Finished).unwrap(),
            serde_json::json!({"status": "finished"})
        );
        assert_eq!(
            serde_json::to_value(LoopOutcome::Exhausted).unwrap(),
            serde_json::json!({"status": "exhausted"})
        );
        assert_eq!(
            serde_json::to_value(LoopOutcome::Aborted {
                reason: "user asked to stop".into()
            })
            .unwrap(),
            serde_json::json!({"status": "aborted", "reason": "user asked to stop"})
        );
    }

    #[test]
    fn step_error_classifies_and_carries_retry_hints() {
        use super::StepError;
        use std::time::Duration;

        let transient = StepError::transient_after(anyhow::anyhow!("503"), Duration::from_secs(3));
        assert!(transient.is_transient());
        assert_eq!(transient.retry_after(), Some(Duration::from_secs(3)));
        assert!(transient.to_string().contains("retry after 3s"));

        let terminal = StepError::terminal("unknown tool");
        assert!(!terminal.is_transient());
        assert_eq!(terminal.retry_after(), None);
        assert!(terminal.to_string().contains("unknown tool"));

        // The transient variant preserves its source chain for the runtime to log.
        assert!(std::error::Error::source(&transient).is_some());
    }

    #[test]
    fn transcript_entry_variants_are_tagged() {
        use super::{AssistantTurn, TranscriptEntry};
        let assistant = TranscriptEntry::Assistant {
            turn: 0,
            assistant: AssistantTurn {
                content: Some("thinking".into()),
                tool_calls: vec![],
            },
            model: Some("glm".into()),
        };
        assert_eq!(
            serde_json::to_value(&assistant).unwrap()["kind"],
            "assistant"
        );
        let policy = TranscriptEntry::Policy {
            turn: 2,
            name: "wind_down".into(),
            detail: serde_json::json!({"turn": 2}),
        };
        assert_eq!(serde_json::to_value(&policy).unwrap()["kind"], "policy");
    }
}
