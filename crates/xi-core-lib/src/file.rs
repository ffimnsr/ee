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
use xi_rpc::RemoteError;

use crate::tabs::BufferId;

#[cfg(feature = "notify")]
use crate::tabs::OPEN_FILE_EVENT_TOKEN;
#[cfg(feature = "notify")]
use crate::watcher::FileWatcher;
#[cfg(target_family = "unix")]
use std::{fs::Permissions, os::unix::fs::PermissionsExt};

const UTF8_BOM: &str = "\u{feff}";

/// Tracks all state related to open files.
pub struct FileManager {
    open_files: HashMap<PathBuf, BufferId>,
    file_info: HashMap<BufferId, FileInfo>,
    /// A monitor of filesystem events, for things like reloading changed files.
    #[cfg(feature = "notify")]
    watcher: FileWatcher,
}

pub struct FileInfo {
    pub encoding: CharacterEncoding,
    pub path: PathBuf,
    pub mod_time: Option<SystemTime>,
    pub has_changed: bool,
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
            .finish_non_exhaustive()
    }
}

pub enum FileError {
    Io(io::Error, PathBuf),
    UnknownEncoding(PathBuf),
    HasChanged(PathBuf),
    /// The path contains non-UTF-8 bytes and cannot be used as an RPC string.
    NonUtf8Path(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub enum CharacterEncoding {
    Utf8,
    Utf8WithBom,
}

impl FileManager {
    #[cfg(feature = "notify")]
    pub fn new(watcher: FileWatcher) -> Self {
        FileManager { open_files: HashMap::new(), file_info: HashMap::new(), watcher }
    }

    #[cfg(not(feature = "notify"))]
    pub fn new() -> Self {
        FileManager { open_files: HashMap::new(), file_info: HashMap::new() }
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

    pub fn open(&mut self, path: &Path, id: BufferId) -> Result<Rope, FileError> {
        // Reject non-UTF-8 paths early: they cannot be round-tripped over the
        // JSON-RPC layer, so any further operations on the buffer would fail.
        if path.to_str().is_none() {
            return Err(FileError::NonUtf8Path(path.to_owned()));
        }
        if !path.exists() {
            return Ok(Rope::from(""));
        }

        let (rope, info) = try_load_file(path)?;

        self.open_files.insert(path.to_owned(), id);
        if self.file_info.insert(id, info).is_none() {
            #[cfg(feature = "notify")]
            self.watcher.watch(path, false, OPEN_FILE_EVENT_TOKEN);
        }
        Ok(rope)
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
        let lock = File::open(path).ok().and_then(|lf| {
            match lf.try_lock_exclusive() {
                Ok(()) => Some(lf),
                Err(e) => {
                    warn!("Could not lock newly saved file {:?}: {}", path, e);
                    None
                }
            }
        });
        let info = FileInfo {
            encoding: CharacterEncoding::Utf8,
            path: path.to_owned(),
            mod_time: get_mod_time(path),
            has_changed: false,
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
    let lock = lock_file.and_then(|lf| {
        match lf.try_lock_exclusive() {
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
        }
    });

    let encoding = CharacterEncoding::guess(&bytes);
    let rope = try_decode(bytes, encoding, path.as_ref())?;
    let info = FileInfo {
        encoding,
        mod_time: get_mod_time(&path),
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

    let mut f =
        File::create(tmp_path).map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
    match encoding {
        CharacterEncoding::Utf8WithBom => {
            f.write_all(UTF8_BOM.as_bytes())
                .map_err(|e| FileError::Io(e, tmp_path.to_owned()))?
        }
        CharacterEncoding::Utf8 => (),
    }

    for chunk in text.iter_chunks(..text.len()) {
        f.write_all(chunk.as_bytes())
            .map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
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
            let _ = std::fs::File::open(parent)
                .and_then(|d| d.sync_all());
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

impl From<FileError> for RemoteError {
    fn from(src: FileError) -> RemoteError {
        let code = src.error_code();
        let message = src.to_string();
        RemoteError::custom(code, message, None)
    }
}

impl FileError {
    fn error_code(&self) -> i64 {
        match self {
            FileError::Io(_, _) => 5,
            FileError::UnknownEncoding(_) => 6,
            FileError::HasChanged(_) => 7,
            FileError::NonUtf8Path(_) => 8,
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
            FileError::NonUtf8Path(p) => write!(
                f,
                "File path contains non-UTF-8 bytes and cannot be used: {}",
                p.display()
            ),
        }
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
}
