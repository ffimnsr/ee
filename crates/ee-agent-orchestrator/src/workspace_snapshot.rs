//! Content-addressed workspace snapshots, per-turn diffs, and selective file
//! restore.
//!
//! A turn captures its workspace before the first model call and after the
//! turn settles.  Files are stored as SHA-256-named objects under the session
//! state directory, the manifest is content-addressed by its sorted entry
//! list, and ignored files are excluded by git's own exclude rules (or the
//! `ignore` walker in a non-git workspace).
//!
//! The store is bounded: a capture stops after a deterministic file/byte cap
//! and records `truncated` on the manifest.  Snapshots are pruned with the
//! session lifecycle alongside `SessionLog::purge_artifacts`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;
use crate::private_fs::{create_private_dir, hex, write_private_file};
use crate::session_log::SessionLogConfig;
use crate::session_log_sink::sanitize_component;

/// Snapshot manifest schema version.
pub const WORKSPACE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Default maximum files captured per snapshot.
pub const DEFAULT_MAX_SNAPSHOT_FILES: usize = 20_000;
/// Default maximum size of one captured file.
pub const DEFAULT_MAX_SNAPSHOT_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Default maximum size of one untracked file (non-git fallback uses this too).
pub const DEFAULT_MAX_SNAPSHOT_UNTRACKED_BYTES: usize = 1024 * 1024;
/// Default maximum total captured bytes per snapshot.
pub const DEFAULT_MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
/// Default maximum rendered patch size per turn diff.
pub const DEFAULT_MAX_PATCH_BYTES: usize = 64 * 1024;

/// Workspace snapshot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceSnapshotConfig {
    /// Whether turns capture workspace snapshots.
    pub enabled: bool,
    /// Maximum files captured per snapshot.
    pub max_files: usize,
    /// Maximum size of one tracked/captured file.
    pub max_file_bytes: usize,
    /// Maximum size of one untracked file in a git workspace.
    pub max_untracked_bytes: usize,
    /// Maximum total captured bytes per snapshot.
    pub max_snapshot_bytes: usize,
    /// Maximum rendered patch bytes in one turn diff.
    pub max_patch_bytes: usize,
}

impl Default for WorkspaceSnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_files: DEFAULT_MAX_SNAPSHOT_FILES,
            max_file_bytes: DEFAULT_MAX_SNAPSHOT_FILE_BYTES,
            max_untracked_bytes: DEFAULT_MAX_SNAPSHOT_UNTRACKED_BYTES,
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
        }
    }
}

impl WorkspaceSnapshotConfig {
    /// Creates a disabled configuration.
    #[must_use]
    pub fn disabled() -> Self {
        Self { enabled: false, ..Self::default() }
    }
}

/// Which side of a turn a snapshot represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOrigin {
    /// Captured before the turn's first model call.
    Start,
    /// Captured after the turn settled.
    End,
}

/// One captured file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Canonical workspace root the file belongs to.
    pub root: String,
    /// Workspace-relative path.
    pub path: String,
    /// SHA-256 of the file bytes.
    pub blob: String,
    /// File size in bytes.
    pub size: u64,
}

impl SnapshotEntry {
    /// Display path: relative for the primary root, `root/path` otherwise.
    #[must_use]
    pub fn display_path(&self, primary_root: &Path) -> String {
        if Path::new(&self.root) == canonical_root(primary_root) {
            self.path.clone()
        } else {
            format!("{}/{}", self.root, self.path)
        }
    }
}

/// One content-addressed snapshot manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Owning session.
    pub session_id: String,
    /// Turn sequence.
    pub turn: u64,
    /// Start or end capture.
    pub origin: SnapshotOrigin,
    /// Sorted captured entries.
    pub entries: Vec<SnapshotEntry>,
    /// Whether a bound stopped the capture early.
    pub truncated: bool,
}

impl WorkspaceSnapshot {
    /// Content-addressed snapshot id.
    #[must_use]
    pub fn id(&self) -> String {
        snapshot_id(&self.entries)
    }
}

/// Per-turn start/end snapshot pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSnapshotRecord {
    /// Turn sequence.
    pub turn: u64,
    /// Snapshot id captured at turn start.
    pub start: String,
    /// Snapshot id captured at turn end, once the turn settled.
    pub end: Option<String>,
}

/// What changed for one path between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffChange {
    /// The file exists only after the turn.
    Added,
    /// The file exists in both snapshots with different bytes.
    Modified,
    /// The file exists only before the turn.
    Deleted,
}

/// One changed file and its bounded unified patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Display path.
    pub path: String,
    /// Change kind.
    pub change: DiffChange,
    /// Unified patch, absent for binary or over-cap content.
    pub patch: Option<String>,
}

/// Files changed by one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDiff {
    /// Turn sequence.
    pub turn: u64,
    /// Start snapshot id.
    pub start: String,
    /// End snapshot id.
    pub end: String,
    /// Changed files in sorted path order.
    pub files: Vec<FileDiff>,
}

#[derive(Default)]
struct MemorySession {
    snapshots: HashMap<String, WorkspaceSnapshot>,
    objects: HashMap<String, Vec<u8>>,
    turns: BTreeMap<u64, TurnSnapshotRecord>,
}

/// Content-addressed snapshot store for one process.
pub struct WorkspaceSnapshotStore {
    config: WorkspaceSnapshotConfig,
    root: Option<PathBuf>,
    memory: Mutex<HashMap<String, MemorySession>>,
}

impl std::fmt::Debug for WorkspaceSnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceSnapshotStore")
            .field("enabled", &self.config.enabled)
            .field("durable", &self.root.is_some())
            .finish_non_exhaustive()
    }
}

impl WorkspaceSnapshotStore {
    /// Builds a store from the snapshot config and the session-log state dir.
    #[must_use]
    pub fn from_config(config: &WorkspaceSnapshotConfig, log: &SessionLogConfig) -> Self {
        Self {
            config: config.clone(),
            root: log.dir.as_ref().map(|dir| dir.join("snapshots")),
            memory: Mutex::new(HashMap::new()),
        }
    }

    /// Whether capture is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Captures the start of one turn and records the turn pair.
    pub fn begin_turn(
        &self,
        session_id: &str,
        roots: &[PathBuf],
    ) -> Result<TurnSnapshotRecord, OrchestratorError> {
        if !self.config.enabled {
            return Err(OrchestratorError::InvalidState("workspace snapshots are disabled".into()));
        }
        let turn = self.next_turn(session_id)?;
        let snapshot = self.capture(session_id, roots, turn, SnapshotOrigin::Start)?;
        let start = snapshot.id();
        self.save_snapshot(session_id, &snapshot)?;
        let record = TurnSnapshotRecord { turn, start, end: None };
        self.save_turn(session_id, &record)?;
        Ok(record)
    }

    /// Captures the end of one turn and records its snapshot id.
    pub fn finish_turn(
        &self,
        session_id: &str,
        turn: u64,
        roots: &[PathBuf],
    ) -> Result<TurnSnapshotRecord, OrchestratorError> {
        if !self.config.enabled {
            return Err(OrchestratorError::InvalidState("workspace snapshots are disabled".into()));
        }
        let snapshot = self.capture(session_id, roots, turn, SnapshotOrigin::End)?;
        let end = snapshot.id();
        self.save_snapshot(session_id, &snapshot)?;
        let mut record = self.turn_record(session_id, turn)?.ok_or_else(|| {
            OrchestratorError::InvalidState(format!("turn {turn} has no start snapshot"))
        })?;
        record.end = Some(end);
        self.save_turn(session_id, &record)?;
        Ok(record)
    }

    /// Reads one turn record.
    pub fn turn_record(
        &self,
        session_id: &str,
        turn: u64,
    ) -> Result<Option<TurnSnapshotRecord>, OrchestratorError> {
        if let Some(memory) = self.memory.lock().expect("snapshot memory poisoned").get(session_id)
        {
            return Ok(memory.turns.get(&turn).cloned());
        }
        let Some(root) = &self.root else { return Ok(None) };
        let path = Self::session_dir(root, session_id).join("turns").join(format!("{turn}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "turn snapshot record {} failed to parse: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OrchestratorError::Serialization(format!(
                "failed to read turn snapshot record {}: {error}",
                path.display()
            ))),
        }
    }

    /// Latest turn that captured any snapshot.
    pub fn latest_turn(&self, session_id: &str) -> Result<Option<u64>, OrchestratorError> {
        if let Some(memory) = self.memory.lock().expect("snapshot memory poisoned").get(session_id)
        {
            return Ok(memory.turns.keys().next_back().copied());
        }
        let Some(root) = &self.root else { return Ok(None) };
        let directory = Self::session_dir(root, session_id).join("turns");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(OrchestratorError::Serialization(format!(
                    "failed to read snapshot turns {}: {error}",
                    directory.display()
                )));
            }
        };
        let mut latest = None;
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
            let Some(turn) = name.strip_suffix(".json").and_then(|turn| turn.parse::<u64>().ok())
            else {
                continue;
            };
            latest = latest.max(Some(turn));
        }
        Ok(latest)
    }

    /// Diffs one turn's start and end snapshots.
    pub fn turn_diff(
        &self,
        session_id: &str,
        turn: u64,
        primary_root: &Path,
    ) -> Result<Option<TurnDiff>, OrchestratorError> {
        let Some(record) = self.turn_record(session_id, turn)? else { return Ok(None) };
        let Some(end) = record.end else { return Ok(None) };
        let start_snapshot = self.load_snapshot(session_id, &record.start)?.ok_or_else(|| {
            OrchestratorError::InvalidState(format!("start snapshot {} is missing", record.start))
        })?;
        let end_snapshot = self.load_snapshot(session_id, &end)?.ok_or_else(|| {
            OrchestratorError::InvalidState(format!("end snapshot {end} is missing"))
        })?;
        let start_entries = entry_map(&start_snapshot);
        let end_entries = entry_map(&end_snapshot);
        let mut keys: Vec<(String, String)> =
            start_entries.keys().chain(end_entries.keys()).cloned().collect();
        keys.sort();
        keys.dedup();
        let mut files = Vec::new();
        for key in keys {
            let before = start_entries.get(&key);
            let after = end_entries.get(&key);
            let change = match (before, after) {
                (None, Some(_)) => DiffChange::Added,
                (Some(_), None) => DiffChange::Deleted,
                (Some(before), Some(after)) if before.blob != after.blob => DiffChange::Modified,
                _ => continue,
            };
            let display =
                after.or(before).map(|entry| entry.display_path(primary_root)).unwrap_or_default();
            let patch = self.render_patch(session_id, before, after)?;
            files.push(FileDiff { path: display, change, patch });
        }
        Ok(Some(TurnDiff { turn, start: record.start, end, files }))
    }

    /// Restores selected paths from a turn's pre-turn snapshot.
    ///
    /// A requested path absent from the start snapshot is removed from the
    /// primary root (it was added during the turn).  Returns the restored
    /// display paths.
    pub fn restore_turn_files(
        &self,
        session_id: &str,
        turn: u64,
        paths: &[String],
        roots: &[PathBuf],
    ) -> Result<Vec<String>, OrchestratorError> {
        let Some(record) = self.turn_record(session_id, turn)? else {
            return Err(OrchestratorError::InvalidState(format!(
                "turn {turn} has no snapshot record"
            )));
        };
        let snapshot = self.load_snapshot(session_id, &record.start)?.ok_or_else(|| {
            OrchestratorError::InvalidState(format!("start snapshot {} is missing", record.start))
        })?;
        let roots = canonical_roots(roots);
        let primary = primary_root(&roots)?;
        let mut restored = Vec::new();
        for requested in paths {
            let entry = find_entry(&snapshot, requested, &primary, &roots);
            match entry {
                Some(entry) => {
                    let bytes = self.load_object(session_id, &entry.blob)?.ok_or_else(|| {
                        OrchestratorError::InvalidState(format!(
                            "snapshot object {} is missing",
                            entry.blob
                        ))
                    })?;
                    let (root, relative) = resolve_entry_path(entry, &primary, &roots)?;
                    let target = safe_join(&root, &relative)?;
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            OrchestratorError::Serialization(format!(
                                "failed to create restore directory {}: {error}",
                                parent.display()
                            ))
                        })?;
                    }
                    std::fs::write(&target, &bytes).map_err(|error| {
                        OrchestratorError::Serialization(format!(
                            "failed to restore {}: {error}",
                            target.display()
                        ))
                    })?;
                    restored.push(entry.display_path(&primary));
                }
                None => {
                    // The path did not exist before the turn: undo the add.
                    let relative = normalize_requested_path(requested)?;
                    let target = safe_join(&primary, &relative)?;
                    match std::fs::remove_file(&target) {
                        Ok(()) => restored.push(requested.clone()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            restored.push(requested.clone());
                        }
                        Err(error) => {
                            return Err(OrchestratorError::Serialization(format!(
                                "failed to remove added file {}: {error}",
                                target.display()
                            )));
                        }
                    }
                }
            }
        }
        Ok(restored)
    }

    /// Removes every snapshot and object for a session.
    pub fn purge_session(&self, session_id: &str) {
        self.memory.lock().expect("snapshot memory poisoned").remove(session_id);
        if let Some(root) = &self.root {
            let directory = Self::session_dir(root, session_id);
            let _ = std::fs::remove_dir_all(directory);
        }
    }

    fn next_turn(&self, session_id: &str) -> Result<u64, OrchestratorError> {
        if let Some(memory) = self.memory.lock().expect("snapshot memory poisoned").get(session_id)
        {
            return Ok(memory.turns.keys().next_back().copied().unwrap_or(0) + 1);
        }
        Ok(self.latest_turn(session_id)?.unwrap_or(0) + 1)
    }

    fn capture(
        &self,
        session_id: &str,
        roots: &[PathBuf],
        turn: u64,
        origin: SnapshotOrigin,
    ) -> Result<WorkspaceSnapshot, OrchestratorError> {
        let roots = canonical_roots(roots);
        let mut files = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            files.extend(collect_files(root, index == 0, &self.config)?);
        }
        files.sort();
        files.dedup();
        let mut entries = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        for (root, relative) in files {
            if entries.len() >= self.config.max_files {
                truncated = true;
                break;
            }
            let target = safe_join(&root, &relative)?;
            let metadata = match std::fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                _ => continue,
            };
            if metadata.len() as usize > self.config.max_file_bytes {
                truncated = true;
                continue;
            }
            if total.saturating_add(metadata.len() as usize) > self.config.max_snapshot_bytes {
                truncated = true;
                break;
            }
            let bytes = match std::fs::read(&target) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            total += bytes.len();
            let blob = hex(&Sha256::digest(&bytes));
            self.save_object(session_id, &blob, &bytes)?;
            entries.push(SnapshotEntry {
                root: root.to_string_lossy().to_string(),
                path: relative.to_string_lossy().to_string(),
                blob,
                size: bytes.len() as u64,
            });
        }
        entries.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
        Ok(WorkspaceSnapshot {
            schema_version: WORKSPACE_SNAPSHOT_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            turn,
            origin,
            entries,
            truncated,
        })
    }

    fn save_snapshot(
        &self,
        session_id: &str,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<(), OrchestratorError> {
        let id = snapshot.id();
        if let Some(root) = &self.root {
            let directory = Self::session_dir(root, session_id).join("manifests");
            create_private_dir(&directory)?;
            let bytes = serde_json::to_vec(snapshot).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "workspace snapshot serialization failed: {error}"
                ))
            })?;
            return write_replace_file(&directory.join(format!("{id}.json")), &bytes);
        }
        self.memory
            .lock()
            .expect("snapshot memory poisoned")
            .entry(session_id.to_string())
            .or_default()
            .snapshots
            .insert(id, snapshot.clone());
        Ok(())
    }

    fn load_snapshot(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<WorkspaceSnapshot>, OrchestratorError> {
        if let Some(memory) = self.memory.lock().expect("snapshot memory poisoned").get(session_id)
        {
            return Ok(memory.snapshots.get(id).cloned());
        }
        let Some(root) = &self.root else { return Ok(None) };
        let path = Self::session_dir(root, session_id).join("manifests").join(format!("{id}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "workspace snapshot {} failed to parse: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OrchestratorError::Serialization(format!(
                "failed to read workspace snapshot {}: {error}",
                path.display()
            ))),
        }
    }

    fn save_turn(
        &self,
        session_id: &str,
        record: &TurnSnapshotRecord,
    ) -> Result<(), OrchestratorError> {
        if let Some(root) = &self.root {
            let directory = Self::session_dir(root, session_id).join("turns");
            create_private_dir(&directory)?;
            let bytes = serde_json::to_vec(record).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "turn snapshot record serialization failed: {error}"
                ))
            })?;
            return write_replace_file(&directory.join(format!("{}.json", record.turn)), &bytes);
        }
        self.memory
            .lock()
            .expect("snapshot memory poisoned")
            .entry(session_id.to_string())
            .or_default()
            .turns
            .insert(record.turn, record.clone());
        Ok(())
    }

    fn save_object(
        &self,
        session_id: &str,
        blob: &str,
        bytes: &[u8],
    ) -> Result<(), OrchestratorError> {
        if let Some(root) = &self.root {
            let directory = Self::session_dir(root, session_id).join("objects");
            let path = directory.join(blob);
            if path.exists() {
                return Ok(());
            }
            create_private_dir(&directory)?;
            return write_private_file(&path, bytes);
        }
        self.memory
            .lock()
            .expect("snapshot memory poisoned")
            .entry(session_id.to_string())
            .or_default()
            .objects
            .insert(blob.to_string(), bytes.to_vec());
        Ok(())
    }

    fn load_object(
        &self,
        session_id: &str,
        blob: &str,
    ) -> Result<Option<Vec<u8>>, OrchestratorError> {
        if let Some(memory) = self.memory.lock().expect("snapshot memory poisoned").get(session_id)
        {
            return Ok(memory.objects.get(blob).cloned());
        }
        let Some(root) = &self.root else { return Ok(None) };
        let path = Self::session_dir(root, session_id).join("objects").join(blob);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OrchestratorError::Serialization(format!(
                "failed to read snapshot object {}: {error}",
                path.display()
            ))),
        }
    }

    fn render_patch(
        &self,
        session_id: &str,
        before: Option<&SnapshotEntry>,
        after: Option<&SnapshotEntry>,
    ) -> Result<Option<String>, OrchestratorError> {
        let before = match before {
            Some(entry) => self.load_object(session_id, &entry.blob)?,
            None => None,
        };
        let after = match after {
            Some(entry) => self.load_object(session_id, &entry.blob)?,
            None => None,
        };
        let before = before.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok());
        let after = after.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok());
        if before.is_none() && after.is_none() {
            return Ok(None);
        }
        let diff = similar::TextDiff::from_lines(before.unwrap_or(""), after.unwrap_or(""));
        let patch = diff.unified_diff().context_radius(3).header("a", "b").to_string();
        Ok(Some(truncate_utf8(patch, self.config.max_patch_bytes)))
    }

    fn session_dir(root: &Path, session_id: &str) -> PathBuf {
        root.join(sanitize_component(session_id))
    }
}

fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Canonicalizes roots and drops duplicate aliases while preserving order.
fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical = Vec::with_capacity(roots.len());
    for root in roots {
        let root = canonical_root(root);
        if !canonical.contains(&root) {
            canonical.push(root);
        }
    }
    canonical
}

fn snapshot_id(entries: &[SnapshotEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.root.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.blob.as_bytes());
        hasher.update(b"\0");
    }
    hex(&hasher.finalize())
}

fn entry_map(snapshot: &WorkspaceSnapshot) -> BTreeMap<(String, String), SnapshotEntry> {
    snapshot
        .entries
        .iter()
        .map(|entry| ((entry.root.clone(), entry.path.clone()), entry.clone()))
        .collect()
}

fn primary_root(roots: &[PathBuf]) -> Result<PathBuf, OrchestratorError> {
    roots
        .first()
        .cloned()
        .ok_or_else(|| OrchestratorError::InvalidState("no workspace roots to restore into".into()))
}

fn find_entry<'a>(
    snapshot: &'a WorkspaceSnapshot,
    requested: &str,
    primary: &Path,
    roots: &[PathBuf],
) -> Option<&'a SnapshotEntry> {
    let normalized = normalize_requested_path(requested).ok()?;
    for entry in &snapshot.entries {
        let entry_root = Path::new(&entry.root);
        if entry_root == primary && Path::new(&entry.path) == normalized {
            return Some(entry);
        }
        let combined = entry_root.join(&entry.path);
        if combined == primary.join(&normalized)
            || roots.iter().any(|root| combined == root.join(&normalized))
        {
            return Some(entry);
        }
    }
    None
}

fn resolve_entry_path(
    entry: &SnapshotEntry,
    primary: &Path,
    roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf), OrchestratorError> {
    let entry_root = PathBuf::from(&entry.root);
    let root = if entry_root == primary || roots.contains(&entry_root) {
        entry_root
    } else {
        return Err(OrchestratorError::InvalidState(format!(
            "snapshot entry root {} is not a workspace root",
            entry.root
        )));
    };
    Ok((root, PathBuf::from(&entry.path)))
}

/// Rejects absolute or parent-escaping requested paths.
fn normalize_requested_path(requested: &str) -> Result<PathBuf, OrchestratorError> {
    let path = Path::new(requested);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(component, std::path::Component::ParentDir | std::path::Component::RootDir)
        })
    {
        return Err(OrchestratorError::PolicyDenied(format!(
            "snapshot restore path must stay inside the workspace: {requested}"
        )));
    }
    Ok(path.to_path_buf())
}

/// Joins a workspace-relative path and rejects anything that escapes the root.
fn safe_join(root: &Path, relative: &Path) -> Result<PathBuf, OrchestratorError> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(component, std::path::Component::ParentDir | std::path::Component::RootDir)
        })
    {
        return Err(OrchestratorError::PolicyDenied(format!(
            "snapshot path escapes the workspace: {}",
            relative.display()
        )));
    }
    Ok(root.join(relative))
}

fn collect_files(
    root: &Path,
    primary: bool,
    config: &WorkspaceSnapshotConfig,
) -> Result<Vec<(PathBuf, PathBuf)>, OrchestratorError> {
    let mut files = Vec::new();
    let git_tracked = git_files(root, &["ls-files", "-z", "--cached"]);
    let git_untracked = git_files(root, &["ls-files", "-z", "--others", "--exclude-standard"]);
    if git_tracked.is_some() || git_untracked.is_some() {
        for (list, untracked) in
            [(git_tracked.unwrap_or_default(), false), (git_untracked.unwrap_or_default(), true)]
        {
            for relative in list {
                if relative.components().any(|component| component.as_os_str() == ".git") {
                    continue;
                }
                let target = safe_join(root, &relative)?;
                let metadata = match std::fs::symlink_metadata(&target) {
                    Ok(metadata) if metadata.file_type().is_file() => metadata,
                    _ => continue,
                };
                if untracked && metadata.len() as usize > config.max_untracked_bytes {
                    continue;
                }
                files.push((root.to_path_buf(), relative));
            }
        }
        if files.len() > config.max_files {
            files.truncate(config.max_files);
        }
        return Ok(files);
    }
    // Non-git fallback: bounded file-copy walk respecting .ignore/.gitignore
    // available without a repository.
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if files.len() >= config.max_files {
            break;
        }
        let Some(file_type) = entry.file_type() else { continue };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.components().any(|component| component.as_os_str() == ".git") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else { continue };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if std::fs::symlink_metadata(path)
            .map(|metadata| metadata.len() as usize > config.max_untracked_bytes)
            .unwrap_or(true)
        {
            continue;
        }
        files.push((root.to_path_buf(), relative.to_path_buf()));
    }
    let _ = primary;
    Ok(files)
}

fn git_files(root: &Path, args: &[&str]) -> Option<Vec<PathBuf>> {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        files.push(PathBuf::from(text));
    }
    Some(files)
}

fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n… patch truncated\n");
    value
}

/// Atomically replaces an owner-only file (manifests and turn records are
/// rewritten as the turn progresses).
fn write_replace_file(path: &Path, bytes: &[u8]) -> Result<(), OrchestratorError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    write_private_file(&tmp, bytes)?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        OrchestratorError::Serialization(format!(
            "failed to finalize snapshot file {}: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::SessionLogConfig;

    fn store(dir: Option<PathBuf>) -> WorkspaceSnapshotStore {
        let log = match dir {
            Some(dir) => SessionLogConfig::durable(dir),
            None => SessionLogConfig::default(),
        };
        WorkspaceSnapshotStore::from_config(&WorkspaceSnapshotConfig::default(), &log)
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, text).expect("write fixture");
    }

    #[test]
    fn turn_diff_reports_exactly_the_changed_files() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join("a.rs"), "fn a() {}\n");
        write(&root.join("b.rs"), "fn b() {}\n");
        write(&root.join("c.rs"), "fn c() {}\n");
        let store = store(None);

        let record = store.begin_turn("session-a", &[root.to_path_buf()]).expect("start capture");
        write(&root.join("a.rs"), "fn a() { changed(); }\n");
        write(&root.join("b.rs"), "fn b() { changed(); }\n");
        write(&root.join("c.rs"), "fn c() { changed(); }\n");
        store.finish_turn("session-a", record.turn, &[root.to_path_buf()]).expect("end capture");

        let diff = store
            .turn_diff("session-a", record.turn, root)
            .expect("diff reads")
            .expect("turn has a diff");
        let paths: Vec<_> = diff.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "c.rs"]);
        assert!(diff.files.iter().all(|file| file.change == DiffChange::Modified));
        assert!(
            diff.files
                .iter()
                .all(|file| file.patch.as_deref().is_some_and(|patch| patch.contains("changed")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn display_path_matches_a_symlinked_primary_root() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        let holder = tempfile::tempdir().expect("holder");
        let alias = holder.path().join("workspace-alias");
        symlink(root, &alias).expect("symlink workspace");
        let entry = SnapshotEntry {
            root: std::fs::canonicalize(root)
                .expect("canonical root")
                .to_string_lossy()
                .into_owned(),
            path: "a.rs".to_string(),
            blob: "deadbeef".to_string(),
            size: 1,
        };
        assert_eq!(entry.display_path(&alias), "a.rs");
    }

    #[cfg(unix)]
    #[test]
    fn canonical_roots_dedupe_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        let holder = tempfile::tempdir().expect("holder");
        let alias = holder.path().join("workspace-alias");
        symlink(root, &alias).expect("symlink workspace");
        let roots = canonical_roots(&[root.to_path_buf(), alias]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], std::fs::canonicalize(root).expect("canonical root"));
    }

    #[test]
    fn selective_restore_restores_one_file_and_leaves_the_other() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join("a.rs"), "original-a\n");
        write(&root.join("b.rs"), "original-b\n");
        let store = store(None);
        let record = store.begin_turn("session-a", &[root.to_path_buf()]).expect("start capture");
        write(&root.join("a.rs"), "changed-a\n");
        write(&root.join("b.rs"), "changed-b\n");
        store.finish_turn("session-a", record.turn, &[root.to_path_buf()]).expect("end capture");

        store
            .restore_turn_files(
                "session-a",
                record.turn,
                &["a.rs".to_string()],
                &[root.to_path_buf()],
            )
            .expect("restore selected path");
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).expect("a"), "original-a\n");
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).expect("b"), "changed-b\n");
    }

    #[test]
    fn selective_restore_removes_a_file_added_by_the_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join("keep.rs"), "keep\n");
        let store = store(None);
        let record = store.begin_turn("session-a", &[root.to_path_buf()]).expect("start capture");
        write(&root.join("added.rs"), "new file\n");
        store.finish_turn("session-a", record.turn, &[root.to_path_buf()]).expect("end capture");

        store
            .restore_turn_files(
                "session-a",
                record.turn,
                &["added.rs".to_string()],
                &[root.to_path_buf()],
            )
            .expect("restore removes the added file");
        assert!(!root.join("added.rs").exists());
        assert!(root.join("keep.rs").exists());
    }

    #[test]
    fn ignored_files_are_excluded_from_snapshots() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join(".ignore"), "ignored.txt\n");
        write(&root.join("tracked.rs"), "tracked\n");
        write(&root.join("ignored.txt"), "ignored\n");
        let store = store(None);
        store.begin_turn("session-a", &[root.to_path_buf()]).expect("capture");
        let latest = store.latest_turn("session-a").expect("latest").expect("turn exists");
        let record = store.turn_record("session-a", latest).expect("record").expect("exists");
        let snapshot =
            store.load_snapshot("session-a", &record.start).expect("load").expect("snapshot");
        let paths: Vec<_> = snapshot.entries.iter().map(|entry| entry.path.as_str()).collect();
        assert!(paths.contains(&"tracked.rs"));
        assert!(!paths.contains(&"ignored.txt"), "ignored file captured: {paths:?}");
    }

    #[test]
    fn durable_snapshot_storage_is_pruned_with_the_session() {
        let state = tempfile::tempdir().expect("state");
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        write(&root.join("a.rs"), "a\n");
        let store = store(Some(state.path().to_path_buf()));
        let record = store.begin_turn("session-a", &[root.to_path_buf()]).expect("capture");
        store.finish_turn("session-a", record.turn, &[root.to_path_buf()]).expect("end capture");
        let session_dir = state.path().join("snapshots").join(sanitize_component("session-a"));
        assert!(session_dir.exists(), "durable snapshot dir exists before purge");

        store.purge_session("session-a");
        assert!(!session_dir.exists(), "snapshot storage follows session lifecycle");
        assert!(store.turn_record("session-a", record.turn).expect("record read").is_none());
    }
}
