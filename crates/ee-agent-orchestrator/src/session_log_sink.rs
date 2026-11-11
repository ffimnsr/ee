//! Durable sinks for the append-only session log.
//!
//! The in-memory sink serves tests and memory-only runtimes; the file sink
//! writes owner-only JSONL records and content-addressed body files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::OrchestratorError;
use crate::private_fs::{append_private_file, create_private_dir, write_private_file};
use crate::session_log::SESSION_LOG_RECORD_FILE;
use crate::session_log::SessionLogRecord;

/// Durable append-only sink used by a [`crate::session_log::SessionLog`].
pub trait SessionLogSink: Send + Sync + std::fmt::Debug {
    /// Appends one record.
    fn append(&self, session_id: &str, record: &SessionLogRecord) -> Result<(), OrchestratorError>;
    /// Reads every record for `session_id` in append order.
    fn records(&self, session_id: &str) -> Result<Vec<SessionLogRecord>, OrchestratorError>;
    /// Stores one content-addressed body. Repeating an existing hash is a
    /// no-op.
    fn put_body(
        &self,
        session_id: &str,
        content_hash: &str,
        body: &[u8],
    ) -> Result<(), OrchestratorError>;
    /// Reads one content-addressed body.
    fn body(
        &self,
        session_id: &str,
        content_hash: &str,
    ) -> Result<Option<Vec<u8>>, OrchestratorError>;
    /// Sessions with at least one record.
    fn session_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Process-lifetime in-memory sink used by tests and memory-only runtimes.
#[derive(Debug, Default)]
pub struct InMemorySessionLogSink {
    records: Mutex<HashMap<String, Vec<SessionLogRecord>>>,
    bodies: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemorySessionLogSink {
    /// Creates an empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionLogSink for InMemorySessionLogSink {
    fn append(&self, session_id: &str, record: &SessionLogRecord) -> Result<(), OrchestratorError> {
        self.records
            .lock()
            .expect("session log records poisoned")
            .entry(session_id.to_string())
            .or_default()
            .push(record.clone());
        Ok(())
    }

    fn records(&self, session_id: &str) -> Result<Vec<SessionLogRecord>, OrchestratorError> {
        Ok(self
            .records
            .lock()
            .expect("session log records poisoned")
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    fn put_body(
        &self,
        session_id: &str,
        content_hash: &str,
        body: &[u8],
    ) -> Result<(), OrchestratorError> {
        self.bodies
            .lock()
            .expect("session log bodies poisoned")
            .entry(format!("{session_id}/{content_hash}"))
            .or_insert_with(|| body.to_vec());
        Ok(())
    }

    fn body(
        &self,
        session_id: &str,
        content_hash: &str,
    ) -> Result<Option<Vec<u8>>, OrchestratorError> {
        Ok(self
            .bodies
            .lock()
            .expect("session log bodies poisoned")
            .get(&format!("{session_id}/{content_hash}"))
            .cloned())
    }

    fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> =
            self.records.lock().expect("session log records poisoned").keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Owner-only JSONL sink with content-addressed body files.
#[derive(Debug)]
pub struct FileSessionLogSink {
    root: PathBuf,
    max_session_bytes: usize,
}

impl FileSessionLogSink {
    /// Creates a file sink rooted at `root`.
    #[must_use]
    pub fn new(root: PathBuf, max_session_bytes: usize) -> Self {
        Self { root, max_session_bytes }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(sanitize_component(session_id))
    }

    fn records_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(SESSION_LOG_RECORD_FILE)
    }

    fn body_path(&self, session_id: &str, content_hash: &str) -> PathBuf {
        self.session_dir(session_id).join("bodies").join(content_hash)
    }
}

impl SessionLogSink for FileSessionLogSink {
    fn append(&self, session_id: &str, record: &SessionLogRecord) -> Result<(), OrchestratorError> {
        let directory = self.session_dir(session_id);
        create_private_dir(&directory)?;
        let path = self.records_path(session_id);
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() as usize > self.max_session_bytes) {
            return Err(OrchestratorError::InvalidState(format!(
                "session log for {session_id} exceeded the {} byte cap",
                self.max_session_bytes
            )));
        }
        let mut line = serde_json::to_vec(record).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "session log record serialization failed: {error}"
            ))
        })?;
        line.push(b'\n');
        append_private_file(&path, &line)
    }

    fn records(&self, session_id: &str) -> Result<Vec<SessionLogRecord>, OrchestratorError> {
        let path = self.records_path(session_id);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(OrchestratorError::Serialization(format!(
                    "failed to read session log {}: {error}",
                    path.display()
                )));
            }
        };
        let mut records = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let record = serde_json::from_slice(line).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "session log {} line {} failed to deserialize: {error}",
                    path.display(),
                    index + 1
                ))
            })?;
            records.push(record);
        }
        Ok(records)
    }

    fn put_body(
        &self,
        session_id: &str,
        content_hash: &str,
        body: &[u8],
    ) -> Result<(), OrchestratorError> {
        if !is_content_hash(content_hash) {
            return Err(OrchestratorError::InvalidState(
                "session log body hash must be 64 lowercase hex characters".into(),
            ));
        }
        let directory = self.session_dir(session_id).join("bodies");
        create_private_dir(&directory)?;
        let path = self.body_path(session_id, content_hash);
        match write_private_file(&path, body) {
            Ok(()) => Ok(()),
            // Content-addressed writes are idempotent: an existing body is
            // byte-identical by construction.
            Err(_) if path.is_file() => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn body(
        &self,
        session_id: &str,
        content_hash: &str,
    ) -> Result<Option<Vec<u8>>, OrchestratorError> {
        if !is_content_hash(content_hash) {
            return Ok(None);
        }
        let path = self.body_path(session_id, content_hash);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OrchestratorError::Serialization(format!(
                "failed to read session log body {}: {error}",
                path.display()
            ))),
        }
    }

    fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.root)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }
}

pub(crate) fn is_content_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn sanitize_component(value: &str) -> String {
    let mut name: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        name.push('_');
    }
    name
}
