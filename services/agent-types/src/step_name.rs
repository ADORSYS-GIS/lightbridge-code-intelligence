//! Stable, journal-safe names for agent workflow steps.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

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
    use super::{StepName, step_names};

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
}
