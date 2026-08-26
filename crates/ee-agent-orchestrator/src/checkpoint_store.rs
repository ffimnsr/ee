//! Durable, bounded checkpoint store.
//!
//! Checkpoints are written as JSON files (synced temp file + rename + parent
//! directory sync where supported), each carrying a SHA-256 checksum over the
//! payload so corruption is detected before restore. This is best-effort
//! crash consistency, not a filesystem-independent power-loss guarantee.  Per-session retention and a global TTL keep the store
//! bounded; expired or over-cap entries are pruned on access.  With
//! `checkpoint_dir: None` the store degrades to an in-memory map so
//! same-process recovery metadata still works, but crash restore is not available.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoint::{
    CheckpointCaptureMetadata, CheckpointContextProvenance, OrchestratorCheckpoint,
    current_unix_millis,
};
use crate::config::RecoveryConfig;
use crate::error::OrchestratorError;

/// Minimum interval between milestone checkpoint captures per turn; the
/// interruption capture always writes regardless of the debounce.
pub const CHECKPOINT_MILESTONE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Metadata for one stored checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointMeta {
    /// Stable per-session identity (`<session-id>-<seq>`).
    pub id: String,
    /// Capture time in Unix millis.
    pub created_at_millis: u64,
    /// Serialized payload bytes.
    pub bytes: usize,
}

/// One stored checkpoint entry (file or memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCheckpoint {
    seq: u64,
    created_at_millis: u64,
    checksum: String,
    checkpoint: OrchestratorCheckpoint,
}

/// Bounded checkpoint persistence.
#[derive(Debug)]
pub struct CheckpointStore {
    dir: Option<PathBuf>,
    ttl: Duration,
    max_bytes: usize,
    max_per_session: usize,
    /// In-memory fallback used when `dir` is `None` (and as the directory
    /// index otherwise, so listing does not re-read the filesystem).
    entries: Mutex<HashMap<String, BTreeMap<u64, StoredCheckpoint>>>,
    next_seq: Mutex<HashMap<String, u64>>,
}

impl CheckpointStore {
    /// Creates a store from recovery config.  With no `checkpoint_dir` the
    /// store is memory-only.
    #[must_use]
    pub fn new(config: &RecoveryConfig) -> Self {
        let store = Self {
            dir: config.checkpoint_dir.clone(),
            ttl: config.checkpoint_ttl,
            max_bytes: config.max_checkpoint_bytes,
            max_per_session: config.max_checkpoints_per_session,
            entries: Mutex::new(HashMap::new()),
            next_seq: Mutex::new(HashMap::new()),
        };
        if let Some(dir) = &store.dir {
            // Construction cannot report I/O failure; `save` repeats this and
            // fails closed if restrictive durable storage cannot be prepared.
            let _ = create_private_dir(dir);
        }
        store
    }

    /// Whether the store persists to disk (crash-restore capable).
    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.dir.is_some()
    }

    /// Whether the store has a non-expired checkpoint for `session_id`.
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.load_latest(session_id).is_ok_and(|latest| latest.is_some())
    }

    /// Persists a checkpoint for `session_id`, returning its stable id.
    /// Fail-closed: payloads above `max_checkpoint_bytes` are rejected.
    pub fn save(
        &self,
        session_id: &str,
        checkpoint: &OrchestratorCheckpoint,
    ) -> Result<String, OrchestratorError> {
        let checkpoint = checkpoint.persistence_safe_copy()?;
        let payload = serde_json::to_vec(&checkpoint).map_err(|error| {
            OrchestratorError::Serialization(format!("checkpoint serialization failed: {error}"))
        })?;
        if payload.len() > self.max_bytes {
            return Err(OrchestratorError::Serialization(format!(
                "checkpoint payload {} bytes exceeds the {} byte cap",
                payload.len(),
                self.max_bytes
            )));
        }
        let checksum = hex(&Sha256::digest(&payload));
        let seq = self.allocate_seq(session_id);
        let stored = StoredCheckpoint {
            seq,
            created_at_millis: checkpoint.created_at_millis,
            checksum,
            checkpoint: checkpoint.clone(),
        };
        if let Some(dir) = &self.dir {
            self.write_file(dir, session_id, &stored)?;
        }
        let mut entries = self.entries.lock().expect("checkpoint entries poisoned");
        let per_session = entries.entry(session_id.to_string()).or_default();
        per_session.insert(seq, stored);
        self.prune_locked(session_id, per_session);
        Ok(self.id(session_id, seq))
    }

    /// Latest checkpoint for `session_id` (newest seq), when one exists and
    /// has not expired.  The checksum is verified before the payload is
    /// trusted; mismatches are treated as corruption and rejected.
    pub fn load_latest(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, OrchestratorCheckpoint)>, OrchestratorError> {
        self.refresh(session_id)?;
        let entries = self.entries.lock().expect("checkpoint entries poisoned");
        let Some(per_session) = entries.get(session_id) else {
            return Ok(None);
        };
        let Some((&seq, stored)) = per_session.last_key_value() else {
            return Ok(None);
        };
        let checkpoint = self.verify_and_take(session_id, seq, stored)?;
        Ok(Some((self.id(session_id, seq), checkpoint)))
    }

    /// Metadata for every non-expired checkpoint of `session_id`, oldest
    /// first.
    pub fn list(&self, session_id: &str) -> Result<Vec<CheckpointMeta>, OrchestratorError> {
        self.refresh(session_id)?;
        let entries = self.entries.lock().expect("checkpoint entries poisoned");
        Ok(entries
            .get(session_id)
            .map(|per_session| {
                per_session
                    .iter()
                    .map(|(seq, stored)| CheckpointMeta {
                        id: self.id(session_id, *seq),
                        created_at_millis: stored.created_at_millis,
                        bytes: serde_json::to_vec(&stored.checkpoint)
                            .map_or(0, |bytes| bytes.len()),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Deletes every checkpoint of `session_id` (explicit close / discard).
    pub fn delete_session(&self, session_id: &str) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(session_dir(dir, session_id));
        }
        self.entries.lock().expect("checkpoint entries poisoned").remove(session_id);
        self.next_seq.lock().expect("checkpoint seq poisoned").remove(session_id);
    }

    /// Every session id with at least one stored checkpoint, in stable
    /// order.  Durable stores scan the checkpoint directory so sessions
    /// written by a previous process are visible; memory stores report the
    /// in-process keys only.
    pub fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = match &self.dir {
            Some(dir) => std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .filter_map(|entry| {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            // Only ids that pass the directory-sanitization
                            // roundtrip are trusted.
                            (session_dir(dir, &name) == entry.path()).then_some(name)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        ids.extend(self.entries.lock().expect("checkpoint entries poisoned").keys().cloned());
        ids.sort();
        ids.dedup();
        ids
    }

    /// Prunes expired checkpoints across all sessions.
    pub fn prune_expired(&self) {
        let now = current_unix_millis();
        let ttl_millis = self.ttl.as_millis() as u64;
        let mut entries = self.entries.lock().expect("checkpoint entries poisoned");
        let mut expired_ids = Vec::new();
        for (session_id, per_session) in entries.iter_mut() {
            let before = per_session.len();
            per_session
                .retain(|_, stored| now.saturating_sub(stored.created_at_millis) <= ttl_millis);
            if per_session.len() != before {
                expired_ids.push(session_id.clone());
            }
        }
        drop(entries);
        if let Some(dir) = &self.dir {
            for session_id in expired_ids {
                let _ = std::fs::remove_dir_all(session_dir(dir, &session_id));
            }
        }
    }

    /// Sanitizes a session id for use as a directory name.
    fn id(&self, session_id: &str, seq: u64) -> String {
        format!("{session_id}-{seq:010}")
    }

    fn allocate_seq(&self, session_id: &str) -> u64 {
        let mut next = self.next_seq.lock().expect("checkpoint seq poisoned");
        let seq = next.get(session_id).copied().unwrap_or(1);
        next.insert(session_id.to_string(), seq + 1);
        seq
    }

    /// Re-reads the filesystem (durable mode) and prunes expired/over-cap
    /// entries for one session.
    fn refresh(&self, session_id: &str) -> Result<(), OrchestratorError> {
        let now = current_unix_millis();
        let ttl_millis = self.ttl.as_millis() as u64;
        let mut entries = self.entries.lock().expect("checkpoint entries poisoned");
        let per_session = entries.entry(session_id.to_string()).or_default();
        if let Some(dir) = &self.dir {
            let path = session_dir(dir, session_id);
            let mut disk = BTreeMap::new();
            if path.is_dir() {
                for entry in std::fs::read_dir(&path).map_err(|error| {
                    OrchestratorError::Serialization(format!(
                        "failed to read checkpoint directory {}: {error}",
                        path.display()
                    ))
                })? {
                    let entry = entry.map_err(|error| {
                        OrchestratorError::Serialization(format!(
                            "failed to read checkpoint directory entry: {error}"
                        ))
                    })?;
                    let Some(seq) = seq_from_name(&entry.file_name().to_string_lossy()) else {
                        continue;
                    };
                    let stored = self.read_file(&path, seq)?;
                    disk.insert(seq, stored);
                }
            }
            // The filesystem is the source of truth in durable mode; drop
            // memory entries that are not on disk (deleted externally).
            *per_session = disk;
        }
        per_session.retain(|_, stored| now.saturating_sub(stored.created_at_millis) <= ttl_millis);
        self.prune_locked(session_id, per_session);
        Ok(())
    }

    /// Keeps only the newest `max_per_session` entries.
    fn prune_locked(&self, session_id: &str, per_session: &mut BTreeMap<u64, StoredCheckpoint>) {
        let overflow: Vec<u64> = per_session
            .keys()
            .take(per_session.len().saturating_sub(self.max_per_session))
            .copied()
            .collect();
        for seq in overflow {
            per_session.remove(&seq);
            if let Some(dir) = &self.dir {
                let _ = std::fs::remove_file(seq_path(dir, session_id, seq));
            }
        }
    }

    /// Recomputes the checksum and deserializes; corruption fails closed.
    fn verify_and_take(
        &self,
        session_id: &str,
        seq: u64,
        stored: &StoredCheckpoint,
    ) -> Result<OrchestratorCheckpoint, OrchestratorError> {
        let expected = stored.checksum.clone();
        let payload = if let Some(dir) = &self.dir {
            match self.read_payload(dir, session_id, seq) {
                Ok(Some(payload)) => payload,
                Ok(None) => {
                    return Err(OrchestratorError::Serialization(format!(
                        "checkpoint file {} disappeared before restore",
                        self.id(session_id, seq)
                    )));
                }
                Err(error) => return Err(error),
            }
        } else {
            serde_json::to_vec(&stored.checkpoint).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "checkpoint re-serialization failed: {error}"
                ))
            })?
        };
        let actual = hex(&Sha256::digest(&payload));
        if actual != expected {
            return Err(OrchestratorError::Serialization(format!(
                "checkpoint {} checksum mismatch (corrupt or tampered)",
                self.id(session_id, seq)
            )));
        }
        let checkpoint: OrchestratorCheckpoint =
            serde_json::from_slice(&payload).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "checkpoint {} failed to deserialize: {error}",
                    self.id(session_id, seq)
                ))
            })?;
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    fn write_file(
        &self,
        dir: &Path,
        session_id: &str,
        stored: &StoredCheckpoint,
    ) -> Result<(), OrchestratorError> {
        create_private_dir(dir)?;
        let session = session_dir(dir, session_id);
        create_private_dir(&session)?;
        let envelope = serde_json::to_vec(&stored).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "checkpoint envelope serialization failed: {error}"
            ))
        })?;
        let final_path = seq_path(dir, session_id, stored.seq);
        let tmp_path = session.join(format!("tmp-{}-{}.json", std::process::id(), stored.seq));
        write_private_file(&tmp_path, &envelope)?;
        std::fs::rename(&tmp_path, &final_path).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to finalize checkpoint {}: {error}",
                final_path.display()
            ))
        })?;
        sync_directory(&session)?;
        Ok(())
    }

    fn read_file(&self, session: &Path, seq: u64) -> Result<StoredCheckpoint, OrchestratorError> {
        let path = session.join(format!("{seq:010}.json"));
        let bytes = std::fs::read(&path).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to read checkpoint {}: {error}",
                path.display()
            ))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "checkpoint {} failed to deserialize: {error}",
                path.display()
            ))
        })
    }

    fn read_payload(
        &self,
        dir: &Path,
        session_id: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, OrchestratorError> {
        let stored = self.read_file(&session_dir(dir, session_id), seq)?;
        Ok(Some(serde_json::to_vec(&stored.checkpoint).map_err(|error| {
            OrchestratorError::Serialization(format!("checkpoint re-serialization failed: {error}"))
        })?))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Creates a durable-store directory with owner-only permissions where the
/// platform exposes POSIX modes. Existing directories are tightened too.
fn create_private_dir(path: &Path) -> Result<(), OrchestratorError> {
    std::fs::create_dir_all(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create checkpoint directory {}: {error}",
            path.display()
        ))
    })?;
    set_private_directory_permissions(path)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), OrchestratorError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create checkpoint {}: {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to durably write checkpoint {}: {error}",
            path.display()
        ))
    })?;
    set_private_file_permissions(path)
}

fn sync_directory(path: &Path) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        std::fs::File::open(path).and_then(|directory| directory.sync_all()).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to sync checkpoint directory {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict checkpoint directory {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict checkpoint {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}

/// Per-turn checkpoint writer wired into the loop engine by the runtime.
///
/// Milestone captures are debounced (at most one per [`CHECKPOINT_MILESTONE_DEBOUNCE`])
/// so tool-heavy turns do not rewrite the store on every tool result; the
/// interruption capture bypasses the debounce and always persists.
#[derive(Clone, Debug)]
pub struct CheckpointHandle {
    store: Arc<CheckpointStore>,
    session_id: String,
    provider: String,
    capture_context: CheckpointContextProvenance,
    evidence_refs: Vec<crate::observability::RedactedEvidenceRef>,
    last_milestone: Arc<Mutex<Option<Instant>>>,
}

impl CheckpointHandle {
    /// Creates a handle for one session.
    #[must_use]
    pub fn new(
        store: Arc<CheckpointStore>,
        session_id: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            store,
            session_id: session_id.into(),
            provider: provider.into(),
            capture_context: CheckpointContextProvenance::default(),
            evidence_refs: Vec::new(),
            last_milestone: Arc::new(Mutex::new(None)),
        }
    }

    /// Attaches bounded host-derived context revisions and redacted evidence
    /// references to every capture for this turn. The loop only receives these
    /// opaque metadata values; it cannot invent editor observations.
    #[must_use]
    pub fn with_capture_metadata(
        mut self,
        capture_context: CheckpointContextProvenance,
        evidence_refs: Vec<crate::observability::RedactedEvidenceRef>,
    ) -> Self {
        self.capture_context = capture_context;
        self.evidence_refs = evidence_refs;
        self
    }

    /// Produces metadata for one capture origin.
    #[must_use]
    pub fn capture_metadata(
        &self,
        origin: crate::checkpoint::CheckpointCaptureOrigin,
    ) -> CheckpointCaptureMetadata {
        CheckpointCaptureMetadata {
            origin,
            context: self.capture_context.clone(),
            evidence_refs: self.evidence_refs.clone(),
        }
    }

    /// Persists a milestone capture (debounced).  Returns the checkpoint id
    /// when a capture actually happened.
    pub fn save_milestone(
        &self,
        checkpoint: &OrchestratorCheckpoint,
    ) -> Result<Option<String>, OrchestratorError> {
        let mut last = self.last_milestone.lock().expect("checkpoint debounce poisoned");
        if last.is_some_and(|at| at.elapsed() < CHECKPOINT_MILESTONE_DEBOUNCE) {
            return Ok(None);
        }
        let id = self.store.save(&self.session_id, checkpoint)?;
        *last = Some(Instant::now());
        Ok(Some(id))
    }

    /// Persists an interruption/terminal capture, bypassing the debounce.
    pub fn save_terminal(
        &self,
        checkpoint: &OrchestratorCheckpoint,
    ) -> Result<String, OrchestratorError> {
        *self.last_milestone.lock().expect("checkpoint debounce poisoned") = Some(Instant::now());
        self.store.save(&self.session_id, checkpoint)
    }

    /// The store backing this handle.
    #[must_use]
    pub fn store(&self) -> &Arc<CheckpointStore> {
        &self.store
    }

    /// The session this handle persists for.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Provider identity stamped into captured checkpoints.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
}

fn session_dir(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(sanitize(session_id))
}

fn seq_path(dir: &Path, session_id: &str, seq: u64) -> PathBuf {
    session_dir(dir, session_id).join(format!("{seq:010}.json"))
}

/// Session ids in file names are restricted to safe characters.
fn sanitize(session_id: &str) -> String {
    session_id
        .chars()
        .map(
            |ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') { ch } else { '_' },
        )
        .collect()
}

fn seq_from_name(name: &str) -> Option<u64> {
    name.strip_suffix(".json")
        .and_then(|stem| stem.strip_prefix("tmp-").is_none().then_some(stem))
        .and_then(|stem| stem.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{
        CompletedToolCall, IdGeneratorState, ResumeState, SubagentTreeState, TranscriptSummary,
        tool_call_fingerprint,
    };
    use crate::config::OrchestratorConfig;
    use crate::memory::MemoryStore;
    use crate::model::{ModelMessage, ModelRole};
    use crate::tasks::TaskGraph;

    fn config() -> OrchestratorConfig {
        OrchestratorConfig::default()
    }

    fn sample(session_id: &str, tag: &str) -> OrchestratorCheckpoint {
        let config = config();
        let mut tasks = TaskGraph::new();
        let root = tasks.create_root(&format!("{tag} plan"), "plan");
        let mut memory = MemoryStore::new(config.memory_limit_bytes);
        memory
            .insert(crate::memory::MemoryItem::from_task("cwd", "/work", root.id.clone()))
            .expect("inserts");
        OrchestratorCheckpoint::new(
            "test",
            config,
            session_id,
            tasks,
            memory,
            TranscriptSummary::from_transcript(&[]),
            crate::budget::BudgetSnapshot {
                iterations_used: 0,
                iterations_max: 16,
                model_calls_used: 0,
                model_calls_max: 16,
                tool_calls_used: 0,
                tool_calls_max: 180,
                subagents_used: 0,
                subagents_max: 8,
                output_bytes_used: 0,
                output_bytes_max: 1024 * 1024,
                input_tokens_used: None,
                input_tokens_max: None,
                output_tokens_used: None,
                output_tokens_max: None,
            },
            SubagentTreeState::new(),
            IdGeneratorState::new(),
        )
        .expect("sample checkpoint is valid")
    }

    fn memory_store() -> CheckpointStore {
        let config =
            RecoveryConfig { enabled: true, checkpoint_dir: None, ..RecoveryConfig::default() };
        CheckpointStore::new(&config)
    }

    #[test]
    fn memory_store_roundtrips_latest() {
        let store = memory_store();
        let id = store.save("s-1", &sample("s-1", "one")).expect("saves");
        assert!(id.starts_with("s-1-"));
        let (loaded_id, loaded) = store.load_latest("s-1").expect("loads").expect("exists");
        assert_eq!(loaded_id, id);
        assert_eq!(loaded.tasks, sample("s-1", "one").tasks);
        assert_eq!(store.list("s-1").expect("lists").len(), 1);
    }

    #[test]
    fn memory_store_keeps_newest_per_session() {
        let mut config = RecoveryConfig {
            enabled: true,
            max_checkpoints_per_session: 2,
            ..RecoveryConfig::default()
        };
        config.checkpoint_dir = None;
        let store = CheckpointStore::new(&config);
        for index in 0..4 {
            store.save("s-1", &sample("s-1", &format!("v{index}"))).expect("saves");
        }
        let metas = store.list("s-1").expect("lists");
        assert_eq!(metas.len(), 2, "oldest pruned");
        let latest = store.load_latest("s-1").expect("loads").expect("exists");
        assert_eq!(latest.0, metas[1].id);
        // The newest capture wins.
        let tasks = latest.1.tasks.list();
        assert_eq!(tasks[0].title, "v3 plan");
    }

    #[test]
    fn disk_store_is_atomic_and_survives_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = RecoveryConfig {
            enabled: true,
            checkpoint_dir: Some(dir.path().to_path_buf()),
            ..RecoveryConfig::default()
        };
        let store = CheckpointStore::new(&config);
        assert!(store.is_durable());
        let id = store.save("s-1", &sample("s-1", "one")).expect("saves");
        // A fresh store instance reads the same files (crash restore).
        let reloaded = CheckpointStore::new(&config);
        let (loaded_id, loaded) = reloaded.load_latest("s-1").expect("loads").expect("exists");
        assert_eq!(loaded_id, id);
        assert_eq!(loaded.tasks.list()[0].title, "one plan");
        // No temp files left behind.
        let session = session_dir(dir.path(), "s-1");
        let names: Vec<String> = std::fs::read_dir(&session)
            .expect("reads")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["0000000001.json"], "only the finalized file remains");
    }

    #[test]
    fn durable_checkpoint_omits_transcript_tool_data_and_secrets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = RecoveryConfig {
            enabled: true,
            checkpoint_dir: Some(dir.path().to_path_buf()),
            ..RecoveryConfig::default()
        };
        let store = CheckpointStore::new(&config);
        let mut checkpoint = sample("s-1", "one");
        let arguments = serde_json::json!({
            "path": "tool-argument-do-not-persist",
            "api_key": "sk-live-1234567890",
        });
        checkpoint.resume = Some(ResumeState {
            transcript: vec![
                ModelMessage::text(ModelRole::User, "PROMPT_DO_NOT_PERSIST"),
                ModelMessage::text(ModelRole::Assistant, "MODEL_DO_NOT_PERSIST"),
            ],
            active_task_id: checkpoint.tasks.list()[0].id.as_str().to_string(),
            completed_tools: vec![CompletedToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "write_file".into(),
                arguments: arguments.clone(),
                arguments_fingerprint: tool_call_fingerprint("write_file", &arguments)
                    .expect("fingerprint"),
                success: true,
                summary: "TOOL_SUMMARY_DO_NOT_PERSIST".into(),
                side_effect_class: crate::tools::SideEffectClass::Write,
            }],
            in_flight: None,
            resumed_count: 0,
            first_started_at_millis: current_unix_millis(),
        });
        store.save("s-1", &checkpoint).expect("saves");

        let bytes = std::fs::read(session_dir(dir.path(), "s-1").join("0000000001.json"))
            .expect("reads checkpoint");
        let persisted = String::from_utf8(bytes).expect("checkpoint is JSON");
        for forbidden in [
            "PROMPT_DO_NOT_PERSIST",
            "MODEL_DO_NOT_PERSIST",
            "tool-argument-do-not-persist",
            "TOOL_SUMMARY_DO_NOT_PERSIST",
            "sk-live-1234567890",
        ] {
            assert!(!persisted.contains(forbidden), "persisted checkpoint leaked {forbidden}");
        }
        let (_, restored) = store.load_latest("s-1").expect("loads").expect("exists");
        let resume = restored.resume.expect("resume state");
        assert!(resume.transcript.is_empty());
        assert_eq!(resume.completed_tools[0].arguments, serde_json::Value::Null);
        assert!(resume.completed_tools[0].summary.is_empty());
        assert!(!resume.completed_tools[0].arguments_fingerprint.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn durable_checkpoint_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let config = RecoveryConfig {
            enabled: true,
            checkpoint_dir: Some(dir.path().to_path_buf()),
            ..RecoveryConfig::default()
        };
        let store = CheckpointStore::new(&config);
        store.save("s-1", &sample("s-1", "one")).expect("saves");
        let session = session_dir(dir.path(), "s-1");
        let file = session.join("0000000001.json");
        assert_eq!(
            std::fs::metadata(session).expect("session metadata").permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(file).expect("file metadata").permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn disk_store_detects_corruption() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = RecoveryConfig {
            enabled: true,
            checkpoint_dir: Some(dir.path().to_path_buf()),
            ..RecoveryConfig::default()
        };
        let store = CheckpointStore::new(&config);
        store.save("s-1", &sample("s-1", "one")).expect("saves");
        // Corrupt the stored checksum in place (flip one hex digit) so the
        // payload stays valid JSON but no longer matches the hash.
        let session = session_dir(dir.path(), "s-1");
        let path = session.join("0000000001.json");
        let mut bytes = std::fs::read(&path).expect("reads");
        let marker = b"\"checksum\":\"";
        let checksum_pos =
            find_subslice(&bytes, marker).expect("checksum field present") + marker.len();
        let hex_pos = (0..bytes.len())
            .find(|offset| bytes.get(checksum_pos + offset) == Some(&b'0'))
            .expect("checksum has a zero hex digit");
        bytes[checksum_pos + hex_pos] = b'1';
        std::fs::write(&path, &bytes).expect("writes");
        let error = store.load_latest("s-1").expect_err("corruption rejected");
        assert!(
            matches!(error, OrchestratorError::Serialization(ref reason) if reason.contains("checksum mismatch")),
            "{error}"
        );
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    #[test]
    fn expired_checkpoints_are_pruned() {
        let config = RecoveryConfig {
            enabled: true,
            checkpoint_dir: None,
            checkpoint_ttl: Duration::from_secs(60 * 60),
            ..RecoveryConfig::default()
        };
        let store = CheckpointStore::new(&config);
        let mut checkpoint = sample("s-1", "old");
        checkpoint.created_at_millis =
            current_unix_millis().saturating_sub(2 * 24 * 60 * 60 * 1000);
        store.save("s-1", &checkpoint).expect("saves");
        // The stored timestamp is honored, so the entry is already expired.
        store.prune_expired();
        assert!(store.list("s-1").expect("lists").is_empty());
        assert!(store.load_latest("s-1").expect("loads").is_none());
    }

    #[test]
    fn oversized_checkpoints_fail_closed() {
        let mut config =
            RecoveryConfig { enabled: true, max_checkpoint_bytes: 64, ..RecoveryConfig::default() };
        config.checkpoint_dir = None;
        let store = CheckpointStore::new(&config);
        let error = store.save("s-1", &sample("s-1", "big")).expect_err("oversized rejected");
        assert!(
            matches!(error, OrchestratorError::Serialization(ref reason) if reason.contains("byte cap")),
            "{error}"
        );
    }

    #[test]
    fn delete_session_removes_everything() {
        let store = memory_store();
        store.save("s-1", &sample("s-1", "one")).expect("saves");
        store.save("s-1", &sample("s-1", "two")).expect("saves");
        store.delete_session("s-1");
        assert!(store.load_latest("s-1").expect("loads").is_none());
        assert!(store.list("s-1").expect("lists").is_empty());
    }
}
