//! OpenRouter HTTP request/response and Server-Sent Events mapping.
//!
//! Buffered calls remain available for compacting session history. Normal agent
//! turns use `call_openrouter_streaming` so text and reasoning deltas reach
//! the ACP client while OpenRouter is still generating them.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// The raw `message` object, appended to the conversation for tool rounds.
    pub raw: Value,
    /// Tool calls the model requested.
    pub tool_calls: Vec<OpenRouterToolCall>,
    /// `choices[0].finish_reason` (`stop`, `tool_calls`, `length`, ...).
    pub finish_reason: Option<String>,
    /// Token usage reported for this round trip, when present.
    pub usage: Option<OpenRouterUsage>,
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

/// Token usage reported by OpenRouter for one round trip.
///
/// `None` fields mean OpenRouter did not report them — treated as unknown,
/// never counted as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OpenRouterUsage {
    /// `usage.prompt_tokens`.
    pub input_tokens: Option<u64>,
    /// `usage.completion_tokens`.
    pub output_tokens: Option<u64>,
    /// `usage.total_tokens`; may exceed `input + output` when cached tokens
    /// are billed separately.
    pub total_tokens: Option<u64>,
}

/// One displayable OpenRouter streaming delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenRouterStreamDelta {
    /// Partial assistant text.
    Text(String),
    /// Partial model reasoning.
    Reasoning(String),
}

/// Performs one buffered chat-completions round trip.
///
/// Used by session compaction. Normal agent turns use
/// [`call_openrouter_streaming`] instead.
pub(crate) async fn call_openrouter(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<OpenRouterMessage, ProviderError> {
    let response = send_openrouter_request(
        http,
        config,
        api_key,
        openrouter_request_body_with_tools(config, messages, tools),
    )
    .await?;
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    let value = response.json::<Value>().await.map_err(|error| {
        ProviderError::BackendFailure(format!("OpenRouter response was not JSON: {error}"))
    })?;
    if !status.is_success() {
        return Err(classify_http_error(
            status.as_u16(),
            retry_after,
            openrouter_error_message(status.as_u16(), &value),
        ));
    }
    extract_openrouter_message(&value).ok_or_else(|| {
        ProviderError::BackendFailure(format!(
            "OpenRouter response did not include choices[0].message: {value}"
        ))
    })
}

/// Streams one chat-completions round trip and returns its accumulated result.
///
/// `on_delta` is called in SSE arrival order. Tool-call deltas are accumulated
/// privately until complete, preventing tools from running with partial JSON.
pub(crate) async fn call_openrouter_streaming<F>(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    messages: &[Value],
    tools: &[Value],
    mut on_delta: F,
) -> Result<OpenRouterMessage, ProviderError>
where
    F: FnMut(OpenRouterStreamDelta) -> Result<(), ProviderError>,
{
    let response = send_openrouter_request(
        http,
        config,
        api_key,
        openrouter_stream_request_body_with_tools(config, messages, tools),
    )
    .await?;
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    if !status.is_success() {
        let value = response.json::<Value>().await.map_err(|error| {
            ProviderError::BackendFailure(format!(
                "OpenRouter error response was not JSON: {error}"
            ))
        })?;
        return Err(classify_http_error(
            status.as_u16(),
            retry_after,
            openrouter_error_message(status.as_u16(), &value),
        ));
    }
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/event-stream"));
    if !is_sse {
        let value = response.json::<Value>().await.map_err(|error| {
            ProviderError::BackendFailure(format!(
                "OpenRouter streaming response was neither SSE nor JSON: {error}"
            ))
        })?;
        let answer = extract_openrouter_message(&value).ok_or_else(|| {
            ProviderError::BackendFailure(format!(
                "OpenRouter response did not include choices[0].message: {value}"
            ))
        })?;
        if !answer.reasoning.is_empty() {
            on_delta(OpenRouterStreamDelta::Reasoning(answer.reasoning.clone()))?;
        }
        if !answer.content.is_empty() {
            on_delta(OpenRouterStreamDelta::Text(answer.content.clone()))?;
        }
        return Ok(answer);
    }

    let mut response = response;
    let mut decoder = SseDecoder::default();
    let mut accumulator = StreamAccumulator::default();
    let mut done = false;

    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ProviderError::BackendFailure(format!("OpenRouter streaming response failed: {error}"))
    })? {
        for event in decoder.push(&chunk)? {
            if event == "[DONE]" {
                done = true;
                break;
            }
            let value = serde_json::from_str::<Value>(&event).map_err(|error| {
                ProviderError::BackendFailure(format!(
                    "OpenRouter stream event was not JSON: {error}"
                ))
            })?;
            for delta in accumulator.apply(&value)? {
                on_delta(delta)?;
            }
        }
        if done {
            break;
        }
    }

    for event in decoder.finish()? {
        if event == "[DONE]" {
            break;
        }
        let value = serde_json::from_str::<Value>(&event).map_err(|error| {
            ProviderError::BackendFailure(format!("OpenRouter stream event was not JSON: {error}"))
        })?;
        for delta in accumulator.apply(&value)? {
            on_delta(delta)?;
        }
    }

    accumulator.finish()
}

async fn send_openrouter_request(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    body: Value,
) -> Result<reqwest::Response, ProviderError> {
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
    http.post(&config.api_url).headers(headers).json(&body).send().await.map_err(|error| {
        ProviderError::BackendFailure(format!("OpenRouter request failed: {error}"))
    })
}

/// Builds a buffered chat-completions request body.
#[cfg(test)]
pub(crate) fn openrouter_request_body(config: &Config, messages: &[Value]) -> Value {
    openrouter_request_body_with_tools(config, messages, &openrouter_tools())
}

/// Builds a streaming chat-completions request body.
#[cfg(test)]
pub(crate) fn openrouter_stream_request_body(config: &Config, messages: &[Value]) -> Value {
    openrouter_stream_request_body_with_tools(config, messages, &openrouter_tools())
}

/// Builds a buffered chat-completions request body.
pub(crate) fn openrouter_request_body_with_tools(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
) -> Value {
    openrouter_request_body_with_stream(config, messages, tools, false)
}

/// Builds a streaming chat-completions request body.
pub(crate) fn openrouter_stream_request_body_with_tools(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
) -> Value {
    openrouter_request_body_with_stream(config, messages, tools, true)
}

fn openrouter_request_body_with_stream(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
    stream: bool,
) -> Value {
    let mut body = json!({
        "model": config.model,
        "messages": messages,
        "stream": stream,
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
    Some(OpenRouterMessage {
        content,
        reasoning,
        raw: message.clone(),
        tool_calls,
        finish_reason,
        usage: extract_openrouter_usage(value),
    })
}

/// Extracts `usage.prompt_tokens` / `completion_tokens` / `total_tokens`;
/// returns `None` when the response carries no usage object.
fn extract_openrouter_usage(value: &Value) -> Option<OpenRouterUsage> {
    let usage = value.get("usage")?;
    let input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    if input_tokens.is_none() && output_tokens.is_none() && total_tokens.is_none() {
        return None;
    }
    Some(OpenRouterUsage { input_tokens, output_tokens, total_tokens })
}

/// Extracts reasoning text from first present of `reasoning`,
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

/// Builds failure message for a non-success HTTP status.
pub(crate) fn openrouter_error_message(status: u16, value: &Value) -> String {
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        format!("OpenRouter HTTP {status}: {message}")
    } else {
        format!("OpenRouter HTTP {status}: {value}")
    }
}

/// Classifies a non-success OpenRouter status into a retry decision and a
/// typed provider error.  Structural classification: never string parsing.
#[must_use]
pub(crate) fn classify_http_error(
    status: u16,
    retry_after: Option<Duration>,
    detail: String,
) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited { retry_after, detail },
        // Transient server/network classes; safe to retry only before any
        // response bytes were produced (streaming guards that itself).
        408 | 409 | 425 | 500 | 502 | 503 | 504 | 521 | 522 | 524 => {
            ProviderError::Transient { retry_after, detail }
        }
        // 401/403 are permanent credential/policy problems: never retried,
        // never auto-resumed.
        401 | 403 => ProviderError::BackendFailure(detail),
        _ if status >= 500 => ProviderError::Transient { retry_after, detail },
        _ => ProviderError::InvalidRequest(detail),
    }
}

/// Whether a provider error may be retried (rate limits and transient
/// failures only; never side-effecting retries).
#[must_use]
pub(crate) fn is_retryable(error: &ProviderError) -> bool {
    matches!(error, ProviderError::RateLimited { .. } | ProviderError::Transient { .. })
}

/// Server-provided retry hint, when the error carries one.
#[must_use]
pub(crate) fn retry_after_of(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimited { retry_after, .. }
        | ProviderError::Transient { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Parses a `Retry-After` header value (delta-seconds form; HTTP dates are
/// rare and rejected rather than mis-parsed).
#[must_use]
pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// Retry delay for `attempt` (0-based): the server hint wins when present
/// (capped), otherwise exponential backoff with bounded jitter.
#[must_use]
pub(crate) fn retry_delay(
    config: &Config,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(hint) = retry_after {
        return hint.min(config.retry_max_delay);
    }
    let base = config.retry_base_delay.as_millis() as u64;
    let backoff = base.saturating_mul(1 << attempt.min(10));
    let capped = backoff.min(config.retry_max_delay.as_millis() as u64);
    // Bounded jitter (±20%) so bursts do not retry in lockstep.
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos() as u64)
        % (capped / 5 + 1);
    Duration::from_millis(capped + jitter)
}

/// One buffered chat-completions round trip with transient/429 retries.
/// Retries only when the first attempt produced no response body (the
/// non-success branch reads the body before classifying, so a retry after a
/// buffered error is always safe).
pub(crate) async fn call_openrouter_with_retry(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<OpenRouterMessage, ProviderError> {
    let mut last_error: Option<ProviderError> = None;
    for attempt in 0..=config.retry_max_attempts {
        match call_openrouter(http, config, api_key, messages, tools).await {
            Err(error) if is_retryable(&error) && attempt < config.retry_max_attempts => {
                let hint = retry_after_of(&error);
                last_error = Some(error);
                tokio::time::sleep(retry_delay(config, attempt, hint)).await;
            }
            other => return other,
        }
    }
    Err(last_error.expect("at least one attempt ran"))
}

/// One streaming round trip with transient/429 retries.  A retry only
/// happens when *no* delta was emitted yet: once the stream produced bytes,
/// a retry would duplicate output, so the error surfaces instead.
pub(crate) async fn call_openrouter_streaming_with_retry<F>(
    http: &reqwest::Client,
    config: &Config,
    api_key: &str,
    messages: &[Value],
    tools: &[Value],
    on_delta: &mut F,
) -> Result<OpenRouterMessage, ProviderError>
where
    F: FnMut(OpenRouterStreamDelta) -> Result<(), ProviderError>,
{
    let mut started = false;
    let mut last_error: Option<ProviderError> = None;
    for attempt in 0..=config.retry_max_attempts {
        let started_ref = &mut started;
        let result = call_openrouter_streaming(http, config, api_key, messages, tools, |delta| {
            *started_ref = true;
            on_delta(delta)
        })
        .await;
        match result {
            Err(error)
                if !started && is_retryable(&error) && attempt < config.retry_max_attempts =>
            {
                let hint = retry_after_of(&error);
                last_error = Some(error);
                tokio::time::sleep(retry_delay(config, attempt, hint)).await;
            }
            other => return other,
        }
    }
    Err(last_error.expect("at least one attempt ran"))
}

/// Incremental SSE framing state. Events are decoded only once their blank
/// line terminator has arrived, so split UTF-8 sequences remain intact.
#[derive(Debug, Default)]
struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        self.drain_complete()
    }

    fn finish(&mut self) -> Result<Vec<String>, ProviderError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let event = std::mem::take(&mut self.pending);
        parse_sse_event(&event).map(|event| event.into_iter().collect())
    }

    fn drain_complete(&mut self) -> Result<Vec<String>, ProviderError> {
        let mut events = Vec::new();
        while let Some((event_end, delimiter_len)) = sse_event_boundary(&self.pending) {
            let event: Vec<u8> = self.pending.drain(..event_end).collect();
            self.pending.drain(..delimiter_len);
            if let Some(data) = parse_sse_event(&event)? {
                events.push(data);
            }
        }
        Ok(events)
    }
}

fn sse_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    for index in 0..bytes.len() {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn parse_sse_event(bytes: &[u8]) -> Result<Option<String>, ProviderError> {
    let event = std::str::from_utf8(bytes).map_err(|error| {
        ProviderError::BackendFailure(format!("OpenRouter SSE event was not UTF-8: {error}"))
    })?;
    let data: Vec<&str> = event
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:").map(|data| data.strip_prefix(' ').unwrap_or(data))
        })
        .collect();
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    content: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PendingToolCall>,
    finish_reason: Option<String>,
    /// Latest reported usage; chunks carry cumulative totals, so later
    /// occurrences replace earlier ones.
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamAccumulator {
    fn apply(&mut self, value: &Value) -> Result<Vec<OpenRouterStreamDelta>, ProviderError> {
        if let Some(error) = value.pointer("/error/message").and_then(Value::as_str) {
            return Err(ProviderError::BackendFailure(format!("OpenRouter stream error: {error}")));
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(Vec::new());
        };
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish_reason.to_string());
        }
        if let Some(usage) = extract_openrouter_usage(value) {
            self.usage = Some(usage);
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(Vec::new());
        };
        let mut output = Vec::new();
        if let Some(content) =
            delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty())
        {
            self.content.push_str(content);
            output.push(OpenRouterStreamDelta::Text(content.to_string()));
        }
        let reasoning = extract_openrouter_reasoning(delta);
        if !reasoning.is_empty() {
            self.reasoning.push_str(&reasoning);
            output.push(OpenRouterStreamDelta::Reasoning(reasoning));
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                self.apply_tool_call_delta(call)?;
            }
        }
        Ok(output)
    }

    fn apply_tool_call_delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            ProviderError::BackendFailure("OpenRouter stream tool call missing index".to_string())
        })? as usize;
        let call = self.tool_calls.entry(index).or_default();
        if let Some(id) = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            call.id = Some(id.to_string());
        }
        if let Some(function) = value.get("function") {
            if let Some(name) =
                function.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())
            {
                call.name = Some(name.to_string());
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<OpenRouterMessage, ProviderError> {
        let mut tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (_index, call) in self.tool_calls {
            let id = call.id.ok_or_else(|| {
                ProviderError::BackendFailure("OpenRouter stream tool call missing id".to_string())
            })?;
            let name = call.name.ok_or_else(|| {
                ProviderError::BackendFailure(
                    "OpenRouter stream tool call missing function name".to_string(),
                )
            })?;
            // Keep the call identity so the tool loop can feed an invalid-input
            // observation back to the model. `Null` is never a valid tool argument
            // object, so validation fails before any tool side effect can occur.
            let arguments = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
            tool_calls.push(OpenRouterToolCall { id, name, arguments });
        }
        let raw_tool_calls: Vec<Value> = tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    },
                })
            })
            .collect();
        let mut raw = json!({
            "role": "assistant",
            "content": (!self.content.is_empty()).then_some(self.content.clone()),
        });
        if !raw_tool_calls.is_empty() {
            raw["tool_calls"] = Value::Array(raw_tool_calls);
        }
        if !self.reasoning.is_empty() {
            raw["reasoning"] = Value::String(self.reasoning.clone());
        }
        Ok(OpenRouterMessage {
            content: self.content,
            reasoning: self.reasoning,
            raw,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            model: String::from("test/model"),
            model_family: None,
            rubber_duck_model: None,
            rubber_duck_model_family: None,
            rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
            api_url: String::from(crate::config::DEFAULT_API_URL),
            api_key: None,
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: std::time::Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated: false,
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
            auto_compact_threshold_percent: 80,
            retry_max_attempts: crate::config::DEFAULT_RETRY_MAX_ATTEMPTS,
            retry_base_delay: std::time::Duration::from_millis(
                crate::config::DEFAULT_RETRY_BASE_DELAY_MS,
            ),
            retry_max_delay: std::time::Duration::from_millis(
                crate::config::DEFAULT_RETRY_MAX_DELAY_MS,
            ),
            checkpoint_dir: None,
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        }
    }

    #[test]
    fn extracts_openrouter_string_answer() {
        let value = json!({ "choices": [{ "message": { "content": "hi" } }] });
        assert_eq!(extract_openrouter_message(&value).unwrap().content, "hi");
    }

    #[test]
    fn extracts_openrouter_reasoning_answer() {
        let value = json!({ "choices": [{ "message": { "reasoning": "check config first", "content": "answer" } }] });
        let message = extract_openrouter_message(&value).unwrap();
        assert_eq!(message.reasoning, "check config first");
        assert_eq!(message.content, "answer");
    }

    #[test]
    fn extracts_openrouter_finish_reason() {
        let value =
            json!({ "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }] });
        assert_eq!(
            extract_openrouter_message(&value).unwrap().finish_reason.as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn streaming_request_body_keeps_model_and_tool_shape() {
        let config = test_config();
        let body =
            openrouter_stream_request_body(&config, &[json!({ "role": "user", "content": "hi" })]);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "tool_read_file");
    }

    #[test]
    fn buffered_request_body_stays_buffered_for_compaction() {
        let config = test_config();
        let body = openrouter_request_body(&config, &[json!({ "role": "user", "content": "hi" })]);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn extracts_openrouter_tool_call_arguments() {
        let value = json!({
            "choices": [{ "message": { "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": { "name": "tool_read_file", "arguments": "{\"path\":\".ee.toml\"}" }
            }] } }]
        });
        let message = extract_openrouter_message(&value).unwrap();
        assert_eq!(message.tool_calls[0].arguments["path"], ".ee.toml");
    }

    #[test]
    fn extracts_openrouter_usage_fields() {
        let value = json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": {
                "prompt_tokens": 6120,
                "completion_tokens": 2311,
                "total_tokens": 8431,
            }
        });
        let usage = extract_openrouter_message(&value).unwrap().usage.expect("usage parsed");
        assert_eq!(usage.input_tokens, Some(6120));
        assert_eq!(usage.output_tokens, Some(2311));
        assert_eq!(usage.total_tokens, Some(8431));
    }

    #[test]
    fn missing_usage_stays_unknown_not_zero() {
        let value = json!({ "choices": [{ "message": { "content": "hi" } }] });
        assert_eq!(extract_openrouter_message(&value).unwrap().usage, None);
    }

    #[test]
    fn partial_usage_keeps_only_known_fields() {
        let value = json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "total_tokens": 100 }
        });
        let usage = extract_openrouter_message(&value).unwrap().usage.expect("usage parsed");
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.total_tokens, Some(100));
    }

    #[test]
    fn stream_accumulator_keeps_latest_usage_chunk() {
        let mut accumulator = StreamAccumulator::default();
        accumulator
            .apply(&json!({ "choices": [{ "delta": { "content": "hi" } }], "usage": {
                "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15
            } }))
            .unwrap();
        accumulator
            .apply(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }], "usage": {
                "prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29
            } }))
            .unwrap();
        let message = accumulator.finish().unwrap();
        let usage = message.usage.expect("usage parsed");
        assert_eq!(usage.input_tokens, Some(20), "later cumulative usage wins");
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.total_tokens, Some(29));
    }

    #[test]
    fn decoder_handles_fragmented_utf8_and_multiple_events() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"").unwrap().is_empty()
        );
        assert!(decoder.push("\u{00e9}".as_bytes()).unwrap().is_empty());
        let events = decoder
            .push(b"\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n")
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains('é'));
    }

    #[test]
    fn decoder_ignores_comments_and_accepts_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b": OPENROUTER PROCESSING\r\n\r\ndata: first\r\ndata: second\r\n\r\n")
            .unwrap();
        assert_eq!(events, vec![String::from("first\nsecond")]);
    }

    #[test]
    fn stream_accumulator_emits_deltas_and_reassembles_tool_calls() {
        let mut accumulator = StreamAccumulator::default();
        let first = accumulator
            .apply(&json!({ "choices": [{ "delta": {
                "reasoning_content": "check ",
                "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "tool_read_file", "arguments": "{\"path\":\"" } }]
            } }] }))
            .unwrap();
        let second = accumulator
            .apply(&json!({ "choices": [{ "delta": {
                "content": "working",
                "tool_calls": [{ "index": 0, "function": { "arguments": "Cargo.toml\"}" } }]
            }, "finish_reason": "tool_calls" }] }))
            .unwrap();
        assert_eq!(first, vec![OpenRouterStreamDelta::Reasoning(String::from("check "))]);
        assert_eq!(second, vec![OpenRouterStreamDelta::Text(String::from("working"))]);
        let message = accumulator.finish().unwrap();
        assert_eq!(message.content, "working");
        assert_eq!(message.reasoning, "check ");
        assert_eq!(message.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(message.tool_calls[0].arguments["path"], "Cargo.toml");
        assert_eq!(
            message.raw["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"Cargo.toml\"}"
        );
    }

    #[test]
    fn stream_accumulator_recovers_malformed_tool_arguments_as_invalid_input() {
        let mut accumulator = StreamAccumulator::default();
        accumulator
            .apply(&json!({ "choices": [{ "delta": {
                "tool_calls": [{ "index": 0, "id": "call_1", "function": {
                    "name": "tool_read_file", "arguments": "{\"path\":\"Cargo"
                } }]
            }, "finish_reason": "tool_calls" }] }))
            .unwrap();

        let message = accumulator.finish().unwrap();
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].id, "call_1");
        assert_eq!(message.tool_calls[0].name, "tool_read_file");
        assert_eq!(message.tool_calls[0].arguments, Value::Null);
        assert_eq!(message.raw["tool_calls"][0]["function"]["arguments"], "null");
    }

    #[test]
    fn stream_accumulator_rejects_incomplete_tool_calls() {
        let mut accumulator = StreamAccumulator::default();
        accumulator
            .apply(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0 }] } }] }))
            .unwrap();
        assert!(accumulator.finish().is_err());
    }

    #[test]
    fn openrouter_http_errors_extract_message() {
        let value = json!({ "error": { "message": "rate limited" } });
        assert_eq!(openrouter_error_message(429, &value), "OpenRouter HTTP 429: rate limited");
    }

    #[test]
    fn classifies_http_errors_into_retry_decisions() {
        use ee_acp_agent_server::ProviderError;
        enum ExpectedError {
            RateLimited,
            Transient,
            BackendFailure,
            InvalidRequest,
        }

        let hint = Some(Duration::from_secs(3));
        let cases = [
            (429, ExpectedError::RateLimited),
            (408, ExpectedError::Transient),
            (500, ExpectedError::Transient),
            (502, ExpectedError::Transient),
            (503, ExpectedError::Transient),
            (504, ExpectedError::Transient),
            (599, ExpectedError::Transient),
            (401, ExpectedError::BackendFailure),
            (403, ExpectedError::BackendFailure),
            (400, ExpectedError::InvalidRequest),
            (404, ExpectedError::InvalidRequest),
        ];
        for (status, expected) in cases {
            let error = classify_http_error(status, hint, format!("HTTP {status}"));
            assert!(
                matches!(
                    (&expected, &error),
                    (ExpectedError::RateLimited, ProviderError::RateLimited { .. })
                        | (ExpectedError::Transient, ProviderError::Transient { .. })
                        | (ExpectedError::BackendFailure, ProviderError::BackendFailure(_))
                        | (ExpectedError::InvalidRequest, ProviderError::InvalidRequest(_))
                ),
                "status {status} classified wrong: {error:?}"
            );
        }
        match classify_http_error(429, hint, "slow".into()) {
            ProviderError::RateLimited { retry_after, .. } => assert_eq!(retry_after, hint),
            other => panic!("expected rate limited, got {other:?}"),
        }
        match classify_http_error(503, hint, "down".into()) {
            ProviderError::Transient { retry_after, .. } => assert_eq!(retry_after, hint),
            other => panic!("expected transient, got {other:?}"),
        }
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after: None,
            detail: "x".into()
        }));
        assert!(is_retryable(&ProviderError::Transient { retry_after: None, detail: "x".into() }));
        assert!(!is_retryable(&ProviderError::BackendFailure("x".into())));
        assert!(!is_retryable(&ProviderError::InvalidRequest("x".into())));
        assert!(!is_retryable(&ProviderError::Cancellation));
    }

    #[test]
    fn parses_retry_after_seconds_and_ignores_dates() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert("retry-after", HeaderValue::from_static("120"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(120)));
        headers.insert("retry-after", HeaderValue::from_static("Tue, 15 Nov 1994 08:12:31 GMT"));
        assert_eq!(parse_retry_after(&headers), None, "HTTP dates are rejected, not mis-parsed");
        headers.insert("retry-after", HeaderValue::from_static("abc"));
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn retry_delay_prefers_capped_server_hint() {
        let config = Config {
            retry_base_delay: Duration::from_millis(100),
            retry_max_delay: Duration::from_secs(10),
            ..test_config()
        };
        assert_eq!(retry_delay(&config, 0, Some(Duration::from_secs(2))), Duration::from_secs(2));
        assert_eq!(retry_delay(&config, 0, Some(Duration::from_secs(60))), Duration::from_secs(10));
    }

    #[test]
    fn retry_delay_backs_off_exponentially_with_bounded_jitter() {
        let config = Config {
            retry_base_delay: Duration::from_millis(100),
            retry_max_delay: Duration::from_secs(10),
            ..test_config()
        };
        for attempt in 0..5 {
            let delay = retry_delay(&config, attempt, None).as_millis() as u64;
            let expected = 100u64 << attempt.min(10);
            let ceiling = expected + expected / 5 + 1;
            assert!(
                (expected..=ceiling).contains(&delay),
                "attempt {attempt}: {delay} outside [{expected}, {ceiling}]"
            );
        }
        let delay = retry_delay(&config, 20, None).as_millis() as u64;
        assert!(delay <= 10_000 + 10_000 / 5 + 1, "{delay}");
    }
}
