//! OpenRouter HTTP request/response mapping.
//!
//! Request body shape (model/messages/tools/tool_choice), reasoning-effort
//! insertion, response extraction, and HTTP error extraction all live here
//! as pure functions; [`call_openrouter`] performs the single HTTP round trip
//! through the async `reqwest` client.  The API key appears only in the
//! Authorization header and never in logs.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::config::Config;
use ee_acp_agent_server::ProviderError;

/// One assistant message decoded from an OpenRouter response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRouterMessage {
    /// Plain-text content of the final answer (may be empty).
    pub content: String,
    /// Reasoning text, if the model emitted any.
    pub reasoning: String,
    /// The raw `message` object, appended to the conversation for tool
    /// rounds.
    pub raw: Value,
    /// Tool calls the model requested.
    pub tool_calls: Vec<OpenRouterToolCall>,
    /// `choices[0].finish_reason` (`stop`, `tool_calls`, `length`, ...);
    /// `None` when the API omits it.
    pub finish_reason: Option<String>,
}

/// One tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRouterToolCall {
    /// Model-assigned tool call id, echoed back on the tool result.
    pub id: String,
    /// Tool name, e.g. `tool_read_file`.
    pub name: String,
    /// Parsed tool arguments object.
    pub arguments: Value,
}

/// Performs one chat-completions round trip.
///
/// Non-success HTTP statuses are converted to a provider backend failure via
/// [`openrouter_error_message`]; the key is never logged.  `tools` is the
/// tool schema advertised to the model this round.
pub(crate) async fn call_openrouter(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<OpenRouterMessage, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|error| {
            ProviderError::BackendFailure(format!(
                "invalid OPENROUTER_API_KEY header value: {error}"
            ))
        })?,
    );
    headers.insert(
        HeaderName::from_static("x-title"),
        HeaderValue::from_str(&config.app_title).map_err(|error| {
            ProviderError::BackendFailure(format!("invalid OpenRouter app title: {error}"))
        })?,
    );
    if let Some(site_url) = &config.site_url {
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_str(site_url).map_err(|error| {
                ProviderError::BackendFailure(format!(
                    "invalid OpenRouter site URL header: {error}"
                ))
            })?,
        );
    }

    let body = openrouter_request_body_with_tools(config, messages, tools);
    let response =
        http.post(&config.api_url).headers(headers).json(&body).send().await.map_err(|error| {
            ProviderError::BackendFailure(format!("OpenRouter request failed: {error}"))
        })?;
    let status = response.status();
    let value = response.json::<Value>().await.map_err(|error| {
        ProviderError::BackendFailure(format!("OpenRouter response was not JSON: {error}"))
    })?;
    if !status.is_success() {
        return Err(ProviderError::BackendFailure(openrouter_error_message(
            status.as_u16(),
            &value,
        )));
    }
    extract_openrouter_message(&value).ok_or_else(|| {
        ProviderError::BackendFailure(format!(
            "OpenRouter response did not include choices[0].message: {value}"
        ))
    })
}

/// Builds the chat-completions request body with the default read-file tool
/// schema (used by the simple provider and by tests).
#[cfg(test)]
pub(crate) fn openrouter_request_body(config: &Config, messages: &[Value]) -> Value {
    openrouter_request_body_with_tools(config, messages, &openrouter_tools())
}

/// Builds the chat-completions request body: model, messages, stream=false,
/// the given tool schemas, and `tool_choice: auto`; inserts
/// `reasoning.effort` when configured.  The `tools` member is omitted when
/// no tools are available.
pub(crate) fn openrouter_request_body_with_tools(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
) -> Value {
    let mut body = json!({
        "model": config.model,
        "messages": messages,
        "stream": false,
        "tool_choice": "auto",
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    if let Some(effort) = config.reasoning_effort.as_deref().filter(|effort| !effort.is_empty())
        && let Some(object) = body.as_object_mut()
    {
        object.insert(String::from("reasoning"), json!({ "effort": effort }));
    }
    body
}

/// The tool schema advertised to the model.
pub(crate) fn openrouter_tools() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "tool_read_file",
            "description": "Read a UTF-8 text file from the current ee workspace. Use this instead of printing tool syntax.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path, or path relative to the session cwd."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })]
}

/// Decodes `choices[0].message`; returns `None` when the shape is missing.
pub(crate) fn extract_openrouter_message(value: &Value) -> Option<OpenRouterMessage> {
    let message = value.pointer("/choices/0/message")?;
    let content = extract_openrouter_content(message.get("content").unwrap_or(&Value::Null));
    let reasoning = extract_openrouter_reasoning(message);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(extract_openrouter_tool_call).collect())
        .unwrap_or_default();
    let finish_reason =
        value.pointer("/choices/0/finish_reason").and_then(Value::as_str).map(str::to_string);
    Some(OpenRouterMessage { content, reasoning, raw: message.clone(), tool_calls, finish_reason })
}

/// Extracts reasoning text from the first present of `reasoning`,
/// `reasoning_content`, or `thinking`.
pub(crate) fn extract_openrouter_reasoning(message: &Value) -> String {
    for pointer in ["/reasoning", "/reasoning_content", "/thinking"] {
        if let Some(text) = message.pointer(pointer).and_then(Value::as_str)
            && !text.is_empty()
        {
            return text.to_string();
        }
    }
    String::new()
}

/// Extracts plain text from a string or a parts array of text chunks.
pub(crate) fn extract_openrouter_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        return parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
    }
    String::new()
}

/// Decodes one tool call; `arguments` may arrive as a JSON string or object.
pub(crate) fn extract_openrouter_tool_call(value: &Value) -> Option<OpenRouterToolCall> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let function = value.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.to_string();
    let arguments = match function.get("arguments")? {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        value => value.clone(),
    };
    Some(OpenRouterToolCall { id, name, arguments })
}

/// Builds the failure message for a non-success HTTP status.
pub(crate) fn openrouter_error_message(status: u16, value: &Value) -> String {
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        format!("OpenRouter HTTP {status}: {message}")
    } else {
        format!("OpenRouter HTTP {status}: {value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            model: String::from("test/model"),
            api_url: String::from(crate::config::DEFAULT_API_URL),
            api_key: None,
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: std::time::Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated: false,
        }
    }

    #[test]
    fn extracts_openrouter_string_answer() {
        let value = json!({ "choices": [{ "message": { "content": "hi" } }] });

        assert_eq!(extract_openrouter_message(&value).unwrap().content, "hi");
    }

    #[test]
    fn extracts_openrouter_reasoning_answer() {
        let value = json!({
            "choices": [{
                "message": {
                    "reasoning": "check config first",
                    "content": "answer"
                }
            }]
        });

        let message = extract_openrouter_message(&value).unwrap();
        assert_eq!(message.reasoning, "check config first");
        assert_eq!(message.content, "answer");
    }

    #[test]
    fn extracts_openrouter_finish_reason() {
        let value = json!({
            "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }]
        });

        assert_eq!(
            extract_openrouter_message(&value).unwrap().finish_reason.as_deref(),
            Some("stop")
        );
        assert_eq!(
            extract_openrouter_message(&json!({ "choices": [{ "message": {} }] }))
                .unwrap()
                .finish_reason,
            None
        );
    }

    #[test]
    fn openrouter_request_body_with_tools_omits_empty_tools() {
        let config = test_config();
        let body = openrouter_request_body_with_tools(
            &config,
            &[json!({ "role": "user", "content": "hi" })],
            &[],
        );
        assert!(body.get("tools").is_none(), "no tools member when empty: {body}");
    }

    #[test]
    fn openrouter_request_body_includes_reasoning_effort_when_configured() {
        let mut config = test_config();
        config.reasoning_effort = Some(String::from("low"));

        let body = openrouter_request_body(&config, &[json!({ "role": "user", "content": "hi" })]);

        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn openrouter_request_body_keeps_model_and_tool_shape() {
        let config = test_config();
        let body = openrouter_request_body(&config, &[json!({ "role": "user", "content": "hi" })]);

        assert_eq!(body["model"], "test/model");
        assert_eq!(body["stream"], false);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "tool_read_file");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn extracts_openrouter_tool_call_arguments() {
        let value = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "tool_read_file",
                            "arguments": "{\"path\":\".ee.toml\"}"
                        }
                    }]
                }
            }]
        });

        let message = extract_openrouter_message(&value).unwrap();
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].name, "tool_read_file");
        assert_eq!(message.tool_calls[0].arguments["path"], ".ee.toml");
    }

    #[test]
    fn openrouter_http_errors_extract_message() {
        let value = json!({ "error": { "message": "rate limited" } });
        assert_eq!(openrouter_error_message(429, &value), "OpenRouter HTTP 429: rate limited");
        assert!(openrouter_error_message(500, &json!({ "detail": "boom" })).contains("500"));
    }

    #[test]
    fn missing_message_shape_yields_none() {
        assert!(extract_openrouter_message(&json!({ "choices": [] })).is_none());
    }
}
