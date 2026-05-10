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

use xi_rope::{Rope, RopeBuilder};
use xi_rpc::RemoteErrorDetails;

use crate::line_ending::{LineEnding, LineEndingError};
use crate::open_policy::{FileLocation, ModeOverride, OpenDecision, OpenPolicy};
use crate::tabs::BufferId;
use crate::text_store::DocumentMode;
use crate::vlf::save::{SaveProgress, VlfSaveError};
use crate::vlf::store::VlfStore;
use crate::whitespace::{Indentation, MixedIndentError};

#[cfg(feature = "notify")]
use crate::tabs::OPEN_FILE_EVENT_TOKEN;
#[cfg(feature = "notify")]
use crate::watcher::FileWatcher;
#[cfg(target_family = "unix")]
use std::{fs::Permissions, os::unix::fs::PermissionsExt};

#[cfg(test)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(test)]
use std::cell::Cell;

const UTF8_BOM: &str = "\u{feff}";
const MAX_FORMATTING_PROBE_BYTES: usize = 65_536;

#[cfg(test)]
struct TrackingAlloc;

#[cfg(test)]
thread_local! {
    static TRACK_ALLOC_THRESHOLD: Cell<usize> = const { Cell::new(0) };
    static TRACK_LARGE_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
    static TRACK_LARGEST_ALLOC: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
#[global_allocator]
static GLOBAL_ALLOCATOR: TrackingAlloc = TrackingAlloc;

#[cfg(test)]
#[inline]
fn record_large_alloc(layout: Layout) {
    TRACK_ALLOC_THRESHOLD.with(|threshold| {
        let threshold = threshold.get();
        if threshold == 0 || layout.size() < threshold {
            return;
        }
        TRACK_LARGE_ALLOC_COUNT.with(|count| count.set(count.get() + 1));
        TRACK_LARGEST_ALLOC.with(|largest| largest.set(largest.get().max(layout.size())));
    });
}

#[cfg(test)]
unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_large_alloc(layout);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_large_alloc(layout);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_large_alloc(Layout::from_size_align(new_size, layout.align()).unwrap());
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
fn with_large_alloc_tracking<T>(threshold: usize, f: impl FnOnce() -> T) -> (T, usize, usize) {
    TRACK_ALLOC_THRESHOLD.with(|value| value.set(threshold));
    TRACK_LARGE_ALLOC_COUNT.with(|value| value.set(0));
    TRACK_LARGEST_ALLOC.with(|value| value.set(0));
    let result = f();
    let alloc_count = TRACK_LARGE_ALLOC_COUNT.with(|value| value.get());
    let largest_alloc = TRACK_LARGEST_ALLOC.with(|value| value.get());
    TRACK_ALLOC_THRESHOLD.with(|value| value.set(0));
    (result, alloc_count, largest_alloc)
}

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

fn sampled_line_count_hint(path: &Path, file_size_bytes: u64) -> Option<u64> {
    let sample_len =
        usize::try_from(file_size_bytes.min(MAX_FORMATTING_PROBE_BYTES as u64)).ok()?;
    let mut sample = vec![0; sample_len];
    let mut file = File::open(path).ok()?;
    let bytes_read = file.read(&mut sample).ok()?;
    sample.truncate(bytes_read);
    estimate_line_count_from_sample(&sample, file_size_bytes)
}

fn estimate_line_count_from_sample(sample: &[u8], file_size_bytes: u64) -> Option<u64> {
    if sample.is_empty() {
        return Some(0);
    }

    let newline_count = sample.iter().filter(|&&byte| byte == b'\n').count() as u64;
    let sample_len = sample.len() as u64;

    if newline_count == 0 {
        return (sample_len == file_size_bytes).then_some(1);
    }

    let mut estimated = newline_count.saturating_mul(file_size_bytes).div_ceil(sample_len);
    if sample_len == file_size_bytes && !sample.ends_with(b"\n") {
        estimated = estimated.saturating_add(1);
    }

    Some(estimated.max(newline_count))
}

#[derive(Debug)]
pub enum FileError {
    Io(io::Error, PathBuf),
    UnknownEncoding(PathBuf),
    HasChanged(PathBuf),
    /// The path contains non-UTF-8 bytes and cannot be used as an RPC string.
    NonUtf8Path(PathBuf),
    /// File size could not be determined; refusing to load to avoid memory exhaustion.
    MetadataUntrusted(PathBuf),
    /// File exceeds the full-memory confirmation threshold for its location.
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
    Rope { text: Rope, mode: DocumentMode },
    /// VLF mode: paged file reader with bounded cache.  No full buffer.
    Vlf(Box<VlfStore>),
}

#[derive(Debug, Clone, Copy)]
pub enum CharacterEncoding {
    Utf8,
    Utf8WithBom,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SaveOptions {
    #[cfg(target_family = "unix")]
    permissions: Option<u32>,
}

impl SaveOptions {
    fn from_info(info: Option<&FileInfo>) -> Self {
        Self {
            #[cfg(target_family = "unix")]
            permissions: info.and_then(|info| info.permissions),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PreparedRopeSaveKind {
    New,
    ExistingSamePath,
    ExistingMove { prev_path: PathBuf },
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedRopeSave {
    pub(crate) buffer_id: BufferId,
    pub(crate) path: PathBuf,
    pub(crate) encoding: CharacterEncoding,
    pub(crate) kind: PreparedRopeSaveKind,
    pub(crate) options: SaveOptions,
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
            return Ok(OpenResult::Rope { text: Rope::from(""), mode: DocumentMode::Normal });
        }

        // Stat the file for size *before* reading any bytes.
        // Fail-closed: if metadata is unavailable, refuse to proceed.
        // Note: we already returned early for non-existent paths above.
        let size_opt = fs::metadata(path).ok().map(|m| m.len());
        let line_count_hint = size_opt.and_then(|size| sampled_line_count_hint(path, size));

        let decision =
            self.open_policy.decide(size_opt, line_count_hint, None, location, mode_override);
        let rope_mode = match decision {
            OpenDecision::Open(mode @ (DocumentMode::Normal | DocumentMode::ConstrainedNormal)) => {
                // Rope backing for both Normal and ConstrainedNormal.
                //
                // **Evaluation (ISSUES.md – Item 2)**: A hybrid paged-rope or
                // lazy `TextStore` backing was considered for ConstrainedNormal
                // to reduce peak RSS on mid-size files (8–30 MiB range).
                //
                // Decision: keep full-Rope until the following preconditions hold:
                //   1. VlfStore-backed chunk-native save lands (saves currently
                //      require a contiguous Rope snapshot for the encoder).
                //   2. The CRDT edit engine (`xi-rope`) is decoupled enough that
                //      deltas can be produced without materialising the whole rope.
                //   3. Profiling shows ConstrainedNormal RAM is a real bottleneck
                //      on representative workloads (not yet evidenced).
                //
                // The `TextStore` abstraction already routes chunk reads through
                // the paged API for VLF; extending it to ConstrainedNormal only
                // makes sense once save semantics are chunk-native.  Until then
                // this path is identical to Normal.
                mode
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
        };

        let (rope, info) = try_load_file(path)?;

        self.open_files.insert(path.to_owned(), id);
        if self.file_info.insert(id, info).is_none() {
            #[cfg(feature = "notify")]
            self.watcher.watch(path, false, OPEN_FILE_EVENT_TOKEN);
        }
        Ok(OpenResult::Rope { text: rope, mode: rope_mode })
    }

    pub fn close(&mut self, id: BufferId) {
        if let Some(info) = self.file_info.remove(&id) {
            self.open_files.remove(&info.path);
            #[cfg(feature = "notify")]
            self.watcher.unwatch(&info.path, OPEN_FILE_EVENT_TOKEN);
        }
    }

    pub fn save(&mut self, path: &Path, text: &Rope, id: BufferId) -> Result<(), FileError> {
        let request = self.prepare_rope_save(path, id)?;
        let mut should_continue = || true;
        execute_prepared_rope_save(&request, text, &mut should_continue)?;
        self.finish_rope_save(&request)
    }

    pub(crate) fn prepare_rope_save(
        &self,
        path: &Path,
        id: BufferId,
    ) -> Result<PreparedRopeSave, FileError> {
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }

        match self.file_info.get(&id) {
            Some(info) if info.has_changed => Err(FileError::HasChanged(path.to_owned())),
            Some(info) if info.path == path => Ok(PreparedRopeSave {
                buffer_id: id,
                path: path.to_owned(),
                encoding: info.encoding,
                kind: PreparedRopeSaveKind::ExistingSamePath,
                options: SaveOptions::from_info(Some(info)),
            }),
            Some(info) => Ok(PreparedRopeSave {
                buffer_id: id,
                path: path.to_owned(),
                encoding: CharacterEncoding::Utf8,
                kind: PreparedRopeSaveKind::ExistingMove { prev_path: info.path.clone() },
                options: SaveOptions::from_info(Some(info)),
            }),
            None => Ok(PreparedRopeSave {
                buffer_id: id,
                path: path.to_owned(),
                encoding: CharacterEncoding::Utf8,
                kind: PreparedRopeSaveKind::New,
                options: SaveOptions::from_info(None),
            }),
        }
    }

    pub(crate) fn finish_rope_save(&mut self, request: &PreparedRopeSave) -> Result<(), FileError> {
        match &request.kind {
            PreparedRopeSaveKind::ExistingSamePath => {
                if let Some(info) = self.file_info.get_mut(&request.buffer_id) {
                    info.mod_time = get_mod_time(&request.path);
                    info.has_changed = false;
                }
            }
            PreparedRopeSaveKind::New | PreparedRopeSaveKind::ExistingMove { .. } => {
                let info = FileInfo {
                    encoding: request.encoding,
                    path: request.path.clone(),
                    mod_time: get_mod_time(&request.path),
                    has_changed: false,
                    open_analysis: FileOpenAnalysis::default(),
                    #[cfg(target_family = "unix")]
                    permissions: get_permissions(&request.path),
                    _lock: open_advisory_lock(&request.path),
                };
                self.open_files.insert(request.path.clone(), request.buffer_id);
                self.file_info.insert(request.buffer_id, info);
                #[cfg(feature = "notify")]
                self.watcher.watch(&request.path, false, OPEN_FILE_EVENT_TOKEN);

                if let PreparedRopeSaveKind::ExistingMove { prev_path } = &request.kind {
                    self.open_files.remove(prev_path);
                    #[cfg(feature = "notify")]
                    self.watcher.unwatch(prev_path, OPEN_FILE_EVENT_TOKEN);
                }
            }
        }

        Ok(())
    }

    /// Save a VLF document by streaming the overlay piece sequence through a
    /// temp file then atomically renaming over `path`.
    ///
    /// `on_progress` is called after each chunk is written.  Return `false`
    /// from the callback to cancel the save before the rename commit point.
    ///
    /// Returns `Err(FileError::Io)` wrapping a [`VlfSaveError`] when the
    /// overlay has not been enabled (editing was never activated) or when an
    /// I/O failure occurs.  For successful saves, file metadata stored in
    /// this [`FileManager`] is updated to reflect the new modification time.
    pub fn save_vlf(
        &mut self,
        path: &Path,
        store: &VlfStore,
        id: BufferId,
        on_progress: &mut dyn FnMut(SaveProgress) -> bool,
    ) -> Result<(), FileError> {
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }

        // Check for external modification before committing.
        if let Some(info) = self.file_info.get(&id) {
            if info.has_changed {
                return Err(FileError::HasChanged(path.to_owned()));
            }
        }

        let policy = store
            .suggested_save_policy()
            .unwrap_or(crate::vlf::overlay::VlfSavePolicy::TempFileRewrite { temp_dir: None });

        store.stream_save(path, &policy, on_progress).map_err(|e| match e {
            VlfSaveError::Io(io_err, err_path) => FileError::Io(io_err, err_path),
            VlfSaveError::Cancelled => FileError::Io(
                io::Error::new(io::ErrorKind::Interrupted, "VLF save cancelled"),
                path.to_owned(),
            ),
            VlfSaveError::EditingNotEnabled => FileError::Io(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "VLF editing not enabled; nothing to save",
                ),
                path.to_owned(),
            ),
            VlfSaveError::InvalidPolicy(reason) => {
                FileError::Io(io::Error::new(io::ErrorKind::InvalidInput, reason), path.to_owned())
            }
        })?;

        // Update stored file metadata to reflect the successful save.
        if let Some(info) = self.file_info.get_mut(&id) {
            info.mod_time = get_mod_time(path);
            info.has_changed = false;
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
    save_options: SaveOptions,
    should_continue: &mut dyn FnMut() -> bool,
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

    if !should_continue() {
        drop(f);
        let _ = fs::remove_file(tmp_path);
        return Err(cancelled_save_error(tmp_path));
    }

    let mut writer = ChunkedSaveWriter { inner: &mut f, should_continue };
    text.write_to(&mut writer).map_err(|e| match e.kind() {
        io::ErrorKind::Interrupted => cancelled_save_error(tmp_path),
        _ => FileError::Io(e, tmp_path.to_owned()),
    })?;

    // Flush OS buffers and sync to storage before rename so that a crash
    // after the rename cannot leave the destination file with stale data.
    f.sync_all().map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
    drop(f);

    if !should_continue() {
        let _ = fs::remove_file(tmp_path);
        return Err(cancelled_save_error(tmp_path));
    }

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
        fs::set_permissions(
            path,
            Permissions::from_mode(save_options.permissions.unwrap_or(0o644)),
        )
        .unwrap_or_else(|e| {
            warn!("Couldn't set permissions on file {} due to error {}", path.display(), e)
        });
    }

    Ok(())
}

pub(crate) fn execute_prepared_rope_save(
    request: &PreparedRopeSave,
    text: &Rope,
    should_continue: &mut dyn FnMut() -> bool,
) -> Result<(), FileError> {
    try_save(&request.path, text, request.encoding, request.options, should_continue)
}

struct ChunkedSaveWriter<'a, W> {
    inner: &'a mut W,
    should_continue: &'a mut dyn FnMut() -> bool,
}

impl<W: Write> Write for ChunkedSaveWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if !(self.should_continue)() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "save cancelled"));
        }
        self.inner.write_all(buf)
    }
}

fn try_decode(bytes: Vec<u8>, encoding: CharacterEncoding, path: &Path) -> Result<Rope, FileError> {
    let text = match encoding {
        CharacterEncoding::Utf8 => {
            str::from_utf8(&bytes).map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?
        }
        CharacterEncoding::Utf8WithBom => {
            let s =
                str::from_utf8(&bytes).map_err(|_e| FileError::UnknownEncoding(path.to_owned()))?;
            &s[UTF8_BOM.len()..]
        }
    };

    let mut builder = RopeBuilder::new();
    builder.push_str(text);
    Ok(builder.finish())
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

fn cancelled_save_error(path: &Path) -> FileError {
    FileError::Io(io::Error::new(io::ErrorKind::Interrupted, "save cancelled"), path.to_owned())
}

fn open_advisory_lock(path: &Path) -> Option<File> {
    File::open(path).ok().and_then(|lf| match lf.try_lock_exclusive() {
        Ok(()) => Some(lf),
        Err(e) => {
            warn!("Could not lock newly saved file {:?}: {}", path, e);
            None
        }
    })
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
            FileError::ConfirmationRequired { path, reason, mode } => {
                write!(f, "{}; selected mode: {:?}. File path: {}", reason, mode, path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use xi_rpc::RemoteError;

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    use super::OpenResult;
    use super::{
        CharacterEncoding, FileError, FileOpenAnalysis, SampledIndentation, SampledLineEnding,
    };
    use crate::text_store::DocumentMode;

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn open_rejects_non_utf8_path() {
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
        use xi_rpc::RemoteErrorDetails;
        let err = FileError::ConfirmationRequired {
            path: PathBuf::from("/tmp/huge.bin"),
            reason: "file is too large for a full-memory open; use VLF mode or confirm normal open",
            mode: DocumentMode::Normal,
        };
        assert_eq!(err.remote_error_code(), 10);
        assert!(err.to_string().contains("full-memory open"));
        assert!(err.to_string().contains("selected mode: Normal"));
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
        let opened = mgr.open(path, crate::tabs::BufferId(1)).unwrap();
        assert!(matches!(opened, OpenResult::Rope { .. }));
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn force_normal_file_above_confirmation_threshold_requires_confirmation() {
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
            vlf_lines: 2,
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
            ModeOverride::ForceNormal,
        );
        assert!(
            matches!(result, Err(FileError::ConfirmationRequired { .. })),
            "expected ConfirmationRequired, got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn strict_byte_thresholds_choose_rope_then_vlf_in_file_manager_open_flow() {
        use super::FileManager;
        use crate::open_policy::{FileLocation, ModeOverride, OpenPolicy, OpenThresholds};
        use std::fs::OpenOptions;

        let thresholds = OpenThresholds {
            normal_bytes: 8,
            normal_lines: 30,
            vlf_bytes: 30,
            vlf_lines: 300,
            confirm_local_bytes: 1_024,
            confirm_remote_bytes: 1_024,
            confirm_web_bytes: 1_024,
        };

        let exact_normal = tempfile::NamedTempFile::new().unwrap();
        OpenOptions::new().write(true).open(exact_normal.path()).unwrap().set_len(8).unwrap();

        let exact_vlf = tempfile::NamedTempFile::new().unwrap();
        OpenOptions::new().write(true).open(exact_vlf.path()).unwrap().set_len(30).unwrap();

        let mut mgr = FileManager::new();
        mgr.set_open_policy(OpenPolicy::new(thresholds));

        let constrained = mgr
            .open_with_override(
                exact_normal.path(),
                crate::tabs::BufferId(3),
                FileLocation::Local,
                ModeOverride::Auto,
            )
            .unwrap();
        assert!(matches!(
            constrained,
            OpenResult::Rope { mode: DocumentMode::ConstrainedNormal, .. }
        ));

        let vlf = mgr
            .open_with_override(
                exact_vlf.path(),
                crate::tabs::BufferId(4),
                FileLocation::Local,
                ModeOverride::Auto,
            )
            .unwrap();
        assert!(matches!(vlf, OpenResult::Vlf(_)));
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn sampled_line_count_hint_can_force_vlf_for_small_high_loc_file() {
        use super::FileManager;
        use crate::open_policy::{FileLocation, ModeOverride, OpenPolicy, OpenThresholds};
        use std::io::Write;

        let thresholds = OpenThresholds {
            normal_bytes: 8 * 1024 * 1024,
            normal_lines: 30_000,
            vlf_bytes: 30 * 1024 * 1024,
            vlf_lines: 50_000,
            confirm_local_bytes: 1_024 * 1_024 * 1_024,
            confirm_remote_bytes: 10 * 1024 * 1024,
            confirm_web_bytes: 50 * 1024 * 1024,
        };

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..60_000 {
            writeln!(tmp, "x").unwrap();
        }

        let mut mgr = FileManager::new();
        mgr.set_open_policy(OpenPolicy::new(thresholds));

        let result = mgr
            .open_with_override(
                tmp.path(),
                crate::tabs::BufferId(5),
                FileLocation::Local,
                ModeOverride::Auto,
            )
            .unwrap();

        assert!(matches!(result, OpenResult::Vlf(_)));
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

    #[test]
    fn try_decode_large_bom_text_builds_multi_leaf_rope() {
        let text = format!("{}{}{}", "a".repeat(1500), "\r\n", "🙂é".repeat(400));
        let bytes = format!("{}{text}", super::UTF8_BOM).into_bytes();

        let rope = super::try_decode(bytes, CharacterEncoding::Utf8WithBom, Path::new("/tmp/demo"))
            .unwrap();

        assert_eq!(String::from(&rope), text);
        assert!(rope.iter_chunks(..).count() >= 2);
    }

    #[test]
    fn try_decode_does_not_allocate_full_intermediate_string() {
        let text = format!("{}{}{}", "line\r\n".repeat(4096), "🙂é", "tail\n".repeat(1024));
        let bytes = format!("{}{text}", super::UTF8_BOM).into_bytes();
        let threshold = text.len();

        let (rope, large_alloc_count, largest_alloc) =
            super::with_large_alloc_tracking(threshold, || {
                super::try_decode(bytes, CharacterEncoding::Utf8WithBom, Path::new("/tmp/demo"))
                    .unwrap()
            });

        assert_eq!(String::from(&rope), text);
        assert_eq!(large_alloc_count, 0, "unexpected >=full-buffer allocation: {largest_alloc}");
        assert_eq!(largest_alloc, 0);
    }

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn open_large_normal_file_uses_multi_leaf_rope() {
        use super::{FileManager, OpenResult};
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "{}{}{}", "a".repeat(1500), "\n", "b".repeat(1300)).unwrap();

        let mut mgr = FileManager::new();
        let opened = mgr.open(tmp.path(), crate::tabs::BufferId(6)).unwrap();

        match opened {
            OpenResult::Rope { text, mode: DocumentMode::Normal } => {
                assert!(text.iter_chunks(..).count() >= 2);
            }
            other => panic!("expected normal rope open, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn line_count_estimate_scales_head_sample_to_full_file() {
        let sample = b"x\nx\nx\n";
        let estimated = super::estimate_line_count_from_sample(sample, 12).unwrap();
        assert_eq!(estimated, 6);
    }

    /// Contract test: `ConstrainedNormal` files open with Rope backing.
    ///
    /// Documents the current evaluation decision from ISSUES.md Item 2:
    /// ConstrainedNormal keeps full-Rope until chunk-native save lands.
    /// `OpenResult::Rope` is the discriminant that proves this contract.
    ///
    /// Gated to `not(feature = "notify")` because `FileManager::new()` requires
    /// a `FileWatcher` argument when the `notify` feature is enabled; this
    /// mirrors the guard used by all other `file::tests` that create a manager.
    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn constrained_normal_uses_rope_backing() {
        use super::{FileManager, OpenResult};
        use crate::open_policy::{FileLocation, ModeOverride, OpenPolicy, OpenThresholds};
        use crate::text_store::DocumentMode;

        // Thresholds: ConstrainedNormal range is [normal_bytes, vlf_bytes).
        let thresholds = OpenThresholds {
            normal_bytes: 0,             // everything ≥ 0 bytes is at least Normal
            normal_lines: 0,             // unused for this test
            vlf_bytes: 64 * 1024 * 1024, // well above our temp file
            vlf_lines: 1_000_000,
            confirm_local_bytes: 1024 * 1024 * 1024,
            confirm_remote_bytes: 64 * 1024 * 1024,
            confirm_web_bytes: 64 * 1024 * 1024,
        };

        // Create a non-empty temp file so the policy has a real size to check.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello\nworld\n").unwrap();

        let mut mgr = FileManager::new();
        mgr.set_open_policy(OpenPolicy::new(thresholds));

        let result = mgr
            .open_with_override(
                tmp.path(),
                crate::tabs::BufferId(99),
                FileLocation::Local,
                ModeOverride::Auto,
            )
            .unwrap();

        // Key assertion: ConstrainedNormal (or Normal) returns OpenResult::Rope,
        // NOT OpenResult::Vlf.  This is the Rope-backing contract.
        assert!(
            matches!(
                result,
                OpenResult::Rope {
                    mode: DocumentMode::Normal | DocumentMode::ConstrainedNormal,
                    ..
                }
            ),
            "expected Rope-backed result for ConstrainedNormal"
        );
    }
}
