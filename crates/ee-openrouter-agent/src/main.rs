use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};

use serde_json::{Value, json};

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";
const MAX_TOOL_ROUNDS: usize = 6;

#[derive(Debug, Parser)]
#[command(version, about = "ACP stdio bridge for OpenRouter chat completions")]
struct Args {
    /// OpenRouter model id, e.g. deepseek/deepseek-v4-flash-0731.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: String,
    /// Chat completions endpoint.
    #[arg(long, env = "OPENROUTER_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,
    /// Optional HTTP-Referer value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_SITE_URL")]
    site_url: Option<String>,
    /// Optional X-Title value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_APP_TITLE", default_value = "ee")]
    app_title: String,
    /// Request timeout in milliseconds.
    #[arg(long, env = "OPENROUTER_TIMEOUT_MS", default_value_t = 120_000)]
    timeout_ms: u64,
    /// Optional OpenRouter reasoning effort (`low`, `medium`, or `high`).
    #[arg(long, env = "OPENROUTER_REASONING_EFFORT")]
    reasoning_effort: Option<String>,
    /// System prompt sent with each session history.
    #[arg(long, env = "OPENROUTER_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,
}

#[derive(Debug, Clone)]
struct Config {
    model: String,
    api_url: String,
    api_key: Option<String>,
    site_url: Option<String>,
    app_title: String,
    timeout: Duration,
    system_prompt: String,
    reasoning_effort: Option<String>,
}

impl Config {
    fn from_args_and_dotenv(args: Args, dotenv: &BTreeMap<String, String>) -> Self {
        Self {
            model: args.model,
            api_url: args.api_url,
            api_key: env_or_dotenv("OPENROUTER_API_KEY", dotenv),
            site_url: args.site_url.or_else(|| env_or_dotenv("OPENROUTER_SITE_URL", dotenv)),
            app_title: args.app_title,
            timeout: Duration::from_millis(args.timeout_ms),
            system_prompt: args.system_prompt,
            reasoning_effort: args.reasoning_effort,
        }
    }
}

fn env_or_dotenv(name: &str, dotenv: &BTreeMap<String, String>) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| dotenv.get(name).cloned().filter(|value| !value.is_empty()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionData {
    cwd: Option<String>,
    messages: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenRouterToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenRouterMessage {
    content: String,
    reasoning: String,
    raw: Value,
    tool_calls: Vec<OpenRouterToolCall>,
}

#[derive(Debug)]
struct AgentState {
    config: Config,
    client: Client,
    sessions: BTreeMap<String, SessionData>,
    next_session: usize,
    next_message: usize,
    next_rpc_id: i64,
}

impl AgentState {
    fn new(config: Config) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self {
            config,
            client,
            sessions: BTreeMap::new(),
            next_session: 1,
            next_message: 1,
            next_rpc_id: 10_000,
        })
    }

    fn handle_request(&mut self, method: &str, params: &Value, id: Value) -> Vec<Value> {
        match method {
            "initialize" => vec![response(
                id,
                json!({
                    "protocolVersion": 1,
                    "agentInfo": {
                        "name": "ee-openrouter-agent",
                        "title": "OpenRouter"
                    },
                    "agentCapabilities": {}
                }),
            )],
            "session/new" => {
                let session_id = format!("openrouter-{}", self.next_session);
                self.next_session += 1;
                self.sessions.insert(
                    session_id.clone(),
                    SessionData {
                        cwd: params.get("cwd").and_then(Value::as_str).map(str::to_string),
                        messages: Vec::new(),
                    },
                );
                vec![response(id, json!({ "sessionId": session_id }))]
            }
            "session/prompt" => vec![error_response(
                id,
                -32603,
                "internal error: session/prompt requires interactive handling",
            )],
            "session/cancel" => vec![response(id, json!({}))],
            "session/load" | "session/resume" => vec![error_response(
                id,
                -32601,
                "session loading is not supported by ee-openrouter-agent",
            )],
            _ => vec![error_response(id, -32601, format!("method not found: {method}"))],
        }
    }

    fn handle_prompt_interactive(
        &mut self,
        params: &Value,
        id: Value,
        stdin: &mut impl BufRead,
        stdout: &mut impl Write,
    ) -> Result<(), String> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return write_json_line(stdout, &error_response(id, -32602, "missing sessionId"))
                .map_err(|error| format!("failed to write JSON-RPC error: {error}"));
        };
        let prompt_text = extract_prompt_text(params);
        if prompt_text.trim().is_empty() {
            return write_json_line(
                stdout,
                &error_response(id, -32602, "prompt has no text content"),
            )
            .map_err(|error| format!("failed to write JSON-RPC error: {error}"));
        }
        if !self.sessions.contains_key(session_id) {
            self.sessions
                .insert(session_id.to_string(), SessionData { cwd: None, messages: Vec::new() });
        }
        let Some(api_key) = self.config.api_key.clone() else {
            return write_json_line(
                stdout,
                &error_response(
                    id,
                    -32000,
                    "OPENROUTER_API_KEY is not set; export it before starting ee",
                ),
            )
            .map_err(|error| format!("failed to write JSON-RPC error: {error}"));
        };

        let mut messages = self.openrouter_messages(session_id, &prompt_text);
        let mut pending_history = vec![json!({ "role": "user", "content": prompt_text })];
        for round in 0..=MAX_TOOL_ROUNDS {
            let answer = match self.call_openrouter(&api_key, &messages) {
                Ok(answer) => answer,
                Err(error) => {
                    return write_json_line(stdout, &error_response(id, -32000, error))
                        .map_err(|error| format!("failed to write JSON-RPC error: {error}"));
                }
            };

            if !answer.reasoning.is_empty() {
                let message_id = format!("openrouter-thought-{}", self.next_message);
                self.next_message += 1;
                write_json_line(
                    stdout,
                    &session_update(
                        session_id,
                        agent_thought_chunk(&message_id, &answer.reasoning),
                    ),
                )
                .map_err(|error| format!("failed to write session update: {error}"))?;
            }

            if answer.tool_calls.is_empty() {
                if !answer.content.is_empty() {
                    let message_id = format!("openrouter-message-{}", self.next_message);
                    self.next_message += 1;
                    write_json_line(
                        stdout,
                        &session_update(
                            session_id,
                            agent_message_chunk(&message_id, &answer.content),
                        ),
                    )
                    .map_err(|error| format!("failed to write session update: {error}"))?;
                }
                pending_history.push(json!({ "role": "assistant", "content": answer.content }));
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.messages.extend(pending_history);
                }
                return write_json_line(stdout, &response(id, json!({ "stopReason": "end_turn" })))
                    .map_err(|error| format!("failed to write prompt response: {error}"));
            }

            if round == MAX_TOOL_ROUNDS {
                return write_json_line(
                    stdout,
                    &error_response(id, -32000, "OpenRouter tool loop exceeded maximum rounds"),
                )
                .map_err(|error| format!("failed to write JSON-RPC error: {error}"));
            }

            messages.push(answer.raw.clone());
            pending_history.push(answer.raw);
            for tool_call in answer.tool_calls {
                let result = self.handle_tool_call(session_id, &tool_call, stdin, stdout)?;
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "content": result,
                }));
                pending_history.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "content": result,
                }));
            }
        }
        unreachable!("tool loop returns inside bounded range")
    }

    fn openrouter_messages(&self, session_id: &str, prompt_text: &str) -> Vec<Value> {
        let mut messages = vec![json!({
            "role": "system",
            "content": self.config.system_prompt,
        })];
        if let Some(session) = self.sessions.get(session_id) {
            messages.extend(session.messages.clone());
        }
        messages.push(json!({ "role": "user", "content": prompt_text }));
        messages
    }

    fn call_openrouter(
        &self,
        api_key: &str,
        messages: &[Value],
    ) -> Result<OpenRouterMessage, String> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|error| format!("invalid OPENROUTER_API_KEY header value: {error}"))?,
        );
        headers.insert(
            HeaderName::from_static("x-title"),
            HeaderValue::from_str(&self.config.app_title)
                .map_err(|error| format!("invalid OpenRouter app title: {error}"))?,
        );
        if let Some(site_url) = &self.config.site_url {
            headers.insert(
                HeaderName::from_static("http-referer"),
                HeaderValue::from_str(site_url)
                    .map_err(|error| format!("invalid OpenRouter site URL header: {error}"))?,
            );
        }

        let body = openrouter_request_body(&self.config, messages);
        let response = self
            .client
            .post(&self.config.api_url)
            .headers(headers)
            .json(&body)
            .send()
            .map_err(|error| format!("OpenRouter request failed: {error}"))?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .map_err(|error| format!("OpenRouter response was not JSON: {error}"))?;
        if !status.is_success() {
            return Err(openrouter_error_message(status.as_u16(), &value));
        }
        extract_openrouter_message(&value).ok_or_else(|| {
            format!("OpenRouter response did not include choices[0].message: {value}")
        })
    }

    fn handle_tool_call(
        &mut self,
        session_id: &str,
        tool_call: &OpenRouterToolCall,
        stdin: &mut impl BufRead,
        stdout: &mut impl Write,
    ) -> Result<String, String> {
        match tool_call.name.as_str() {
            "tool_read_file" | "read_file" => {
                let Some(raw_path) = tool_call.arguments.get("path").and_then(Value::as_str) else {
                    return Ok(String::from("error: missing path"));
                };
                let Some(path) = self.resolve_workspace_path(session_id, raw_path) else {
                    return Ok(format!("error: path outside workspace or no cwd: {raw_path}"));
                };
                write_json_line(
                    stdout,
                    &session_update(
                        session_id,
                        tool_call_update(
                            &tool_call.id,
                            "read file",
                            "in_progress",
                            Some(format!("path: {path}")),
                        ),
                    ),
                )
                .map_err(|error| format!("failed to write tool update: {error}"))?;
                let result = self.request_read_text_file(session_id, &path, stdin, stdout)?;
                write_json_line(
                    stdout,
                    &session_update(
                        session_id,
                        tool_call_update(
                            &tool_call.id,
                            "read file",
                            "completed",
                            Some(format!("read {} bytes", result.len())),
                        ),
                    ),
                )
                .map_err(|error| format!("failed to write tool update: {error}"))?;
                Ok(result)
            }
            other => Ok(format!("error: unsupported tool {other}")),
        }
    }

    fn resolve_workspace_path(&self, session_id: &str, raw_path: &str) -> Option<String> {
        let path = Path::new(raw_path);
        if path.is_absolute() {
            return Some(path.to_string_lossy().to_string());
        }
        let cwd = self.sessions.get(session_id)?.cwd.as_ref()?;
        let joined = Path::new(cwd).join(path);
        Some(joined.to_string_lossy().to_string())
    }

    fn request_read_text_file(
        &mut self,
        session_id: &str,
        path: &str,
        stdin: &mut impl BufRead,
        stdout: &mut impl Write,
    ) -> Result<String, String> {
        let request_id = self.next_rpc_id;
        self.next_rpc_id += 1;
        write_json_line(
            stdout,
            &json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": request_id,
                "method": "fs/read_text_file",
                "params": { "sessionId": session_id, "path": path }
            }),
        )
        .map_err(|error| format!("failed to write fs/read_text_file request: {error}"))?;

        loop {
            let mut line = String::new();
            let read = stdin
                .read_line(&mut line)
                .map_err(|error| format!("failed to read fs/read_text_file response: {error}"))?;
            if read == 0 {
                return Err(String::from(
                    "stdin closed while waiting for fs/read_text_file response",
                ));
            }
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|error| format!("invalid JSON-RPC response from host: {error}"))?;
            if value.get("id").and_then(Value::as_i64) != Some(request_id) {
                continue;
            }
            if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
                return Ok(format!("error: {message}"));
            }
            return value
                .pointer("/result/content")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    format!("fs/read_text_file response missing result.content: {value}")
                });
        }
    }
}

fn load_dotenv(path: &Path) -> io::Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_dotenv(&text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error),
    }
}

fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(is_env_name_char) {
            continue;
        }
        values.insert(name.to_string(), unquote_dotenv_value(value.trim()));
    }
    values
}

fn is_env_name_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn unquote_dotenv_value(value: &str) -> String {
    let Some(stripped) = value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
        return value
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .unwrap_or(value)
            .to_string();
    };
    let mut out = String::new();
    let mut chars = stripped.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn extract_prompt_text(params: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(prompt) = params.get("prompt").and_then(Value::as_array) {
        for block in prompt {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n")
}

fn openrouter_request_body(config: &Config, messages: &[Value]) -> Value {
    let mut body = json!({
        "model": config.model,
        "messages": messages,
        "stream": false,
        "tools": openrouter_tools(),
        "tool_choice": "auto",
    });
    if let Some(effort) = config.reasoning_effort.as_deref().filter(|effort| !effort.is_empty())
        && let Some(object) = body.as_object_mut()
    {
        object.insert(String::from("reasoning"), json!({ "effort": effort }));
    }
    body
}

fn openrouter_tools() -> Value {
    json!([
        {
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
        }
    ])
}

fn extract_openrouter_message(value: &Value) -> Option<OpenRouterMessage> {
    let message = value.pointer("/choices/0/message")?;
    let content = extract_openrouter_content(message.get("content").unwrap_or(&Value::Null));
    let reasoning = extract_openrouter_reasoning(message);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(extract_openrouter_tool_call).collect())
        .unwrap_or_default();
    Some(OpenRouterMessage { content, reasoning, raw: message.clone(), tool_calls })
}

fn extract_openrouter_reasoning(message: &Value) -> String {
    for pointer in ["/reasoning", "/reasoning_content", "/thinking"] {
        if let Some(text) = message.pointer(pointer).and_then(Value::as_str)
            && !text.is_empty()
        {
            return text.to_string();
        }
    }
    String::new()
}

fn extract_openrouter_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        return parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
    }
    String::new()
}

fn extract_openrouter_tool_call(value: &Value) -> Option<OpenRouterToolCall> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let function = value.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.to_string();
    let arguments = match function.get("arguments")? {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        value => value.clone(),
    };
    Some(OpenRouterToolCall { id, name, arguments })
}

fn tool_call_update(id: &str, title: &str, status: &str, content: Option<String>) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "title": title,
        "status": status,
    });
    if let Some(content) = content
        && let Some(object) = update.as_object_mut()
    {
        object.insert(
            String::from("content"),
            json!([{ "type": "content", "content": { "type": "text", "text": content } }]),
        );
    }
    update
}

fn openrouter_error_message(status: u16, value: &Value) -> String {
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        format!("OpenRouter HTTP {status}: {message}")
    } else {
        format!("OpenRouter HTTP {status}: {value}")
    }
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": "session/update",
        "params": { "sessionId": session_id, "update": update }
    })
}

fn agent_message_chunk(message_id: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "messageId": message_id,
        "content": { "type": "text", "text": text }
    })
}

fn agent_thought_chunk(message_id: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_thought_chunk",
        "messageId": message_id,
        "content": { "type": "text", "text": text }
    })
}

fn write_json_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn run(config: Config, mut stdin: impl BufRead, mut stdout: impl Write) -> Result<(), String> {
    let mut state = AgentState::new(config)?;
    loop {
        let mut line = String::new();
        let read =
            stdin.read_line(&mut line).map_err(|error| format!("failed to read stdin: {error}"))?;
        if read == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                write_json_line(
                    &mut stdout,
                    &error_response(Value::Null, -32700, error.to_string()),
                )
                .map_err(|error| format!("failed to write parse error: {error}"))?;
                continue;
            }
        };
        let Some(method) = parsed.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = parsed.get("id").cloned() else {
            continue;
        };
        let params = parsed.get("params").unwrap_or(&Value::Null);
        if method == "session/prompt" {
            state.handle_prompt_interactive(params, id, &mut stdin, &mut stdout)?;
            continue;
        }
        for outbound in state.handle_request(method, params, id) {
            write_json_line(&mut stdout, &outbound)
                .map_err(|error| format!("failed to write JSON-RPC response: {error}"))?;
        }
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    let dotenv = match load_dotenv(Path::new(".env")) {
        Ok(values) => values,
        Err(error) => {
            eprintln!("ee-openrouter-agent: warning: failed to read .env: {error}");
            BTreeMap::new()
        }
    };
    let config = Config::from_args_and_dotenv(args, &dotenv);
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = run(config, stdin.lock(), stdout.lock()) {
        eprintln!("ee-openrouter-agent: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            model: String::from("test/model"),
            api_url: String::from(DEFAULT_API_URL),
            api_key: None,
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
        }
    }

    #[test]
    fn extracts_prompt_text_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" },
                { "type": "image", "data": "ignored" }
            ]
        });

        assert_eq!(extract_prompt_text(&params), "hello\nworld");
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
    fn openrouter_request_body_includes_reasoning_effort_when_configured() {
        let mut config = test_config();
        config.reasoning_effort = Some(String::from("low"));

        let body = openrouter_request_body(&config, &[json!({ "role": "user", "content": "hi" })]);

        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn agent_thought_chunk_uses_acp_thought_update() {
        let chunk = agent_thought_chunk("thought-1", "plan");

        assert_eq!(chunk["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(chunk["messageId"], "thought-1");
        assert_eq!(chunk["content"]["text"], "plan");
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
    fn read_file_tool_call_requests_acp_file_read() {
        let mut state = AgentState::new(test_config()).unwrap();
        let new_session =
            state.handle_request("session/new", &json!({ "cwd": "/workspace" }), json!(1));
        let session_id = new_session[0]["result"]["sessionId"].as_str().unwrap().to_string();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 10000,
            "result": { "content": "[agents]\nenabled = true\n" }
        });
        let mut input = io::Cursor::new(format!("{response}\n").into_bytes());
        let mut output = Vec::new();
        let tool_call = OpenRouterToolCall {
            id: String::from("call_1"),
            name: String::from("tool_read_file"),
            arguments: json!({ "path": ".ee.toml" }),
        };

        let result =
            state.handle_tool_call(&session_id, &tool_call, &mut input, &mut output).unwrap();

        assert_eq!(result, "[agents]\nenabled = true\n");
        let lines = String::from_utf8(output).unwrap();
        assert!(lines.contains("\"method\":\"fs/read_text_file\""), "{lines}");
        assert!(lines.contains("\"path\":\"/workspace/.ee.toml\""), "{lines}");
        assert!(lines.contains("\"status\":\"completed\""), "{lines}");
    }

    #[test]
    fn prompt_without_api_key_returns_json_rpc_error() {
        let mut state = AgentState::new(test_config()).unwrap();
        let new_session = state.handle_request("session/new", &Value::Null, json!(1));
        let session_id = new_session[0]["result"]["sessionId"].as_str().unwrap().to_string();
        let mut input = io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        state
            .handle_prompt_interactive(
                &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hello" }] }),
                json!(2),
                &mut input,
                &mut output,
            )
            .unwrap();

        let out: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(out["error"]["code"], -32000);
        assert!(out["error"]["message"].as_str().unwrap().contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn initialize_response_has_acp_v1() {
        let mut state = AgentState::new(test_config()).unwrap();
        let out = state.handle_request("initialize", &Value::Null, json!(1));

        assert_eq!(out[0]["result"]["protocolVersion"], 1);
        assert_eq!(out[0]["result"]["agentInfo"]["name"], "ee-openrouter-agent");
    }

    #[test]
    fn parses_dotenv_values_without_mutating_process_env() {
        let parsed = parse_dotenv(
            r#"
# comment
OPENROUTER_API_KEY=sk-test
export OPENROUTER_SITE_URL="https://example.test"
OPENROUTER_SYSTEM_PROMPT='hello agent'
BAD LINE
BAD-NAME=value
ESCAPED="line\nnext"
"#,
        );

        assert_eq!(parsed.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-test"));
        assert_eq!(
            parsed.get("OPENROUTER_SITE_URL").map(String::as_str),
            Some("https://example.test")
        );
        assert_eq!(parsed.get("OPENROUTER_SYSTEM_PROMPT").map(String::as_str), Some("hello agent"));
        assert_eq!(parsed.get("ESCAPED").map(String::as_str), Some("line\nnext"));
        assert!(!parsed.contains_key("BAD-NAME"));
    }

    #[test]
    fn loads_dotenv_from_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "OPENROUTER_API_KEY=from-file\n").unwrap();

        let loaded = load_dotenv(&path).unwrap();

        assert_eq!(loaded.get("OPENROUTER_API_KEY").map(String::as_str), Some("from-file"));
    }
}
