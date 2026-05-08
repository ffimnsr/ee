// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Interactions with the file system.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str;
use std::time::SystemTime;

use fs2::FileExt;
use log::warn;

use xi_rope::Rope;
use xi_rpc::RemoteErrorDetails;

use crate::line_ending::{LineEnding, LineEndingError};
use crate::open_policy::{FileLocation, ModeOverride, OpenDecision, OpenPolicy};
use crate::tabs::BufferId;
use crate::text_store::DocumentMode;
use crate::vlf::store::VlfStore;
use crate::whitespace::{Indentation, MixedIndentError};

#[cfg(feature = "notify")]
use crate::tabs::OPEN_FILE_EVENT_TOKEN;
#[cfg(feature = "notify")]
use crate::watcher::FileWatcher;
#[cfg(target_family = "unix")]
use std::{fs::Permissions, os::unix::fs::PermissionsExt};

const UTF8_BOM: &str = "\u{feff}";
const MAX_FORMATTING_PROBE_BYTES: usize = 65_536;

/// Tracks all state related to open files.
pub struct FileManager {
    open_files: HashMap<PathBuf, BufferId>,
    file_info: HashMap<BufferId, FileInfo>,
    /// Open-mode policy applied before every file load.
    open_policy: OpenPolicy,
    /// A monitor of filesystem events, for things like reloading changed files.
    #[cfg(feature = "notify")]
    watcher: FileWatcher,
}

pub struct FileInfo {
    pub encoding: CharacterEncoding,
    pub path: PathBuf,
    pub mod_time: Option<SystemTime>,
    pub has_changed: bool,
    pub open_analysis: FileOpenAnalysis,
    #[cfg(target_family = "unix")]
    pub permissions: Option<u32>,
    /// Advisory exclusive lock held for the lifetime of this open buffer.
    /// Prevents a second editor instance from silently corrupting the file.
    _lock: Option<File>,
}

impl fmt::Debug for FileInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileInfo")
            .field("encoding", &self.encoding)
            .field("path", &self.path)
            .field("mod_time", &self.mod_time)
            .field("has_changed", &self.has_changed)
            .field("open_analysis", &self.open_analysis)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampledIndentation {
    Tabs,
    Spaces(usize),
    Mixed,
    None,
}

impl From<Result<Option<Indentation>, MixedIndentError>> for SampledIndentation {
    fn from(value: Result<Option<Indentation>, MixedIndentError>) -> Self {
        match value {
            Ok(Some(Indentation::Tabs)) => Self::Tabs,
            Ok(Some(Indentation::Spaces(width))) => Self::Spaces(width),
            Ok(None) => Self::None,
            Err(_) => Self::Mixed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampledLineEnding {
    CrLf,
    Lf,
    Mixed,
    LegacyCr,
    None,
}

impl From<Result<Option<LineEnding>, LineEndingError>> for SampledLineEnding {
    fn from(value: Result<Option<LineEnding>, LineEndingError>) -> Self {
        match value {
            Ok(Some(LineEnding::CrLf)) => Self::CrLf,
            Ok(Some(LineEnding::Lf)) => Self::Lf,
            Ok(None) => Self::None,
            Err(LineEndingError::Mixed) => Self::Mixed,
            Err(LineEndingError::LegacyCr) => Self::LegacyCr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOpenAnalysis {
    pub indentation: SampledIndentation,
    pub line_ending: SampledLineEnding,
    pub line_ending_complete: bool,
}

impl Default for FileOpenAnalysis {
    fn default() -> Self {
        Self {
            indentation: SampledIndentation::None,
            line_ending: SampledLineEnding::None,
            line_ending_complete: true,
        }
    }
}

impl FileOpenAnalysis {
    fn from_bytes(bytes: &[u8], encoding: CharacterEncoding) -> Self {
        let Some(sample) = formatting_probe_str(bytes, encoding) else {
            return Self::default();
        };

        let sample_rope = Rope::from(sample);
        let indentation =
            SampledIndentation::from(Indentation::parse_bounded(&sample_rope, usize::MAX));
        let line_ending =
            SampledLineEnding::from(LineEnding::parse_bounded(&sample_rope, usize::MAX));
        let skipped_bom =
            usize::from(matches!(encoding, CharacterEncoding::Utf8WithBom)) * UTF8_BOM.len();
        let line_ending_complete =
            bytes.len().saturating_sub(skipped_bom) <= MAX_FORMATTING_PROBE_BYTES;

        Self { indentation, line_ending, line_ending_complete }
    }

    pub fn needs_line_ending_verification(self) -> bool {
        !self.line_ending_complete
    }
}

fn formatting_probe_str(bytes: &[u8], encoding: CharacterEncoding) -> Option<&str> {
    let bytes = match encoding {
        CharacterEncoding::Utf8WithBom if bytes.starts_with(UTF8_BOM.as_bytes()) => {
            &bytes[UTF8_BOM.len()..]
        }
        _ => bytes,
    };

    let probe = &bytes[..bytes.len().min(MAX_FORMATTING_PROBE_BYTES)];
    match str::from_utf8(probe) {
        Ok(text) => Some(text),
        Err(err) if err.valid_up_to() > 0 => str::from_utf8(&probe[..err.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

pub enum FileError {
    Io(io::Error, PathBuf),
    UnknownEncoding(PathBuf),
    HasChanged(PathBuf),
    /// The path contains non-UTF-8 bytes and cannot be used as an RPC string.
    NonUtf8Path(PathBuf),
    /// File size could not be determined; refusing to load to avoid memory exhaustion.
    MetadataUntrusted(PathBuf),
    /// File exceeds the hard-open confirmation threshold for its location.
    ///
    /// The caller must surface `reason` to the user.  If the user accepts, retry
    /// with [`FileManager::open_with_override`] passing the appropriate
    /// [`ModeOverride`].
    ConfirmationRequired {
        path: PathBuf,
        reason: &'static str,
        /// The mode that would be used after confirmation.
        mode: DocumentMode,
    },
}

/// Result of opening a file, distinguishing the document mode.
///
/// - `Rope` is returned for `Normal` and `ConstrainedNormal` files loaded fully
///   into memory via `try_load_file`.
/// - `Vlf` is returned for files above the VLF threshold; the caller must use
///   the [`VlfStore`] for all reads.  No `Rope` is ever constructed.
pub enum OpenResult {
    /// Normal / ConstrainedNormal mode: full file content as a `Rope`.
    Rope(Rope),
    /// VLF mode: paged file reader with bounded cache.  No full buffer.
    Vlf(Box<VlfStore>),
}

#[derive(Debug, Clone, Copy)]
pub enum CharacterEncoding {
    Utf8,
    Utf8WithBom,
}

impl FileManager {
    #[cfg(feature = "notify")]
    pub fn new(watcher: FileWatcher) -> Self {
        FileManager {
            open_files: HashMap::new(),
            file_info: HashMap::new(),
            open_policy: OpenPolicy::default(),
            watcher,
        }
    }

    #[cfg(not(feature = "notify"))]
    pub fn new() -> Self {
        FileManager {
            open_files: HashMap::new(),
            file_info: HashMap::new(),
            open_policy: OpenPolicy::default(),
        }
    }

    /// Replace the open policy used for subsequent [`open`] calls.
    ///
    /// [`open`]: FileManager::open
    pub fn set_open_policy(&mut self, policy: OpenPolicy) {
        self.open_policy = policy;
    }

    #[cfg(feature = "notify")]
    pub fn watcher(&mut self) -> &mut FileWatcher {
        &mut self.watcher
    }

    pub fn get_info(&self, id: BufferId) -> Option<&FileInfo> {
        self.file_info.get(&id)
    }

    pub fn get_editor(&self, path: &Path) -> Option<BufferId> {
        self.open_files.get(path).cloned()
    }

    /// Returns `true` if this file is open and has changed on disk.
    /// This state is stashed.
    pub fn check_file(&mut self, path: &Path, id: BufferId) -> bool {
        if let Some(info) = self.file_info.get_mut(&id) {
            let mod_t = get_mod_time(path);
            if mod_t != info.mod_time {
                info.has_changed = true
            }
            return info.has_changed;
        }
        false
    }

    /// Open a file using `Auto` mode selection (policy decides Normal / ConstrainedNormal / Vlf).
    pub fn open(&mut self, path: &Path, id: BufferId) -> Result<OpenResult, FileError> {
        self.open_with_override(path, id, FileLocation::Local, ModeOverride::Auto)
    }

    /// Open a file with an explicit mode override and location hint.
    ///
    /// Use this when the caller has already shown the user a confirmation
    /// dialog (i.e. after receiving [`FileError::ConfirmationRequired`]) and
    /// wants to proceed with the policy-suggested mode.
    pub fn open_with_override(
        &mut self,
        path: &Path,
        id: BufferId,
        location: FileLocation,
        mode_override: ModeOverride,
    ) -> Result<OpenResult, FileError> {
        // Reject non-UTF-8 paths early: they cannot be round-tripped over the
        // JSON-RPC layer, so any further operations on the buffer would fail.
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }
        if !path.exists() {
            return Ok(OpenResult::Rope(Rope::from("")));
        }

        // Stat the file for size *before* reading any bytes.
        // Fail-closed: if metadata is unavailable, refuse to proceed.
        // Note: we already returned early for non-existent paths above.
        let size_opt = fs::metadata(path).ok().map(|m| m.len());

        match self.open_policy.decide(size_opt, None, None, location, mode_override) {
            OpenDecision::Open(DocumentMode::Normal)
            | OpenDecision::Open(DocumentMode::ConstrainedNormal) => {
                // Normal rope load path — both Normal and ConstrainedNormal use
                // the existing read_to_end + Rope path for now.
            }
            OpenDecision::Open(DocumentMode::Vlf) => {
                // VLF files must never be loaded into a full Rope.
                // Open via VlfStore which uses bounded pread I/O; the file
                // is never read_to_end or converted to a Rope.
                let store = VlfStore::open(path).map_err(|e| FileError::Io(e, path.to_owned()))?;

                // Kick off background indexing immediately so line-count
                // estimates become available without blocking the first render.
                store.start_background_indexing();

                // Register file metadata so close/reload work correctly.
                let info = FileInfo {
                    encoding: CharacterEncoding::Utf8,
                    path: path.to_owned(),
                    mod_time: get_mod_time(path),
                    has_changed: false,
                    open_analysis: FileOpenAnalysis::default(),
                    #[cfg(target_family = "unix")]
                    permissions: get_permissions(path),
                    _lock: None, // VLF files are read-only; no write lock needed.
                };
                self.open_files.insert(path.to_owned(), id);
                if self.file_info.insert(id, info).is_none() {
                    #[cfg(feature = "notify")]
                    self.watcher.watch(path, false, OPEN_FILE_EVENT_TOKEN);
                }
                return Ok(OpenResult::Vlf(Box::new(store)));
            }
            OpenDecision::ConfirmationRequired { reason, mode } => {
                return Err(FileError::ConfirmationRequired {
                    path: path.to_owned(),
                    reason,
                    mode,
                });
            }
            OpenDecision::Reject { reason: _ } => {
                return Err(FileError::MetadataUntrusted(path.to_owned()));
            }
        }

        let (rope, info) = try_load_file(path)?;

        self.open_files.insert(path.to_owned(), id);
        if self.file_info.insert(id, info).is_none() {
            #[cfg(feature = "notify")]
            self.watcher.watch(path, false, OPEN_FILE_EVENT_TOKEN);
        }
        Ok(OpenResult::Rope(rope))
    }

    pub fn close(&mut self, id: BufferId) {
        if let Some(info) = self.file_info.remove(&id) {
            self.open_files.remove(&info.path);
            #[cfg(feature = "notify")]
            self.watcher.unwatch(&info.path, OPEN_FILE_EVENT_TOKEN);
        }
    }

    pub fn save(&mut self, path: &Path, text: &Rope, id: BufferId) -> Result<(), FileError> {
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }
        let is_existing = self.file_info.contains_key(&id);
        if is_existing { self.save_existing(path, text, id) } else { self.save_new(path, text, id) }
    }

    fn save_new(&mut self, path: &Path, text: &Rope, id: BufferId) -> Result<(), FileError> {
        try_save(path, text, CharacterEncoding::Utf8, self.get_info(id))?;
        // Acquire advisory lock on the newly-saved file.
        let lock = File::open(path).ok().and_then(|lf| match lf.try_lock_exclusive() {
            Ok(()) => Some(lf),
            Err(e) => {
                warn!("Could not lock newly saved file {:?}: {}", path, e);
                None
            }
        });
        let info = FileInfo {
            encoding: CharacterEncoding::Utf8,
            path: path.to_owned(),
            mod_time: get_mod_time(path),
            has_changed: false,
            open_analysis: FileOpenAnalysis::default(),
            #[cfg(target_family = "unix")]
            permissions: get_permissions(path),
            _lock: lock,
        };
        self.open_files.insert(path.to_owned(), id);
        self.file_info.insert(id, info);
        #[cfg(feature = "notify")]
        self.watcher.watch(path, false, OPEN_FILE_EVENT_TOKEN);
        Ok(())
    }

    fn save_existing(&mut self, path: &Path, text: &Rope, id: BufferId) -> Result<(), FileError> {
        let prev_path = self.file_info[&id].path.clone();
        if prev_path != path {
            self.save_new(path, text, id)?;
            self.open_files.remove(&prev_path);
            #[cfg(feature = "notify")]
            self.watcher.unwatch(&prev_path, OPEN_FILE_EVENT_TOKEN);
        } else if self.file_info[&id].has_changed {
            return Err(FileError::HasChanged(path.to_owned()));
        } else {
            let encoding = self.file_info[&id].encoding;
            try_save(path, text, encoding, self.get_info(id))?;
            if let Some(info) = self.file_info.get_mut(&id) {
                info.mod_time = get_mod_time(path);
            } else {
                return Err(FileError::Io(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("missing file metadata for buffer {:?}", id),
                    ),
                    path.to_owned(),
                ));
            }
        }
        Ok(())
    }
}

fn try_load_file<P>(path: P) -> Result<(Rope, FileInfo), FileError>
where
    P: AsRef<Path>,
{
    // Non-UTF-8 file contents are rejected with FileError::UnknownEncoding.
    // it's arguable that the rope crate should have file loading functionality
    let mut f =
        File::open(path.as_ref()).map_err(|e| FileError::Io(e, path.as_ref().to_owned()))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(|e| FileError::Io(e, path.as_ref().to_owned()))?;

    // Acquire an advisory exclusive lock so that a second editor instance
    // cannot open the same file for writing without first detecting the lock.
    // `try_lock_exclusive` is non-blocking; if another process holds the lock
    // we warn and proceed without the lock rather than refusing to open the file.
    let lock_file = File::open(path.as_ref()).ok();
    let lock = lock_file.and_then(|lf| match lf.try_lock_exclusive() {
        Ok(()) => Some(lf),
        Err(e) => {
            warn!(
                "Could not acquire advisory lock on {:?}: {}. \
                     Another editor instance may have the file open.",
                path.as_ref(),
                e
            );
            None
        }
    });

    let encoding = CharacterEncoding::guess(&bytes);
    let open_analysis = FileOpenAnalysis::from_bytes(&bytes, encoding);
    let rope = try_decode(bytes, encoding, path.as_ref())?;
    let info = FileInfo {
        encoding,
        mod_time: get_mod_time(&path),
        open_analysis,
        #[cfg(target_family = "unix")]
        permissions: get_permissions(&path),
        path: path.as_ref().to_owned(),
        has_changed: false,
        _lock: lock,
    };
    Ok((rope, info))
}

#[allow(unused)]
fn try_save(
    path: &Path,
    text: &Rope,
    encoding: CharacterEncoding,
    file_info: Option<&FileInfo>,
) -> Result<(), FileError> {
    let tmp_extension = path.extension().map_or_else(
        || OsString::from("swp"),
        |ext| {
            let mut ext = ext.to_os_string();
            ext.push(".swp");
            ext
        },
    );
    let tmp_path = &path.with_extension(tmp_extension);

    let mut f = File::create(tmp_path).map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
    match encoding {
        CharacterEncoding::Utf8WithBom => {
            f.write_all(UTF8_BOM.as_bytes()).map_err(|e| FileError::Io(e, tmp_path.to_owned()))?
        }
        CharacterEncoding::Utf8 => (),
    }

    for chunk in text.iter_chunks(..text.len()) {
        f.write_all(chunk.as_bytes()).map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
    }

    // Flush OS buffers and sync to storage before rename so that a crash
    // after the rename cannot leave the destination file with stale data.
    f.sync_all().map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
    drop(f);

    fs::rename(tmp_path, path).map_err(|e| FileError::Io(e, path.to_owned()))?;

    // Sync the parent directory entry so the rename itself is durable.
    #[cfg(target_family = "unix")]
    {
        if let Some(parent) = path.parent() {
            // Best-effort: ignore errors (some fs don't support dir fsync).
            let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
        }
    }

    #[cfg(target_family = "unix")]
    {
        if let Some(info) = file_info {
            fs::set_permissions(path, Permissions::from_mode(info.permissions.unwrap_or(0o644)))
                .unwrap_or_else(|e| {
                    warn!("Couldn't set permissions on file {} due to error {}", path.display(), e)
                });
        }
    }

    Ok(())
}

fn try_decode(bytes: Vec<u8>, encoding: CharacterEncoding, path: &Path) -> Result<Rope, FileError> {
    match encoding {
        CharacterEncoding::Utf8 => Ok(Rope::from(
            str::from_utf8(&bytes).map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?,
        )),
        CharacterEncoding::Utf8WithBom => {
            let s = String::from_utf8(bytes)
                .map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?;
            Ok(Rope::from(&s[UTF8_BOM.len()..]))
        }
    }
}

impl CharacterEncoding {
    fn guess(s: &[u8]) -> Self {
        if s.starts_with(UTF8_BOM.as_bytes()) {
            CharacterEncoding::Utf8WithBom
        } else {
            CharacterEncoding::Utf8
        }
    }
}

/// Returns the modification timestamp for the file at a given path,
/// if present.
fn get_mod_time<P: AsRef<Path>>(path: P) -> Option<SystemTime> {
    File::open(path).and_then(|f| f.metadata()).and_then(|meta| meta.modified()).ok()
}

/// Returns the file permissions for the file at a given path on UNIXy systems,
/// if present.
#[cfg(target_family = "unix")]
fn get_permissions<P: AsRef<Path>>(path: P) -> Option<u32> {
    File::open(path).and_then(|f| f.metadata()).map(|meta| meta.permissions().mode()).ok()
}

impl RemoteErrorDetails for FileError {
    fn remote_error_code(&self) -> i64 {
        match self {
            FileError::Io(_, _) => 5,
            FileError::UnknownEncoding(_) => 6,
            FileError::HasChanged(_) => 7,
            FileError::NonUtf8Path(_) => 8,
            FileError::MetadataUntrusted(_) => 9,
            FileError::ConfirmationRequired { .. } => 10,
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FileError::Io(e, p) => write!(f, "{}. File path: {}", e, p.display()),
            FileError::UnknownEncoding(p) => {
                write!(f, "Error decoding UTF-8 file contents: {}", p.display())
            }
            FileError::HasChanged(p) => write!(
                f,
                "File has changed on disk. \
                 Please save elsewhere and reload the file. File path: {}",
                p.display()
            ),
            FileError::NonUtf8Path(p) => {
                write!(f, "File path contains non-UTF-8 bytes and cannot be used: {}", p.display())
            }
            FileError::MetadataUntrusted(p) => write!(
                f,
                "File size could not be determined safely; refusing to load: {}",
                p.display()
            ),
            FileError::ConfirmationRequired { path, reason, .. } => {
                write!(f, "{} ({})", reason, path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use xi_rpc::RemoteError;

    use super::{
        CharacterEncoding, FileError, FileOpenAnalysis, SampledIndentation, SampledLineEnding,
    };

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn open_rejects_non_utf8_path() {
        use super::FileManager;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        use super::FileManager;

        let mut mgr = FileManager::new();
        // Construct a path with a raw non-UTF-8 byte sequence.
        let bad_bytes: &[u8] = b"/tmp/\xff\xfe_bad.txt";
        let bad_path = PathBuf::from(OsStr::from_bytes(bad_bytes));
        let result = mgr.open(&bad_path, crate::tabs::BufferId(99));
        assert!(
            matches!(result, Err(FileError::NonUtf8Path(_))),
            "expected NonUtf8Path error, got {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    #[test]
    fn file_error_converts_into_remote_error() {
        let err: RemoteError = FileError::UnknownEncoding(PathBuf::from("/tmp/demo.txt")).into();

        assert_eq!(
            err,
            RemoteError::custom(
                6,
                "Error decoding UTF-8 file contents: /tmp/demo.txt",
                None::<serde_json::Value>,
            )
        );
    }

    #[test]
    fn metadata_untrusted_has_correct_code() {
        let err = FileError::MetadataUntrusted(PathBuf::from("/tmp/big.bin"));
        use xi_rpc::RemoteErrorDetails;
        assert_eq!(err.remote_error_code(), 9);
        assert!(err.to_string().contains("refusing to load"));
    }

    #[test]
    fn confirmation_required_has_correct_code() {
        use crate::text_store::DocumentMode;
        use xi_rpc::RemoteErrorDetails;
        let err = FileError::ConfirmationRequired {
            path: PathBuf::from("/tmp/huge.bin"),
            reason: "file exceeds 1 GiB",
            mode: DocumentMode::Vlf,
        };
        assert_eq!(err.remote_error_code(), 10);
        assert!(err.to_string().contains("file exceeds 1 GiB"));
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn small_real_file_opens_normally() {
        use super::FileManager;
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello world").unwrap();
        let path = tmp.path();
        let mut mgr = FileManager::new();
        let rope = mgr.open(path, crate::tabs::BufferId(1)).unwrap();
        assert!(rope.len() > 0);
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn file_above_confirmation_threshold_requires_confirmation() {
        use super::FileManager;
        use crate::open_policy::{FileLocation, ModeOverride, OpenPolicy, OpenThresholds};
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Write a few bytes — we override the thresholds to be tiny.
        writeln!(tmp, "data").unwrap();

        let thresholds = OpenThresholds {
            normal_bytes: 1,
            normal_lines: 1,
            vlf_bytes: 2,
            confirm_local_bytes: 3, // 3-byte file triggers confirmation
            confirm_remote_bytes: 3,
            confirm_web_bytes: 3,
        };
        let mut mgr = FileManager::new();
        mgr.set_open_policy(OpenPolicy::new(thresholds));

        let result = mgr.open_with_override(
            tmp.path(),
            crate::tabs::BufferId(2),
            FileLocation::Local,
            ModeOverride::Auto,
        );
        assert!(
            matches!(result, Err(FileError::ConfirmationRequired { .. })),
            "expected ConfirmationRequired, got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    #[test]
    fn open_analysis_detects_small_complete_sample() {
        let bytes = b"  alpha\r\n  beta\r\n";
        let analysis = FileOpenAnalysis::from_bytes(bytes, CharacterEncoding::Utf8);

        assert_eq!(analysis.indentation, SampledIndentation::Spaces(2));
        assert_eq!(analysis.line_ending, SampledLineEnding::CrLf);
        assert!(analysis.line_ending_complete);
    }

    #[test]
    fn open_analysis_uses_head_sample_and_defers_line_ending_verification() {
        let head: String = (0..10_000).map(|_| "  item\n").collect();
        let tail = "tail\r\n";
        let bytes = format!("{head}{tail}").into_bytes();

        let analysis = FileOpenAnalysis::from_bytes(&bytes, CharacterEncoding::Utf8);

        assert_eq!(analysis.indentation, SampledIndentation::Spaces(2));
        assert_eq!(analysis.line_ending, SampledLineEnding::Lf);
        assert!(analysis.needs_line_ending_verification());
    }
}
