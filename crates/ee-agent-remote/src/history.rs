//! Durable per-session turn history.
//!
//! Enabled only when a session log directory is configured. One JSONL file per
//! session records terminal turn outcomes and a bounded, redacted assistant
//! text tail. Appends are failure-tolerant and the file rotates once at the
//! size cap so history cannot grow unbounded.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::redact::Redactor;

/// Size cap per session file before one rotation.
pub const DEFAULT_SESSION_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum characters retained in the assistant text tail.
pub const MAX_HISTORY_TAIL_CHARS: usize = 4096;
/// Maximum entries returned by one history tail.
pub const MAX_HISTORY_ENTRIES: usize = 500;

/// One durable turn record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// Session id.
    pub session_id: String,
    /// Remote turn id.
    pub turn_id: u64,
    /// Terminal status label.
    pub status: String,
    /// ACP stop reason, when the turn completed normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Bounded, redacted assistant text tail.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assistant_tail: String,
}

/// Session history store.
#[derive(Debug, Clone)]
pub struct SessionLog {
    dir: Option<PathBuf>,
    max_bytes: u64,
}

impl SessionLog {
    /// Disabled history.
    #[must_use]
    pub fn disabled() -> Self {
        Self { dir: None, max_bytes: DEFAULT_SESSION_LOG_MAX_BYTES }
    }

    /// History rooted at `dir`.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir: Some(dir), max_bytes: DEFAULT_SESSION_LOG_MAX_BYTES }
    }

    /// Whether history is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// The configured directory.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    fn path_for(&self, session_id: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(dir.join(format!("{}.jsonl", sanitize_session_id(session_id))))
    }

    /// Appends one terminal turn record.
    ///
    /// # Errors
    ///
    /// Returns I/O errors to the caller, which logs them and keeps serving.
    pub fn record(&self, entry: &HistoryEntry) -> std::io::Result<()> {
        let Some(path) = self.path_for(&entry.session_id) else {
            return Ok(());
        };
        let mut line = serde_json::to_string(entry)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        line.push('\n');
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed(&path, line.len())?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(line.as_bytes())?;
        file.flush()
    }

    fn rotate_if_needed(&self, path: &Path, incoming: usize) -> std::io::Result<()> {
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(());
        };
        if metadata.len() == 0 || metadata.len().saturating_add(incoming as u64) <= self.max_bytes {
            return Ok(());
        }
        let rotated = path.with_extension("jsonl.1");
        match std::fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(path, rotated)
    }

    /// Returns the newest `limit` records for one session, oldest first.
    ///
    /// # Errors
    ///
    /// Returns I/O errors for unreadable files.
    pub fn tail(&self, session_id: &str, limit: usize) -> std::io::Result<Vec<HistoryEntry>> {
        let Some(path) = self.path_for(session_id) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, MAX_HISTORY_ENTRIES);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut entries: Vec<HistoryEntry> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::warn!(%error, "ignoring malformed session history line");
                    None
                }
            })
            .collect();
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    }
}

/// Builds a bounded, redacted assistant text tail from a session snapshot.
#[must_use]
pub fn assistant_text_tail(state: &ee_agent_host::SessionState, redactor: &Redactor) -> String {
    let mut text = String::new();
    for message in state.messages.iter().rev() {
        if message.kind != ee_agent_host::MessageKind::Assistant {
            continue;
        }
        let mut blocks = String::new();
        for block in &message.blocks {
            if let ee_agent_protocol::ContentBlock::Text(content) = block {
                blocks.push_str(&content.text);
            }
        }
        if blocks.trim().is_empty() {
            continue;
        }
        text.push_str(&blocks);
        if text.len() >= MAX_HISTORY_TAIL_CHARS {
            break;
        }
    }
    let redacted = redactor.text(&text);
    if redacted.len() <= MAX_HISTORY_TAIL_CHARS {
        return redacted;
    }
    let mut boundary = MAX_HISTORY_TAIL_CHARS;
    while boundary > 0 && !redacted.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &redacted[..boundary])
}

/// Builds a durable record for one terminal turn.
#[must_use]
pub fn entry_for_turn(
    session_id: &str,
    turn_id: u64,
    status: &str,
    stop_reason: Option<String>,
    state: Option<&ee_agent_host::SessionState>,
    redactor: &Redactor,
) -> HistoryEntry {
    HistoryEntry {
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        session_id: session_id.to_string(),
        turn_id,
        status: status.to_string(),
        stop_reason,
        assistant_tail: state.map(|state| assistant_text_tail(state, redactor)).unwrap_or_default(),
    }
}

fn sanitize_session_id(session_id: &str) -> String {
    const MAX_FILE_NAME_CHARS: usize = 100;
    let mut name: String = session_id
        .chars()
        .take(MAX_FILE_NAME_CHARS)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.trim_matches('.').is_empty() {
        name = String::from("session");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_tails_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SessionLog::new(dir.path().to_path_buf());
        for turn in 1..=3 {
            log.record(&entry_for_turn("s-1", turn, "completed", None, None, &Redactor::default()))
                .expect("record");
        }
        let entries = log.tail("s-1", 2).expect("tail");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.last().expect("last").turn_id, 3);
    }

    #[test]
    fn disabled_log_is_a_no_op() {
        let log = SessionLog::disabled();
        assert!(!log.enabled());
        assert!(log.tail("s", 10).expect("tail").is_empty());
    }

    #[test]
    fn session_ids_cannot_escape_the_directory() {
        assert_eq!(sanitize_session_id("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_session_id(""), "session");
    }

    #[test]
    fn tails_are_bounded_and_redacted() {
        let mut state = ee_agent_host::SessionState::default();
        state.messages.push(ee_agent_host::ReducedMessage {
            kind: ee_agent_host::MessageKind::Assistant,
            message_id: None,
            blocks: vec![ee_agent_protocol::ContentBlock::Text(
                ee_agent_protocol::TextContent::new(format!(
                    "Authorization: Bearer topsecret {}",
                    "x".repeat(MAX_HISTORY_TAIL_CHARS)
                )),
            )],
        });
        let tail = assistant_text_tail(&state, &Redactor::default());
        assert!(!tail.contains("topsecret"));
        assert!(tail.len() <= MAX_HISTORY_TAIL_CHARS + 4);
    }
}
