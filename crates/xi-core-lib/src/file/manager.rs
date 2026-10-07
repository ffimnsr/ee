// Copyright 2016 The xi-editor Authors.
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

//! The `FileManager`: open-mode policy application, buffer metadata, and the
//! open/close/save lifecycle.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use xi_rope::Rope;

use crate::open_policy::{FileLocation, ModeOverride, OpenDecision, OpenPolicy};
use crate::tabs::BufferId;
use crate::text_store::DocumentMode;
use crate::vlf::overlay::VlfSavePolicy;
use crate::vlf::save::{PreparedVlfSavePlan, SaveProgress, VlfSaveError, stream_save_snapshot};
use crate::vlf::store::VlfStore;

#[cfg(feature = "notify")]
use crate::tabs::OPEN_FILE_EVENT_TOKEN;
#[cfg(feature = "notify")]
use crate::watcher::FileWatcher;

use super::*;

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
    pub len: Option<u64>,
    pub has_changed: bool,
    pub open_analysis: FileOpenAnalysis,
    #[cfg(target_family = "unix")]
    pub permissions: Option<u32>,
    #[cfg(target_family = "unix")]
    pub(crate) change_cookie: Option<FileChangeCookie>,
    /// Advisory exclusive lock held for the lifetime of this open buffer.
    /// Prevents a second editor instance from silently corrupting the file.
    pub(crate) _lock: Option<File>,
}

impl fmt::Debug for FileInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("FileInfo");
        debug
            .field("encoding", &self.encoding)
            .field("path", &self.path)
            .field("mod_time", &self.mod_time)
            .field("len", &self.len)
            .field("has_changed", &self.has_changed)
            .field("open_analysis", &self.open_analysis);
        #[cfg(target_family = "unix")]
        debug.field("change_cookie", &self.change_cookie);
        debug.finish_non_exhaustive()
    }
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
            let len = get_file_len(path);
            #[cfg(target_family = "unix")]
            let change_cookie = get_change_cookie(path);
            if mod_t != info.mod_time || len != info.len || {
                #[cfg(target_family = "unix")]
                {
                    change_cookie != info.change_cookie
                }
                #[cfg(not(target_family = "unix"))]
                {
                    false
                }
            } {
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
        #[cfg(test)]
        let stat_started = std::time::Instant::now();
        let size_opt = std::fs::metadata(path).ok().map(|m| m.len());
        let line_count_hint = size_opt.and_then(|size| sampled_line_count_hint(path, size));

        let decision =
            self.open_policy.decide(size_opt, line_count_hint, None, location, mode_override);
        #[cfg(test)]
        crate::open_probe::record(crate::open_probe::OpenStage::Stat, stat_started.elapsed());
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
                #[cfg(test)]
                let vlf_started = std::time::Instant::now();
                let store = VlfStore::open(path).map_err(|e| FileError::Io(e, path.to_owned()))?;

                // Kick off background indexing immediately so line-count
                // estimates become available without blocking the first render.
                store.start_background_indexing();
                #[cfg(test)]
                crate::open_probe::record(
                    crate::open_probe::OpenStage::VlfOpen,
                    vlf_started.elapsed(),
                );

                // Register file metadata so close/reload work correctly.
                let info = FileInfo {
                    encoding: CharacterEncoding::Utf8,
                    path: path.to_owned(),
                    mod_time: get_mod_time(path),
                    len: get_file_len(path),
                    has_changed: false,
                    open_analysis: FileOpenAnalysis::default(),
                    #[cfg(target_family = "unix")]
                    permissions: get_permissions(path),
                    #[cfg(target_family = "unix")]
                    change_cookie: get_change_cookie(path),
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
                    info.len = get_file_len(&request.path);
                    info.has_changed = false;
                    #[cfg(target_family = "unix")]
                    {
                        info.change_cookie = get_change_cookie(&request.path);
                    }
                }
            }
            PreparedRopeSaveKind::New | PreparedRopeSaveKind::ExistingMove { .. } => {
                let info = FileInfo {
                    encoding: request.encoding,
                    path: request.path.clone(),
                    mod_time: get_mod_time(&request.path),
                    len: get_file_len(&request.path),
                    has_changed: false,
                    open_analysis: FileOpenAnalysis::default(),
                    #[cfg(target_family = "unix")]
                    permissions: get_permissions(&request.path),
                    #[cfg(target_family = "unix")]
                    change_cookie: get_change_cookie(&request.path),
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

    pub(crate) fn prepare_vlf_save(
        &self,
        path: &Path,
        id: BufferId,
        policy: VlfSavePolicy,
    ) -> Result<PreparedVlfSave, FileError> {
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }

        match self.file_info.get(&id) {
            Some(info) if info.has_changed => Err(FileError::HasChanged(path.to_owned())),
            Some(info) if info.path == path => Ok(PreparedVlfSave {
                buffer_id: id,
                path: path.to_owned(),
                policy,
                kind: PreparedVlfSaveKind::ExistingSamePath,
            }),
            Some(info) => Ok(PreparedVlfSave {
                buffer_id: id,
                path: path.to_owned(),
                policy,
                kind: PreparedVlfSaveKind::ExistingMove { prev_path: info.path.clone() },
            }),
            None => Err(FileError::Io(
                io::Error::new(io::ErrorKind::NotFound, "VLF save missing file metadata"),
                path.to_owned(),
            )),
        }
    }

    pub(crate) fn finish_vlf_save(&mut self, request: &PreparedVlfSave) -> Result<(), FileError> {
        match &request.kind {
            PreparedVlfSaveKind::ExistingSamePath => {
                if let Some(info) = self.file_info.get_mut(&request.buffer_id) {
                    info.mod_time = get_mod_time(&request.path);
                    info.len = get_file_len(&request.path);
                    info.has_changed = false;
                    #[cfg(target_family = "unix")]
                    {
                        info.change_cookie = get_change_cookie(&request.path);
                        if info._lock.is_none() {
                            info._lock = open_advisory_lock(&request.path);
                        }
                        info.permissions = get_permissions(&request.path);
                    }
                    return Ok(());
                }
            }
            PreparedVlfSaveKind::ExistingMove { prev_path } => {
                let previous_info = self.file_info.get(&request.buffer_id).ok_or_else(|| {
                    FileError::Io(
                        io::Error::new(io::ErrorKind::NotFound, "VLF save missing file metadata"),
                        request.path.clone(),
                    )
                })?;

                let info = FileInfo {
                    encoding: previous_info.encoding,
                    path: request.path.clone(),
                    mod_time: get_mod_time(&request.path),
                    len: get_file_len(&request.path),
                    has_changed: false,
                    open_analysis: previous_info.open_analysis,
                    #[cfg(target_family = "unix")]
                    permissions: get_permissions(&request.path),
                    #[cfg(target_family = "unix")]
                    change_cookie: get_change_cookie(&request.path),
                    _lock: open_advisory_lock(&request.path),
                };

                self.open_files.insert(request.path.clone(), request.buffer_id);
                self.file_info.insert(request.buffer_id, info);
                #[cfg(feature = "notify")]
                self.watcher.watch(&request.path, false, OPEN_FILE_EVENT_TOKEN);

                self.open_files.remove(prev_path);
                #[cfg(feature = "notify")]
                self.watcher.unwatch(prev_path, OPEN_FILE_EVENT_TOKEN);
                return Ok(());
            }
        }

        Err(FileError::Io(
            io::Error::new(io::ErrorKind::NotFound, "VLF save missing file metadata"),
            request.path.clone(),
        ))
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
        if matches!(policy, VlfSavePolicy::SaveAs(_)) {
            return Err(FileError::Io(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "VLF save requires explicit save-as destination",
                ),
                path.to_owned(),
            ));
        }
        let request = self.prepare_vlf_save(path, id, policy.clone())?;
        let plan = store.prepare_save_plan().map_err(|e| match e {
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

        stream_save_snapshot(
            &PreparedVlfSavePlan { source_path: plan.source_path, snapshot: plan.snapshot },
            path,
            &policy,
            on_progress,
        )
        .map_err(|e| match e {
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

        self.finish_vlf_save(&request)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn open_rejects_non_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

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

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn small_real_file_opens_normally() {
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

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn open_large_normal_file_uses_multi_leaf_rope() {
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
        use crate::open_policy::{FileLocation, ModeOverride, OpenPolicy, OpenThresholds};

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

    #[cfg(all(target_family = "unix", not(feature = "notify")))]
    #[test]
    fn check_file_detects_same_size_rewrite_when_mtime_is_restored() {
        use std::fs::{File, FileTimes};
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "alpha\n").unwrap();
        tmp.flush().unwrap();

        let path = tmp.path();
        let buffer_id = crate::tabs::BufferId(123);
        let mut mgr = FileManager::new();
        let opened = mgr.open(path, buffer_id).unwrap();
        assert!(matches!(opened, OpenResult::Rope { .. }));

        let original_mod_time = mgr.get_info(buffer_id).unwrap().mod_time.unwrap();

        std::fs::write(path, b"bravo\n").unwrap();
        let file = File::options().write(true).open(path).unwrap();
        file.set_times(FileTimes::new().set_modified(original_mod_time)).unwrap();

        assert!(mgr.check_file(path, buffer_id));
        assert!(mgr.get_info(buffer_id).unwrap().has_changed);
    }
}
