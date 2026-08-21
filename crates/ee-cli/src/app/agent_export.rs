//! Private Markdown export for locally retained agent transcripts.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{AgentThreadUi, MessageRenderKind, TranscriptItem};

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
                output.push_str(&format!(
                    "### {timestamp} · Tool: {}\n\n- ID: `{id}`\n- Status: `{status}`\n- Detail: {}\n\n",
                    redacted_export_text(title, secrets),
                    redacted_export_text(detail, secrets),
                ));
                let tool = snapshot.tool_calls.get(id);
                append_raw_json_section(
                    &mut output,
                    "Input",
                    tool.and_then(|tool| tool.raw_input.as_ref()),
                    secrets,
                );
                append_raw_json_section(
                    &mut output,
                    "Output",
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
