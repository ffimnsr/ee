//! Append-only audit trail for remote decisions.
//!
//! Every remote trigger and approval decision appends one JSON line. Appends
//! are failure-tolerant: a write error logs a warning and increments a
//! counter exposed through status; it never drops a turn or an approval. The
//! file rotates once at the configured size cap so it cannot grow unbounded.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{SecondsFormat, Utc};
use ee_agent_host::redact::redact_sensitive_text;

/// Maximum characters retained in an audit detail field.
pub const MAX_AUDIT_DETAIL_CHARS: usize = 512;

/// One audit record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuditEntry {
    /// RFC 3339 timestamp in UTC.
    pub timestamp: String,
    /// Event class (`permission_decision`, `turn_start`, `turn_interrupt`).
    pub event: String,
    /// Bounded caller identity (client id or `rules`, `timeout`, `agent`).
    pub actor: String,
    /// Session id, when scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Remote method or ACP method that caused the decision.
    pub method: String,
    /// Decision or outcome label.
    pub decision: String,
    /// Remote permission mode in force.
    pub mode: String,
    /// Remote approval id, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    /// Bounded, redacted detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditEntry {
    /// Builds an entry stamped with the current UTC time.
    #[must_use]
    pub fn new(
        event: impl Into<String>,
        actor: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            event: event.into(),
            actor: actor.into(),
            session_id: None,
            method: method.into(),
            decision: String::new(),
            mode: String::new(),
            request_id: None,
            detail: None,
        }
    }

    /// Sets the decision label.
    #[must_use]
    pub fn decision(mut self, decision: impl Into<String>) -> Self {
        self.decision = decision.into();
        self
    }

    /// Sets the session id.
    #[must_use]
    pub fn session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the permission mode in force.
    #[must_use]
    pub fn mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = mode.into();
        self
    }

    /// Sets the approval id.
    #[must_use]
    pub fn request_id(mut self, request_id: u64) -> Self {
        self.request_id = Some(request_id);
        self
    }

    /// Sets a redacted, bounded detail.
    #[must_use]
    pub fn detail(mut self, detail: impl AsRef<str>) -> Self {
        let redacted = redact_sensitive_text(detail.as_ref());
        let mut chars = redacted.chars();
        let bounded: String = chars.by_ref().take(MAX_AUDIT_DETAIL_CHARS).collect();
        self.detail = Some(if chars.next().is_some() { format!("{bounded}…") } else { bounded });
        self
    }
}

/// Shared audit log.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: Option<PathBuf>,
    max_bytes: u64,
    failures: std::sync::Arc<AtomicU64>,
    write_lock: std::sync::Arc<Mutex<()>>,
}

impl AuditLog {
    /// Creates a log. `None` disables writing but keeps the failure counter.
    #[must_use]
    pub fn new(path: Option<PathBuf>, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes: max_bytes.max(1024),
            failures: std::sync::Arc::new(AtomicU64::new(0)),
            write_lock: std::sync::Arc::new(Mutex::new(())),
        }
    }

    /// A disabled log (tests and in-memory hosts).
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(None, 1)
    }

    /// The file path, when enabled.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Number of failed appends since process start.
    #[must_use]
    pub fn failure_count(&self) -> u64 {
        self.failures.load(Ordering::Acquire)
    }

    /// Appends one entry. Failures are logged, counted, and swallowed.
    pub fn record(&self, entry: &AuditEntry) {
        let Some(path) = &self.path else {
            return;
        };
        let mut line = match serde_json::to_string(entry) {
            Ok(line) => line,
            Err(error) => {
                self.failures.fetch_add(1, Ordering::AcqRel);
                tracing::warn!(%error, "audit entry could not be serialized");
                return;
            }
        };
        line.push('\n');
        let _guard = self.write_lock.lock().expect("audit log poisoned");
        if let Err(error) = self.append(path, &line) {
            self.failures.fetch_add(1, Ordering::AcqRel);
            tracing::warn!(
                %error,
                path = %path.display(),
                "audit append failed; decision is still applied"
            );
        }
    }

    fn append(&self, path: &Path, line: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed(path, line.len())?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
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
        let rotated = path.with_extension("1");
        match std::fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(path, rotated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_json_lines_with_bounded_redacted_details() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit/decisions.jsonl");
        let log = AuditLog::new(Some(path.clone()), 64 * 1024);
        log.record(
            &AuditEntry::new("permission_decision", "hermes", "permissions/respond")
                .decision("deny")
                .session("s-1")
                .mode("ask")
                .request_id(7)
                .detail("Authorization: Bearer supersecret"),
        );
        let text = std::fs::read_to_string(&path).expect("audit file");
        let line: serde_json::Value = serde_json::from_str(text.trim()).expect("json line");
        assert_eq!(line["event"], "permission_decision");
        assert_eq!(line["actor"], "hermes");
        assert_eq!(line["request_id"], 7);
        assert!(!text.contains("supersecret"));
        assert_eq!(log.failure_count(), 0);
    }

    #[test]
    fn rotation_keeps_one_previous_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::new(Some(path.clone()), 1024);
        for _ in 0..40 {
            log.record(
                &AuditEntry::new("turn_start", "driver", "turn/start")
                    .decision("started")
                    .mode("ask")
                    .detail("x".repeat(40)),
            );
        }
        let rotated = path.with_extension("1");
        assert!(rotated.exists(), "the cap must rotate once");
        assert!(std::fs::metadata(&path).expect("current").len() <= 1024);
    }

    #[test]
    fn disabled_log_never_writes() {
        let log = AuditLog::disabled();
        log.record(&AuditEntry::new("turn_start", "driver", "turn/start"));
        assert_eq!(log.failure_count(), 0);
    }
}
