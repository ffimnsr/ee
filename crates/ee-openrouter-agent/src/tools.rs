//! Tool execution for OpenRouter prompt turns.
//!
//! The model may only request file reads (`tool_read_file` or the `read_file`
//! alias).  Relative paths resolve against the session cwd; the actual read
//! goes through the framework's [`ClientBridge`], which validates the path
//! is absolute before anything is written to the transport.  Tool state
//! transitions are emitted through the [`UpdateSink`].

use std::path::Path;

use ee_acp_agent_server::{ClientBridge, UpdateSink};
use ee_agent_protocol::{
    ContentBlock, ReadTextFileRequest, SessionId, TextContent, ToolCallContent,
};

use crate::openrouter::OpenRouterToolCall;
use serde_json::Value;

/// Executes one tool call and returns the tool result text.
///
/// Read failures (client error, transport close) become `error: ...` result
/// text and a failed tool-call update, so the turn can continue; the prompt
/// itself only fails for framework-level breakdowns.
pub(crate) async fn handle_tool_call(
    session_id: &SessionId,
    cwd: Option<&str>,
    tool_call: &OpenRouterToolCall,
    sink: &UpdateSink,
    client: &ClientBridge,
) -> String {
    match tool_call.name.as_str() {
        "tool_read_file" | "read_file" => {
            let Some(raw_path) = tool_call.arguments.get("path").and_then(Value::as_str) else {
                return String::from("error: missing path");
            };
            let Some(path) = resolve_workspace_path(cwd, raw_path) else {
                return format!("error: path outside workspace or no cwd: {raw_path}");
            };
            let running = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
                format!("path: {path}"),
            )))];
            if let Err(error) = sink.tool_call_in_progress(&tool_call.id, "read file", running) {
                return format!("error: failed to emit tool update: {error}");
            }

            let request = ReadTextFileRequest::new(session_id.clone(), path);
            match client.read_text_file(request).await {
                Ok(response) => {
                    let completed = vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(format!("read {} bytes", response.content.len())),
                    ))];
                    if let Err(error) =
                        sink.tool_call_completed(&tool_call.id, "read file", completed)
                    {
                        return format!("error: failed to emit tool update: {error}");
                    }
                    response.content
                }
                Err(error) => {
                    let message = error.to_string();
                    if let Err(sink_error) =
                        sink.tool_call_failed(&tool_call.id, "read file", message.clone())
                    {
                        return format!("error: failed to emit tool update: {sink_error}");
                    }
                    format!("error: {message}")
                }
            }
        }
        other => format!("error: unsupported tool {other}"),
    }
}

/// Resolves a tool path: absolute paths pass through, relative paths join
/// the session cwd; `None` when a relative path has no cwd to resolve
/// against.
pub(crate) fn resolve_workspace_path(cwd: Option<&str>, raw_path: &str) -> Option<String> {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return Some(path.to_string_lossy().to_string());
    }
    let cwd = cwd?;
    Some(Path::new(cwd).join(path).to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_pass_through() {
        assert_eq!(
            resolve_workspace_path(Some("/work"), "/tmp/notes.txt").as_deref(),
            Some("/tmp/notes.txt")
        );
    }

    #[test]
    fn relative_paths_join_the_session_cwd() {
        assert_eq!(
            resolve_workspace_path(Some("/work"), ".ee.toml").as_deref(),
            Some("/work/.ee.toml")
        );
    }

    #[test]
    fn relative_paths_without_cwd_are_rejected() {
        assert_eq!(resolve_workspace_path(None, ".ee.toml"), None);
    }
}
