//! Portable structured output through a hidden forced tool.
//!
//! The reference agent does not rely on cross-provider `response_format`
//! support: it advertises a `generate_object` tool whose input is the requested
//! schema, forces the model to call it, and decodes the arguments. That path
//! works on every dialect EE speaks, so it is the parity implementation here.
//! Native response formatting stays an optional optimisation.

use serde_json::{Value, json};

use crate::model::{ModelRequest, ModelResponse};
use crate::model_options::{ResponseFormat, ToolCallControl, validate_tool_names};
use crate::tools::ToolDefinition;

/// Hidden tool the model must call to answer a structured-output request.
pub const OBJECT_TOOL: &str = "generate_object";

/// Description advertised on the hidden tool.
pub const OBJECT_TOOL_DESCRIPTION: &str = "Return the structured result by calling this tool.";

/// Adds the hidden tool and forces the model to call it.
///
/// The tool list is validated first, so a duplicate name fails here instead of
/// reaching a provider.
///
/// # Errors
///
/// Returns guidance when the tool list is invalid or the reserved name is
/// already taken.
pub fn prepare(request: &mut ModelRequest) -> Result<(), String> {
    let Some(format) = request.response_format.clone() else {
        return Ok(());
    };
    validate_tool_names(&request.tools)?;
    if request.tools.iter().any(|tool| tool.name == OBJECT_TOOL) {
        return Err(format!("`{OBJECT_TOOL}` is reserved for structured output"));
    }
    let (schema, strict) = match &format {
        ResponseFormat::JsonObject => (json!({ "type": "object" }), false),
        ResponseFormat::JsonSchema { schema, strict, .. } => (schema.clone(), *strict),
    };
    let mut definition =
        ToolDefinition::new(OBJECT_TOOL, OBJECT_TOOL_DESCRIPTION).input_schema(schema);
    if strict {
        definition = definition.strict();
    }
    let had_tools = !request.tools.is_empty();
    request.tools.push(definition);
    // With other tools present the model must call some tool; otherwise the
    // hidden tool is the only candidate, so name it explicitly as the
    // reference does.
    request.tool_control =
        if had_tools { ToolCallControl::required() } else { ToolCallControl::named(OBJECT_TOOL) };
    Ok(())
}

/// Takes the decoded object off a response, if the request asked for one.
///
/// The hidden tool intent is removed from the turn so the loop never executes
/// it, and the response completes when it was the only intent.
///
/// # Errors
///
/// Returns guidance when the model never called the forced tool, or when the
/// arguments do not satisfy the requested schema.
pub fn take_object(
    request: &ModelRequest,
    response: &mut ModelResponse,
) -> Result<Option<Value>, String> {
    if request.response_format.is_none() {
        return Ok(None);
    }
    let Some(position) = response.tool_intents.iter().position(|intent| intent.name == OBJECT_TOOL)
    else {
        // With other tools advertised, a step that calls them is an
        // intermediate step: the reference decodes only when no client-executed
        // call remains.
        if !response.tool_intents.is_empty() {
            return Ok(None);
        }
        // A cut-off call arrives truncated, so the real cause is the limit.
        if response.finish_reason.as_deref() == Some("length") {
            return Err(String::from(
                "output hit the token limit before the forced tool call completed",
            ));
        }
        return Err(format!("model did not call the forced `{OBJECT_TOOL}` tool"));
    };
    let intent = response.tool_intents.remove(position);
    validate_object(request.response_format.as_ref(), &intent.arguments)?;
    response.structured_output = Some(intent.arguments.clone());
    if response.tool_intents.is_empty() {
        response.completed = true;
    }
    Ok(Some(intent.arguments))
}

fn validate_object(format: Option<&ResponseFormat>, arguments: &Value) -> Result<(), String> {
    match format {
        Some(ResponseFormat::JsonObject) => {
            if arguments.is_object() {
                Ok(())
            } else {
                Err(String::from("structured output must be a JSON object"))
            }
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            ToolDefinition::new(OBJECT_TOOL, OBJECT_TOOL_DESCRIPTION)
                .input_schema(schema.clone())
                .validate_arguments(arguments)
                .map_err(|error| format!("structured output does not match the schema: {error}"))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{ModelMessage, ModelRole, ModelUsage};
    use crate::model_options::ToolChoice;
    use crate::tasks::{TaskId, TaskNode};
    use crate::tools::ToolIntent;

    fn request(format: ResponseFormat, tools: Vec<ToolDefinition>) -> ModelRequest {
        let mut request = ModelRequest::new(
            vec![ModelMessage::text(ModelRole::User, "classify")],
            tools,
            crate::budget::BudgetTracker::new(&crate::config::OrchestratorConfig::default())
                .snapshot(),
            TaskNode::new(TaskId::new("t"), "task", "task"),
        );
        request.response_format = Some(format);
        request
    }

    fn verdict_schema() -> ResponseFormat {
        ResponseFormat::JsonSchema {
            name: String::from("verdict"),
            schema: json!({
                "type": "object",
                "properties": { "risk": { "type": "string" } },
                "required": ["risk"]
            }),
            strict: true,
        }
    }

    #[test]
    fn prepare_adds_the_hidden_tool_and_forces_it() {
        let mut request = request(verdict_schema(), Vec::new());
        prepare(&mut request).expect("prepare");

        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, OBJECT_TOOL);
        assert!(request.tools[0].strict);
        assert_eq!(
            request.tool_control.choice,
            ToolChoice::Tool { name: String::from(OBJECT_TOOL) }
        );
    }

    #[test]
    fn prepare_requires_a_tool_when_other_tools_exist() {
        let mut request =
            request(ResponseFormat::JsonObject, vec![ToolDefinition::new("read_file", "reads")]);
        prepare(&mut request).expect("prepare");

        assert_eq!(request.tools.len(), 2);
        assert_eq!(request.tool_control.choice, ToolChoice::Required);
    }

    #[test]
    fn prepare_rejects_duplicate_and_reserved_names() {
        let mut duplicate = request(
            ResponseFormat::JsonObject,
            vec![ToolDefinition::new("a", "a"), ToolDefinition::new("a", "a")],
        );
        let error = prepare(&mut duplicate).expect_err("duplicate");
        assert!(error.contains("duplicate tool name"), "{error}");

        let mut reserved =
            request(ResponseFormat::JsonObject, vec![ToolDefinition::new(OBJECT_TOOL, "taken")]);
        let error = prepare(&mut reserved).expect_err("reserved");
        assert!(error.contains("reserved"), "{error}");
    }

    #[test]
    fn take_object_decodes_and_completes_the_turn() {
        let request = request(verdict_schema(), Vec::new());
        let mut response = ModelResponse::new()
            .tool_intents(vec![ToolIntent::new("call_1", OBJECT_TOOL, json!({ "risk": "low" }))])
            .with_usage(ModelUsage::new());

        let object = take_object(&request, &mut response).expect("decodes").expect("object");

        assert_eq!(object["risk"], "low");
        assert_eq!(response.structured_output.as_ref().expect("stored")["risk"], "low");
        assert!(response.tool_intents.is_empty(), "the hidden call is not executed");
        assert!(response.completed);
    }

    #[test]
    fn missing_or_invalid_calls_fail_with_guidance() {
        let request = request(verdict_schema(), Vec::new());
        let mut truncated = ModelResponse::new().with_finish_reason(Some(String::from("length")));
        let error = take_object(&request, &mut truncated).expect_err("truncated");
        assert!(error.contains("token limit"), "{error}");

        let mut silent = ModelResponse::new();
        let error = take_object(&request, &mut silent).expect_err("silent");
        assert!(error.contains("did not call"), "{error}");

        let mut invalid = ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "call_1",
            OBJECT_TOOL,
            json!({ "wrong": 1 }),
        )]);
        let error = take_object(&request, &mut invalid).expect_err("invalid");
        assert!(error.contains("does not match the schema"), "{error}");
        assert_eq!(invalid.structured_output, None);
    }

    #[test]
    fn an_intermediate_tool_step_is_not_a_decode_failure() {
        let request = request(verdict_schema(), vec![ToolDefinition::new("read_file", "reads")]);
        let mut intermediate = ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "call_1",
            "read_file",
            json!({}),
        )]);

        assert_eq!(take_object(&request, &mut intermediate).expect("skips"), None);
        assert_eq!(intermediate.tool_intents.len(), 1, "the normal tool still runs");
    }

    #[test]
    fn a_request_without_a_format_is_untouched() {
        let mut request = request(ResponseFormat::JsonObject, Vec::new());
        request.response_format = None;
        prepare(&mut request).expect("prepare");
        assert!(request.tools.is_empty());

        let mut response = ModelResponse::new();
        assert_eq!(take_object(&request, &mut response).expect("skips"), None);
    }
}
