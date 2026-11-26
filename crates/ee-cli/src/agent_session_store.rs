//! Owner-only rotating transcript journal for agent sessions.
//!
//! The client-side transcript is the canonical local record. It is stored as
//! an append-only JSONL journal that rotates by bytes like a log file; old
//! segments are never trimmed or deleted by the writer. `agent-sessions.json`
//! keeps only the registry, with a full transcript retained there only when the
//! journal write fails.
//!
//! Journal records are compact deltas (`Snapshot`, `Set`, `Append`,
//! `Truncate`) derived from a per-session fingerprint baseline. Replay reads
//! every rotated segment in order, then the active journal, so a restart
//! reconstructs the exact in-memory transcript. A torn final line is ignored;
//! malformed content anywhere else fails closed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::agent_pane::TranscriptItem;
use crate::policy::WorkspaceIdentity;

/// On-disk transcript metadata format.
pub(crate) const TRANSCRIPT_SCHEMA_VERSION: u32 = 1;
/// Rotate the active journal once it reaches this size. Old segments remain.
pub(crate) const DEFAULT_TRANSCRIPT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
const META_FILE: &str = "meta.json";
const JOURNAL_FILE: &str = "transcript.jsonl";
const SEGMENT_PREFIX: &str = "segment-";
const TRANSCRIPTS_DIR: &str = "transcripts";

/// One session's durable metadata. Metadata never contains transcript bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionArchiveMeta {
    pub(crate) schema_version: u32,
    /// Canonical workspace identity (`sha256:<hex>`).
    pub(crate) workspace_identity: String,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) session_name: Option<String>,
    #[serde(default)]
    pub(crate) archived: bool,
    /// Unix seconds; preserved across saves.
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) archived_at: Option<u64>,
    /// Items in the reconstructed transcript after the last save.
    #[serde(default)]
    pub(crate) items: usize,
    /// Next rotated segment number.
    #[serde(default)]
    pub(crate) next_segment: u32,
}

/// Inputs for one archive save.
#[derive(Debug, Clone)]
pub(crate) struct SessionArchiveRecord {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) session_name: Option<String>,
    pub(crate) archived: bool,
}

impl SessionArchiveRecord {
    pub(crate) fn new(
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        session_name: Option<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            session_name,
            archived: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum JournalRecord {
    Snapshot { items: Vec<TranscriptItem> },
    Set { index: usize, item: TranscriptItem },
    Append { items: Vec<TranscriptItem> },
    Truncate { len: usize },
}

/// Workspace-scoped transcript journal rooted in the user data directory.
#[derive(Debug)]
pub(crate) struct WorkspaceSessionArchive {
    root: PathBuf,
    workspace_identity: WorkspaceIdentity,
    rotate_bytes: u64,
    /// Item fingerprints from the last saved transcript, by session id.
    baselines: RefCell<HashMap<String, Vec<[u8; 32]>>>,
}

impl WorkspaceSessionArchive {
    /// Opens the archive for one workspace under `root`
    /// (`$XDG_DATA_HOME/ee/agent-sessions`).
    pub(crate) fn open(
        root: PathBuf,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<Self, String> {
        Self::open_with_rotation(root, workspace_identity, DEFAULT_TRANSCRIPT_ROTATE_BYTES)
    }

    fn open_with_rotation(
        root: PathBuf,
        workspace_identity: WorkspaceIdentity,
        rotate_bytes: u64,
    ) -> Result<Self, String> {
        if rotate_bytes == 0 {
            return Err(String::from("transcript rotation threshold must be greater than zero"));
        }
        let workspace_dir = workspace_dir(&root, workspace_identity);
        ensure_private_dir(&workspace_dir)?;
        Ok(Self { root, workspace_identity, rotate_bytes, baselines: RefCell::new(HashMap::new()) })
    }

    /// Default archive root. Kept here so the CLI prune command and the editor
    /// resolve the same path.
    pub(crate) fn default_root() -> Option<PathBuf> {
        dirs::data_local_dir().map(|directory| directory.join("ee").join("agent-sessions"))
    }

    /// Directory for one session. The session id is hashed, so provider ids can
    /// never escape the archive root.
    pub(crate) fn session_dir(&self, session_id: &str) -> Result<PathBuf, String> {
        session_dir(&self.root, self.workspace_identity, session_id)
    }

    /// Saves the full transcript and metadata. Missing baselines produce a
    /// `Snapshot`; later saves append compact deltas.
    pub(crate) fn save(
        &self,
        record: &SessionArchiveRecord,
        transcript: &[TranscriptItem],
    ) -> Result<(), String> {
        let dir = self.session_dir(&record.session_id)?;
        ensure_private_dir(&dir)?;
        let now = now_secs();
        let existing = read_meta(&dir)?;
        let created_at = existing.as_ref().map_or(now, |meta| meta.created_at);
        let archived_at = if record.archived {
            existing.as_ref().and_then(|meta| meta.archived_at).or(Some(now))
        } else {
            None
        };
        let mut meta = SessionArchiveMeta {
            schema_version: TRANSCRIPT_SCHEMA_VERSION,
            workspace_identity: self.workspace_identity.as_string(),
            agent_id: record.agent_id.clone(),
            session_id: record.session_id.clone(),
            session_name: record.session_name.clone(),
            archived: record.archived,
            created_at,
            updated_at: now,
            archived_at,
            items: transcript.len(),
            next_segment: existing.as_ref().map_or(0, |meta| meta.next_segment),
        };

        let records = self.delta_records(&record.session_id, transcript)?;
        if !records.is_empty() {
            meta.next_segment = self.append_records(&dir, meta.next_segment, &records)?;
        }
        write_meta(&dir, &meta)?;
        self.baselines.borrow_mut().insert(record.session_id.clone(), fingerprints(transcript)?);
        Ok(())
    }

    /// Loads and replays every journal segment for `session_id`.
    pub(crate) fn load_transcript(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<TranscriptItem>>, String> {
        let dir = self.session_dir(session_id)?;
        let segments = segment_paths(&dir)?;
        if segments.is_empty() {
            return Ok(None);
        }
        let mut items: Vec<TranscriptItem> = Vec::new();
        let last = segments.len().saturating_sub(1);
        for (position, path) in segments.iter().enumerate() {
            replay_file(path, position == last, &mut items)?;
        }
        self.note_loaded(session_id, &items)?;
        Ok(Some(items))
    }

    /// Seeds the fingerprint baseline after a transcript was loaded from
    /// somewhere else (legacy JSON fallback or a fresh restore).
    pub(crate) fn note_loaded(
        &self,
        session_id: &str,
        transcript: &[TranscriptItem],
    ) -> Result<(), String> {
        self.baselines.borrow_mut().insert(session_id.to_string(), fingerprints(transcript)?);
        Ok(())
    }

    /// Deletes one session's journal directory. Used only by explicit delete.
    pub(crate) fn delete_session(&self, session_id: &str) -> Result<bool, String> {
        let dir = self.session_dir(session_id)?;
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("failed to inspect {}: {error}", dir.display())),
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                Err(format!("refusing to delete non-directory archive path {}", dir.display()))
            }
            Ok(_) => {
                fs::remove_dir_all(&dir)
                    .map_err(|error| format!("failed to delete {}: {error}", dir.display()))?;
                self.baselines.borrow_mut().remove(session_id);
                Ok(true)
            }
        }
    }

    fn delta_records(
        &self,
        session_id: &str,
        transcript: &[TranscriptItem],
    ) -> Result<Vec<JournalRecord>, String> {
        let current = fingerprints(transcript)?;
        let baseline = self.baselines.borrow().get(session_id).cloned();
        let Some(previous) = baseline else {
            if transcript.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![JournalRecord::Snapshot { items: transcript.to_vec() }]);
        };
        let mut records = Vec::new();
        if current.len() < previous.len() {
            records.push(JournalRecord::Truncate { len: current.len() });
        }
        let shared = current.len().min(previous.len());
        let mut changed = false;
        for index in 0..shared {
            if current[index] != previous[index] {
                records.push(JournalRecord::Set { index, item: transcript[index].clone() });
                changed = true;
            }
        }
        if current.len() > previous.len() {
            let tail = transcript[shared..].to_vec();
            if !tail.is_empty() {
                records.push(JournalRecord::Append { items: tail });
            }
        } else if !changed && current.len() == previous.len() {
            return Ok(Vec::new());
        }
        Ok(records)
    }

    fn append_records(
        &self,
        dir: &Path,
        mut next_segment: u32,
        records: &[JournalRecord],
    ) -> Result<u32, String> {
        let active = dir.join(JOURNAL_FILE);
        // A crash can leave a partial JSON record in the active journal. Remove
        // it before appending: otherwise the next record would be concatenated
        // onto the torn line and turn a tolerated tail into permanent
        // mid-stream corruption on the following replay.
        repair_torn_tail(&active)?;
        let active_size = fs::symlink_metadata(&active).map(|meta| meta.len()).unwrap_or(0);
        if active_size >= self.rotate_bytes {
            next_segment = next_segment.max(next_segment_number(dir)?);
            let rotated = dir.join(format!("{SEGMENT_PREFIX}{next_segment:08}.jsonl"));
            fs::rename(&active, &rotated).map_err(|error| {
                format!("failed to rotate {} to {}: {error}", active.display(), rotated.display())
            })?;
            sync_dir(dir);
            next_segment = next_segment.saturating_add(1);
        }
        let mut payload = Vec::new();
        for record in records {
            serde_json::to_writer(&mut payload, record)
                .map_err(|error| format!("failed to serialize transcript record: {error}"))?;
            payload.push(b'\n');
        }
        append_private_file(&active, &payload)?;
        Ok(next_segment)
    }
}

/// One prune pass summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PruneReport {
    pub(crate) transcripts: usize,
    pub(crate) session_logs: usize,
    pub(crate) bytes: u64,
    pub(crate) skipped_live: usize,
}

/// Prunes transcript journals and provider session logs older than
/// `cutoff_secs` (or every entry when `all`). Live session ids are never
/// removed. `dry_run` reports the same candidates without deleting anything.
pub(crate) fn prune_sessions(
    root: &Path,
    cutoff_secs: u64,
    all: bool,
    live: &std::collections::BTreeSet<String>,
    dry_run: bool,
) -> Result<PruneReport, String> {
    let mut report = PruneReport::default();
    let transactions = root.join("transcripts");
    if let Ok(workspaces) = fs::read_dir(&transactions) {
        for workspace in workspaces {
            let workspace = workspace
                .map_err(|error| format!("failed to read transcript workspace: {error}"))?;
            if !is_real_dir(&workspace.path())? {
                continue;
            }
            for session in fs::read_dir(workspace.path())
                .map_err(|error| format!("failed to read transcript workspace: {error}"))?
            {
                let session =
                    session.map_err(|error| format!("failed to read transcript entry: {error}"))?;
                let path = session.path();
                if !is_real_dir(&path)? {
                    continue;
                }
                let meta = match read_meta(&path) {
                    Ok(meta) => meta,
                    // Never turn a corrupt manifest into a deletion decision.
                    Err(_) => continue,
                };
                let session_id = meta.as_ref().map(|meta| meta.session_id.as_str());
                if session_id.is_some_and(|id| live.contains(id)) {
                    report.skipped_live += 1;
                    continue;
                }
                let updated =
                    meta.as_ref().map_or_else(|| modified_secs(&path), |meta| meta.updated_at);
                if !all && updated >= cutoff_secs {
                    continue;
                }
                let bytes = path_size(&path);
                if !dry_run {
                    delete_tree(&path)?;
                }
                report.transcripts += 1;
                report.bytes = report.bytes.saturating_add(bytes);
            }
        }
    }

    let log_root = root.join("session-log");
    if let Ok(entries) = fs::read_dir(&log_root) {
        let protected = live
            .iter()
            .map(|id| log_root.join(sanitize_session_component(id)))
            .collect::<std::collections::BTreeSet<_>>();
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read session log entry: {error}"))?;
            let path = entry.path();
            if !is_real_dir(&path)? || protected.contains(&path) {
                if protected.contains(&path) {
                    report.skipped_live += 1;
                }
                continue;
            }
            if !all && modified_secs(&path) >= cutoff_secs {
                continue;
            }
            let bytes = path_size(&path);
            if !dry_run {
                delete_tree(&path)?;
            }
            report.session_logs += 1;
            report.bytes = report.bytes.saturating_add(bytes);
        }
    }
    Ok(report)
}

fn sanitize_session_component(value: &str) -> String {
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

fn is_real_dir(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_dir() && !meta.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
    }
}

/// Truncates a partial final record left by an interrupted append. The
/// function is a no-op for a missing file, an empty file, or a file that ends
/// on a record boundary.
fn repair_torn_tail(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to inspect {}: {error}", path.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("refusing non-file transcript journal {}", path.display()));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    let keep = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |position| position + 1);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    file.set_len(keep as u64)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to repair {}: {error}", path.display()))?;
    Ok(())
}

fn modified_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

fn path_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| path_size(&entry.path()))
        .fold(0, u64::saturating_add)
}

fn delete_tree(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("refusing to delete symlink {}", path.display()));
    }
    if meta.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| format!("failed to delete {}: {error}", path.display()))
    } else {
        fs::remove_file(path)
            .map_err(|error| format!("failed to delete {}: {error}", path.display()))
    }
}

fn workspace_dir(root: &Path, identity: WorkspaceIdentity) -> PathBuf {
    root.join(TRANSCRIPTS_DIR).join(identity.hex())
}

fn session_dir(
    root: &Path,
    identity: WorkspaceIdentity,
    session_id: &str,
) -> Result<PathBuf, String> {
    if session_id.is_empty() || session_id.len() > 512 {
        return Err(String::from("agent session id is empty or too long"));
    }
    if session_id.chars().any(char::is_control) {
        return Err(String::from("agent session id contains control characters"));
    }
    let digest = Sha256::digest(session_id.as_bytes());
    Ok(workspace_dir(root, identity).join(hex(&digest)))
}

fn segment_paths(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(format!("failed to read {}: {error}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read transcript entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(SEGMENT_PREFIX) && name.ends_with(".jsonl") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let active = dir.join(JOURNAL_FILE);
    if active.exists() {
        paths.push(active);
    }
    Ok(paths)
}

fn next_segment_number(dir: &Path) -> Result<u32, String> {
    let mut next = 0u32;
    for path in segment_paths(dir)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(number) = name
            .strip_prefix(SEGMENT_PREFIX)
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        next = next.max(number.saturating_add(1));
    }
    Ok(next)
}

fn replay_file(
    path: &Path,
    allow_torn_tail: bool,
    items: &mut Vec<TranscriptItem>,
) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read transcript {}: {error}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let last_non_empty = lines.iter().rposition(|line| !line.trim().is_empty());
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<JournalRecord>(line) {
            Ok(record) => record,
            Err(_) if allow_torn_tail && Some(index) == last_non_empty => break,
            Err(error) => {
                return Err(format!(
                    "transcript journal {} line {} is malformed: {error}",
                    path.display(),
                    index + 1
                ));
            }
        };
        apply_record(record, items, path, index)?;
    }
    Ok(())
}

fn apply_record(
    record: JournalRecord,
    items: &mut Vec<TranscriptItem>,
    path: &Path,
    index: usize,
) -> Result<(), String> {
    let malformed = |reason: &str| {
        format!("transcript journal {} line {} is invalid: {reason}", path.display(), index + 1)
    };
    match record {
        JournalRecord::Snapshot { items: snapshot } => {
            *items = snapshot;
        }
        JournalRecord::Set { index: item_index, item } => match item_index.cmp(&items.len()) {
            std::cmp::Ordering::Less => items[item_index] = item,
            std::cmp::Ordering::Equal => items.push(item),
            std::cmp::Ordering::Greater => {
                return Err(malformed("set index leaves a gap in the transcript"));
            }
        },
        JournalRecord::Append { items: tail } => items.extend(tail),
        JournalRecord::Truncate { len } => {
            if len > items.len() {
                return Err(malformed("truncate length exceeds the transcript"));
            }
            items.truncate(len);
        }
    }
    Ok(())
}

fn read_meta(dir: &Path) -> Result<Option<SessionArchiveMeta>, String> {
    let path = dir.join(META_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let meta: SessionArchiveMeta = serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    if meta.schema_version != TRANSCRIPT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported transcript metadata version {} in {}",
            meta.schema_version,
            path.display()
        ));
    }
    Ok(Some(meta))
}

fn write_meta(dir: &Path, meta: &SessionArchiveMeta) -> Result<(), String> {
    let bytes = serde_json::to_vec(meta)
        .map_err(|error| format!("failed to serialize transcript metadata: {error}"))?;
    atomic_private_write(&dir.join(META_FILE), &bytes)
}

fn fingerprints(items: &[TranscriptItem]) -> Result<Vec<[u8; 32]>, String> {
    items
        .iter()
        .map(|item| {
            let bytes = serde_json::to_vec(item)
                .map_err(|error| format!("failed to fingerprint transcript item: {error}"))?;
            Ok(Sha256::digest(&bytes).into())
        })
        .collect()
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        }
        Err(error) => return Err(format!("failed to inspect {}: {error}", path.display())),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(format!("refusing non-directory archive path {}", path.display()));
        }
        Ok(_) => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("failed to restrict {}: {error}", path.display()))?;
    }
    Ok(())
}

fn append_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to append {}: {error}", path.display()))?;
    restrict_private_file(path);
    Ok(())
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir =
        path.parent().ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
    ensure_private_dir(dir)?;
    let temp = dir.join(format!(".transcript.tmp-{}-{}", std::process::id(), nonce()));
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| format!("failed to create {}: {error}", temp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("failed to write {}: {error}", temp.display()))?;
        drop(file);
        restrict_private_file(&temp);
        fs::rename(&temp, path)
            .map_err(|error| format!("failed to finalize {}: {error}", path.display()))?;
        sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn restrict_private_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = fs::File::open(path) {
        let _ = directory.sync_all();
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}

fn nonce() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent_pane::MessageRenderKind;

    fn open_archive(root: &Path, rotate_bytes: u64) -> WorkspaceSessionArchive {
        let identity =
            WorkspaceIdentity::from_canonical_root_bytes(root.as_os_str().as_encoded_bytes());
        WorkspaceSessionArchive::open_with_rotation(root.join("data"), identity, rotate_bytes)
            .expect("archive")
    }

    fn message(text: &str) -> TranscriptItem {
        TranscriptItem::Message {
            nick: String::from("agent"),
            text: text.to_string(),
            kind: MessageRenderKind::Assistant,
            message_id: None,
            response_group: None,
            at: SystemTime::UNIX_EPOCH,
        }
    }

    fn tool(status: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            id: String::from("tool-1"),
            title: String::from("Run tests"),
            status: status.to_string(),
            detail: String::from("detail"),
            response_group: 1,
            at: SystemTime::UNIX_EPOCH,
        }
    }

    fn record(session_id: &str) -> SessionArchiveRecord {
        SessionArchiveRecord::new("agent", session_id, Some(String::from("work")))
    }

    #[test]
    fn snapshots_and_deltas_replay_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        let mut items = vec![message("hello"), tool("running")];
        archive.save(&record("session-1"), &items).unwrap();

        items[1] = tool("completed");
        items.push(message("world"));
        archive.save(&record("session-1"), &items).unwrap();

        let restored = archive.load_transcript("session-1").unwrap().expect("transcript");
        assert_eq!(restored.len(), 3);
        assert!(matches!(
            &restored[1],
            TranscriptItem::ToolCall { status, .. } if status == "completed"
        ));
        assert!(matches!(&restored[2], TranscriptItem::Message { text, .. } if text == "world"));
    }

    #[test]
    fn rotation_keeps_every_item() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 256);
        let mut items =
            (0..1_600).map(|index| message(&format!("line {index}"))).collect::<Vec<_>>();
        archive.save(&record("session-1"), &items).unwrap();
        items.push(message("tail"));
        archive.save(&record("session-1"), &items).unwrap();

        let dir = archive.session_dir("session-1").unwrap();
        let segments = segment_paths(&dir).unwrap();
        assert!(segments.len() > 1, "journal must rotate: {segments:?}");
        let restored = archive.load_transcript("session-1").unwrap().expect("transcript");
        assert_eq!(restored.len(), 1_601);
        assert!(matches!(
            &restored[1_600],
            TranscriptItem::Message { text, .. } if text == "tail"
        ));
    }

    #[test]
    fn torn_tail_is_ignored_but_mid_stream_damage_fails() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("hello")]).unwrap();
        let dir = archive.session_dir("session-1").unwrap();
        let journal = dir.join(JOURNAL_FILE);
        append_private_file(&journal, b"{\"op\":\"set\"").unwrap();
        assert_eq!(archive.load_transcript("session-1").unwrap().unwrap().len(), 1);

        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(b"\nnot-json\n");
        fs::write(&journal, bytes).unwrap();
        assert!(archive.load_transcript("session-1").is_err());
    }

    #[test]
    fn torn_tail_is_repaired_before_the_next_append() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("hello")]).unwrap();
        let dir = archive.session_dir("session-1").unwrap();
        let journal = dir.join(JOURNAL_FILE);
        // Simulate a crash halfway through appending a delta record.
        append_private_file(&journal, b"{\"op\":\"append\",\"items\":[").unwrap();

        // A restarted archive tolerates the torn tail, then appends more.
        let reopened = open_archive(temp.path(), 1024 * 1024);
        assert_eq!(reopened.load_transcript("session-1").unwrap().unwrap().len(), 1);
        reopened.save(&record("session-1"), &[message("hello"), message("world")]).unwrap();

        let restarted_again = open_archive(temp.path(), 1024 * 1024);
        let restored = restarted_again.load_transcript("session-1").unwrap().expect("transcript");
        assert_eq!(restored.len(), 2, "torn tail must not poison later appends");
        assert!(matches!(&restored[1], TranscriptItem::Message { text, .. } if text == "world"));
    }

    #[test]
    fn session_ids_are_hashed_and_empty_ids_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        assert!(archive.session_dir("").is_err());
        let dir = archive.session_dir("../escape").unwrap();
        assert!(dir.starts_with(temp.path().join("data").join("transcripts")));
        assert!(!dir.to_string_lossy().contains("escape"));
    }

    #[test]
    fn delete_removes_only_the_requested_session() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("one")]).unwrap();
        archive.save(&record("session-2"), &[message("two")]).unwrap();
        assert!(archive.delete_session("session-1").unwrap());
        assert!(archive.load_transcript("session-1").unwrap().is_none());
        assert!(archive.load_transcript("session-2").unwrap().is_some());
        assert!(!archive.delete_session("session-1").unwrap());
    }

    fn make_old(archive: &WorkspaceSessionArchive, session_id: &str) {
        let dir = archive.session_dir(session_id).unwrap();
        let path = dir.join(META_FILE);
        let mut meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        meta["updated_at"] = serde_json::json!(0u64);
        fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
    }

    #[test]
    fn prune_removes_only_stale_and_never_live_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("one")]).unwrap();
        archive.save(&record("session-2"), &[message("two")]).unwrap();
        make_old(&archive, "session-1");
        make_old(&archive, "session-2");
        let mut live = std::collections::BTreeSet::new();
        live.insert(String::from("session-2"));

        let dry = prune_sessions(&archive.root, now_secs(), true, &live, true).unwrap();
        assert_eq!(dry.transcripts, 1);
        assert_eq!(dry.session_logs, 0);
        assert_eq!(dry.skipped_live, 1);
        assert!(dry.bytes > 0);
        assert!(archive.load_transcript("session-1").unwrap().is_some());

        let report = prune_sessions(&archive.root, now_secs(), true, &live, false).unwrap();
        assert_eq!(report.transcripts, 1);
        assert!(archive.load_transcript("session-1").unwrap().is_none());
        assert!(archive.load_transcript("session-2").unwrap().is_some());

        let again = prune_sessions(&archive.root, now_secs(), true, &live, false).unwrap();
        assert_eq!(again.transcripts, 0, "prune is idempotent");
    }

    #[test]
    fn cutoff_keeps_recent_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("one")]).unwrap();
        let live = std::collections::BTreeSet::new();
        let report = prune_sessions(&archive.root, 0, false, &live, false).unwrap();
        assert_eq!(report.transcripts, 0);
        assert!(archive.load_transcript("session-1").unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn archive_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let archive = open_archive(temp.path(), 1024 * 1024);
        archive.save(&record("session-1"), &[message("hello")]).unwrap();
        let dir = archive.session_dir("session-1").unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(dir.join(META_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(dir.join(JOURNAL_FILE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
