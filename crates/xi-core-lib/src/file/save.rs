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

//! Save execution: prepared-save request types, the atomic temp-file writer,
//! and the VLF streaming snapshot path.

use std::ffi::OsString;
use std::fs::{self, File, Permissions};
use std::io::{self, Write};
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use log::warn;
use xi_rope::Rope;

use super::*;

use crate::tabs::BufferId;
use crate::vlf::overlay::VlfSavePolicy;
use crate::vlf::save::{PreparedVlfSavePlan, SaveProgress, VlfSaveError, stream_save_snapshot};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SaveOptions {
    #[cfg(target_family = "unix")]
    permissions: Option<u32>,
}

impl SaveOptions {
    pub(crate) fn from_info(info: Option<&FileInfo>) -> Self {
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

#[derive(Debug, Clone)]
pub(crate) enum PreparedVlfSaveKind {
    ExistingSamePath,
    ExistingMove { prev_path: PathBuf },
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedVlfSave {
    pub(crate) buffer_id: BufferId,
    pub(crate) path: PathBuf,
    pub(crate) policy: VlfSavePolicy,
    pub(crate) kind: PreparedVlfSaveKind,
}

#[allow(unused)]
fn try_save(
    path: &Path,
    text: &Rope,
    encoding: CharacterEncoding,
    save_options: SaveOptions,
    should_continue: &mut dyn FnMut() -> bool,
    on_progress: &mut dyn FnMut(SaveProgress),
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
    let total_bytes = text.len() as u64
        + u64::from(matches!(encoding, CharacterEncoding::Utf8WithBom)) * UTF8_BOM.len() as u64;
    let mut bytes_written = 0u64;
    match encoding {
        CharacterEncoding::Utf8WithBom => {
            f.write_all(UTF8_BOM.as_bytes()).map_err(|e| FileError::Io(e, tmp_path.to_owned()))?;
            bytes_written += UTF8_BOM.len() as u64;
            on_progress(SaveProgress { bytes_written, total_bytes });
        }
        CharacterEncoding::Utf8 => (),
    }

    if !should_continue() {
        drop(f);
        let _ = fs::remove_file(tmp_path);
        return Err(cancelled_save_error(tmp_path));
    }

    let mut writer = ChunkedSaveWriter {
        inner: &mut f,
        should_continue,
        on_progress,
        bytes_written: &mut bytes_written,
        total_bytes,
    };
    text.write_to(&mut writer).map_err(|e| match e.kind() {
        io::ErrorKind::Interrupted => cancelled_save_error(tmp_path),
        _ => FileError::Io(e, tmp_path.to_owned()),
    })?;

    // Flush OS buffers and sync to storage before rename so that a crash
    // after the rename cannot leave the destination file with stale content.
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
    let mut ignore_progress = |_progress: SaveProgress| {};
    execute_prepared_rope_save_with_progress(request, text, should_continue, &mut ignore_progress)
}

pub(crate) fn execute_prepared_rope_save_with_progress(
    request: &PreparedRopeSave,
    text: &Rope,
    should_continue: &mut dyn FnMut() -> bool,
    on_progress: &mut dyn FnMut(SaveProgress),
) -> Result<(), FileError> {
    try_save(&request.path, text, request.encoding, request.options, should_continue, on_progress)
}

pub(crate) fn execute_prepared_vlf_save(
    request: &PreparedVlfSave,
    plan: &PreparedVlfSavePlan,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<(), FileError> {
    stream_save_snapshot(plan, &request.path, &request.policy, on_progress).map_err(|e| match e {
        VlfSaveError::Io(io_err, err_path) => FileError::Io(io_err, err_path),
        VlfSaveError::Cancelled => FileError::Io(
            io::Error::new(io::ErrorKind::Interrupted, "VLF save cancelled"),
            request.path.clone(),
        ),
        VlfSaveError::EditingNotEnabled => FileError::Io(
            io::Error::new(io::ErrorKind::InvalidInput, "VLF editing not enabled; nothing to save"),
            request.path.clone(),
        ),
        VlfSaveError::InvalidPolicy(reason) => {
            FileError::Io(io::Error::new(io::ErrorKind::InvalidInput, reason), request.path.clone())
        }
    })
}

struct ChunkedSaveWriter<'a, W> {
    inner: &'a mut W,
    should_continue: &'a mut dyn FnMut() -> bool,
    on_progress: &'a mut dyn FnMut(SaveProgress),
    bytes_written: &'a mut u64,
    total_bytes: u64,
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
        self.inner.write_all(buf)?;
        *self.bytes_written += buf.len() as u64;
        (self.on_progress)(SaveProgress {
            bytes_written: *self.bytes_written,
            total_bytes: self.total_bytes,
        });
        Ok(())
    }
}

fn cancelled_save_error(path: &Path) -> FileError {
    FileError::Io(io::Error::new(io::ErrorKind::Interrupted, "save cancelled"), path.to_owned())
}
