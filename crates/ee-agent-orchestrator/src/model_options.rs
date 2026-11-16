//! Request-scoped model options and the validation they need before send.
//!
//! Two knobs the reference agent exposes and EE previously hardcoded live
//! here: tool-call control (`tool_choice` variants plus `parallel_tool_calls`)
//! and structured output. Adapters translate [`ToolCallControl`] into their
//! dialect-native field and emit [`ResponseFormat`] only where a route
//! advertises native support; the portable path is the hidden forced tool in
//! [`crate::structured_output`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolDefinition;

/// Which tool (if any) the model must call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolChoice {
    /// The model decides (provider default).
    #[default]
    Auto,
    /// Tools are advertised but the model must answer in text.
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call this exact tool.
    Tool {
        /// Tool name the model must call.
        name: String,
    },
}

/// Per-request tool-call control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCallControl {
    /// Which tool the model may or must call.
    pub choice: ToolChoice,
    /// Whether the model may emit parallel calls. `None` omits the field so
    /// strict providers do not reject an unknown body key.
    pub parallel: Option<bool>,
}

impl ToolCallControl {
    /// The provider default: the model decides, parallel calls unset.
    #[must_use]
    pub fn auto() -> Self {
        Self::default()
    }

    /// The model must call at least one tool.
    #[must_use]
    pub fn required() -> Self {
        Self { choice: ToolChoice::Required, parallel: None }
    }

    /// The model must call `name`.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self { choice: ToolChoice::Tool { name: name.into() }, parallel: None }
    }

    /// Sets the parallel-call policy.
    #[must_use]
    pub fn with_parallel(mut self, parallel: Option<bool>) -> Self {
        self.parallel = parallel;
        self
    }

    /// Whether the default `auto` control is in effect.
    #[must_use]
    pub fn is_auto(&self) -> bool {
        self.choice == ToolChoice::Auto && self.parallel.is_none()
    }

    /// Validates that a named tool is actually advertised.
    ///
    /// # Errors
    ///
    /// Returns guidance naming the missing tool, or noting that a forced choice
    /// cannot be expressed without any tools.
    pub fn validate(&self, tools: &[ToolDefinition]) -> Result<(), String> {
        match &self.choice {
            ToolChoice::Auto => Ok(()),
            ToolChoice::None => Ok(()),
            ToolChoice::Required if tools.is_empty() => {
                Err(String::from("tool_choice \"required\" needs at least one advertised tool"))
            }
            ToolChoice::Required => Ok(()),
            ToolChoice::Tool { name } => {
                if tools.iter().any(|tool| tool.name == *name) {
                    Ok(())
                } else {
                    Err(format!("tool_choice names {name:?}, which is not an advertised tool"))
                }
            }
        }
    }
}

/// Structured-output request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseFormat {
    /// Any JSON object.
    JsonObject,
    /// A JSON schema the answer must satisfy.
    JsonSchema {
        /// Schema name reported to providers that support native formatting.
        name: String,
        /// JSON schema for the object.
        schema: Value,
        /// Whether the provider should enforce the schema strictly.
        strict: bool,
    },
}

/// Names the first tool that appeared in `current` but not in `first`, or the
/// marker `"(removed)"` when the change was a removal only.
///
/// Used by the loop to describe a mid-conversation tool-list change on routes
/// that cannot honour it.
#[must_use]
pub fn tool_list_change(first: &[String], current: &[String]) -> Option<String> {
    if first == current {
        return None;
    }
    Some(
        current
            .iter()
            .find(|name| !first.contains(name))
            .cloned()
            .unwrap_or_else(|| String::from("(removed)")),
    )
}

/// Rejects duplicate tool names before a request is sent.
///
/// A duplicate is a configuration defect, never a provider error, so it fails
/// locally with the offending name.
///
/// # Errors
///
/// Returns the first duplicated tool name.
pub fn validate_tool_names(tools: &[ToolDefinition]) -> Result<(), String> {
    for (index, tool) in tools.iter().enumerate() {
        if tools[index + 1..].iter().any(|other| other.name == tool.name) {
            return Err(format!("duplicate tool name: {}", tool.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition::new(name, "test tool")
    }

    #[test]
    fn duplicate_tool_names_fail_before_send() {
        let error = validate_tool_names(&[tool("a"), tool("b"), tool("a")]).expect_err("duplicate");
        assert!(error.contains("duplicate tool name: a"), "{error}");
        assert!(validate_tool_names(&[tool("a"), tool("b")]).is_ok());
    }

    #[test]
    fn named_choice_requires_the_tool_to_be_advertised() {
        let tools = [tool("read_file")];
        assert!(ToolCallControl::named("read_file").validate(&tools).is_ok());
        let error = ToolCallControl::named("missing").validate(&tools).expect_err("unknown tool");
        assert!(error.contains("not an advertised tool"), "{error}");
        assert!(ToolCallControl::required().validate(&[]).is_err());
        assert!(ToolCallControl::auto().validate(&[]).is_ok());
    }

    #[test]
    fn default_control_is_auto_without_a_parallel_flag() {
        let control = ToolCallControl::default();
        assert!(control.is_auto());
        assert_eq!(control.parallel, None);
        assert!(!control.with_parallel(Some(false)).is_auto());
    }

    #[test]
    fn tool_list_change_names_the_added_or_removed_tool() {
        let first = vec![String::from("a"), String::from("b")];
        assert_eq!(tool_list_change(&first, &first), None);
        assert_eq!(
            tool_list_change(&first, &[String::from("a"), String::from("b"), String::from("c")]),
            Some(String::from("c"))
        );
        assert_eq!(tool_list_change(&first, &[String::from("a")]), Some(String::from("(removed)")));
    }

    #[test]
    fn response_format_serializes_the_schema_verbatim() {
        let format = ResponseFormat::JsonSchema {
            name: String::from("verdict"),
            schema: json!({ "type": "object" }),
            strict: true,
        };
        let round_trip =
            serde_json::from_value::<ResponseFormat>(serde_json::to_value(&format).expect("serde"))
                .expect("round trip");
        assert_eq!(round_trip, format);
    }
}
