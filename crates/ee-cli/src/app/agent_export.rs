//! Private Markdown export for locally retained agent transcripts.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{AgentThreadUi, MessageRenderKind, TranscriptItem};
use ee_agent_protocol::ToolKind;

fn transcript_item_time(item: &TranscriptItem) -> SystemTime {
    match item {
        TranscriptItem::Message { at, .. }
        | TranscriptItem::ToolCall { at, .. }
        | TranscriptItem::Permission { at, .. }
        | TranscriptItem::Elicitation { at, .. }
        | TranscriptItem::System { at, .. }
        | TranscriptItem::Stderr { at, .. } => *at,
    }
}

fn format_export_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn markdown_code_fence(content: &str, language: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for character in content.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{language}\n{content}\n{fence}\n")
}

fn redacted_export_text(text: &str, secrets: &[String]) -> String {
    ee_agent_host::redact::redact_secret_values(text, secrets)
}

fn redacted_export_json(value: &serde_json::Value, secrets: &[String]) -> String {
    let redacted = ee_agent_host::redact::redact_json(value);
    let formatted =
        serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| String::from("null"));
    redacted_export_text(&formatted, secrets)
}

fn append_raw_json_section(
    output: &mut String,
    heading: &str,
    value: Option<&serde_json::Value>,
    secrets: &[String],
) {
    output.push_str(&format!("#### {heading}\n\n"));
    match value {
        Some(value) => {
            output.push_str(&markdown_code_fence(&redacted_export_json(value, secrets), "json"))
        }
        None => output.push_str("_Not provided._\n"),
    }
    output.push('\n');
}

fn append_tool_payload_sections(
    output: &mut String,
    kind: Option<ToolKind>,
    raw_input: Option<&serde_json::Value>,
    raw_output: Option<&serde_json::Value>,
    secrets: &[String],
) {
    if matches!(kind, Some(ToolKind::Fetch)) {
        output.push_str("#### External content\n\n");
        output.push_str("- Tool kind: `fetch`\n");
        output.push_str("- Trust: untrusted remote content\n");
        output.push_str("- Raw fetch input/output: omitted from export.\n\n");
        return;
    }

    append_raw_json_section(output, "Input", raw_input, secrets);
    append_raw_json_section(output, "Output", raw_output, secrets);
}

/// Renders a locally retained session transcript in timestamp order.
pub(super) fn format_agent_transcript_markdown(
    thread: &AgentThreadUi,
    exported_at: SystemTime,
    secrets: &[String],
) -> String {
    let snapshot = thread.host.snapshot();
    let mut items: Vec<&TranscriptItem> = thread.transcript.iter().collect();
    items.sort_by_key(|item| transcript_item_time(item));

    let mut output = String::from("# Agent session transcript\n\n");
    output.push_str(&format!("- Session: `{}`\n", thread.session_id));
    output.push_str(&format!("- Agent: `{}`\n", thread.agent_id));
    if let Some(name) = &thread.session_name {
        output.push_str(&format!("- Name: {}\n", redacted_export_text(name, secrets)));
    }
    if let Some(title) = &thread.session_title {
        output.push_str(&format!("- Title: {}\n", redacted_export_text(title, secrets)));
    }
    output.push_str(&format!("- Exported: {}\n\n", format_export_time(exported_at)));
    output.push_str("## Transcript\n\n");

    for item in items {
        let timestamp = format_export_time(transcript_item_time(item));
        match item {
            TranscriptItem::Message { nick, text, kind, .. } => {
                let role = match kind {
                    MessageRenderKind::User => "User",
                    MessageRenderKind::Assistant => "Assistant",
                    MessageRenderKind::Thought => "Thought",
                };
                output.push_str(&format!("### {timestamp} · {role} ({nick})\n\n"));
                output.push_str(&redacted_export_text(text, secrets));
                output.push_str("\n\n");
            }
            TranscriptItem::ToolCall { id, title, status, detail, .. } => {
                let tool = snapshot.tool_calls.get(id);
                let is_fetch = matches!(tool, Some(tool) if tool.kind == ToolKind::Fetch);
                if is_fetch {
                    output.push_str(&format!(
                        "### {timestamp} · Tool: fetch\n\n- ID: `{id}`\n- Status: `{status}`\n\n"
                    ));
                } else {
                    output.push_str(&format!(
                        "### {timestamp} · Tool: {}\n\n- ID: `{id}`\n- Status: `{status}`\n- Detail: {}\n\n",
                        redacted_export_text(title, secrets),
                        redacted_export_text(detail, secrets),
                    ));
                }
                append_tool_payload_sections(
                    &mut output,
                    tool.map(|tool| tool.kind),
                    tool.and_then(|tool| tool.raw_input.as_ref()),
                    tool.and_then(|tool| tool.raw_output.as_ref()),
                    secrets,
                );
            }
            TranscriptItem::Permission { title, options, .. } => {
                output.push_str(&format!(
                    "### {timestamp} · Permission\n\n{}\n\n",
                    redacted_export_text(title, secrets)
                ));
                for option in options {
                    output.push_str(&format!("- {}\n", redacted_export_text(option, secrets)));
                }
                output.push('\n');
            }
            TranscriptItem::Elicitation { agent, message, url, .. } => {
                output.push_str(&format!(
                    "### {timestamp} · Elicitation ({agent})\n\n{}\n",
                    redacted_export_text(message, secrets)
                ));
                if let Some(url) = url {
                    output.push_str(&format!("\nURL: {}\n", redacted_export_text(url, secrets)));
                }
                output.push('\n');
            }
            TranscriptItem::System { text, .. } => {
                output.push_str(&format!(
                    "### {timestamp} · System\n\n{}\n\n",
                    redacted_export_text(text, secrets)
                ));
            }
            TranscriptItem::Stderr { text, .. } => {
                output.push_str(&format!(
                    "### {timestamp} · Stderr\n\n{}\n\n",
                    markdown_code_fence(&redacted_export_text(text, secrets), "text")
                ));
            }
        }
    }

    output
}

fn sanitized_export_session_id(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .map(|character| match character {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => character,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() { String::from("session") } else { sanitized }
}

#[cfg(unix)]
fn ensure_private_export_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(dir)?,
        Err(error) => return Err(error),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "export path is not a directory",
            ));
        }
        Ok(_) => {}
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn ensure_private_export_dir(_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "private transcript export requires owner-only filesystem permissions",
    ))
}

#[cfg(unix)]
fn create_private_export_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    options.open(path)
}

#[cfg(not(unix))]
fn create_private_export_file(_path: &Path) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "private transcript export requires owner-only filesystem permissions",
    ))
}

/// Writes workspace memory JSON without replacing existing exports.
pub(super) fn write_workspace_memory_export(
    dir: &Path,
    export: &ee_agent_host::WorkspaceMemoryExportDto,
) -> io::Result<PathBuf> {
    ensure_private_export_dir(dir)?;
    let bytes = serde_json::to_vec_pretty(export)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    for sequence in 0..1_000 {
        let suffix = if sequence == 0 { String::new() } else { format!("-{sequence}") };
        let path = dir.join(format!("workspace-memory-{timestamp}{suffix}.json"));
        match create_private_export_file(&path) {
            Ok(mut file) => {
                let result = file.write_all(&bytes).and_then(|_| file.sync_all());
                if let Err(error) = result {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "workspace memory export filename space exhausted",
    ))
}

/// Writes a complete transcript without replacing existing exports.
pub(super) fn write_agent_transcript_export(
    dir: &Path,
    session_id: &str,
    markdown: &str,
) -> io::Result<PathBuf> {
    ensure_private_export_dir(dir)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let session_id = sanitized_export_session_id(session_id);
    for sequence in 0..1_000 {
        let suffix = if sequence == 0 { String::new() } else { format!("-{sequence}") };
        let path = dir.join(format!("session-{session_id}-{timestamp}{suffix}.md"));
        match create_private_export_file(&path) {
            Ok(mut file) => {
                let result = file.write_all(markdown.as_bytes()).and_then(|_| file.sync_all());
                if let Err(error) = result {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "unable to allocate export filename"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn workspace_memory_export_is_private_and_preserves_values_when_requested() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("memory");
        let export = ee_agent_host::WorkspaceMemoryExportDto {
            schema_version: 1,
            workspace_id: String::from("workspace"),
            redacted: false,
            facts: vec![ee_agent_host::WorkspaceMemoryExportedFact {
                namespace: String::from("default"),
                key: String::from("build.command"),
                value: Some(String::from("cargo test --quiet")),
                kind: String::from("command"),
                authority: String::from("user_asserted"),
                freshness: String::from("current"),
                provenance: ee_agent_host::WorkspaceMemoryExportProvenance {
                    source_kind: String::from("user"),
                    source_id: String::from("approval"),
                    revision: None,
                    fingerprint: None,
                    verified_at: None,
                },
                expires_at: None,
                content_hash: String::from("hash"),
            }],
        };

        let path = write_workspace_memory_export(&directory, &export).unwrap();
        assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let decoded: ee_agent_host::WorkspaceMemoryExportDto =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(decoded, export);
    }

    #[test]
    fn fetch_payloads_are_omitted_from_export() {
        let raw_input = json!({
            "url": "https://example.test/search?query=private-query",
            "body": "private-request-body",
        });
        let raw_output = json!({"body": "private-response-body"});
        let mut output = String::new();

        append_tool_payload_sections(
            &mut output,
            Some(ToolKind::Fetch),
            Some(&raw_input),
            Some(&raw_output),
            &[],
        );

        assert!(output.contains("Tool kind: `fetch`"));
        assert!(output.contains("Trust: untrusted remote content"));
        assert!(output.contains("Raw fetch input/output: omitted from export."));
        assert!(!output.contains("private-query"));
        assert!(!output.contains("private-request-body"));
        assert!(!output.contains("private-response-body"));
    }

    #[test]
    fn non_fetch_payloads_remain_in_export() {
        let raw_input = json!({"command": "cargo test"});
        let raw_output = json!({"stdout": "test result: ok"});
        let mut output = String::new();

        append_tool_payload_sections(
            &mut output,
            Some(ToolKind::Execute),
            Some(&raw_input),
            Some(&raw_output),
            &[],
        );

        assert!(output.contains("#### Input"));
        assert!(output.contains("cargo test"));
        assert!(output.contains("#### Output"));
        assert!(output.contains("test result: ok"));
    }
}
