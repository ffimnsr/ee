// Copyright 2026 The xi-editor Authors.
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

//! Streaming save for VLF documents.
//!
//! # Strategy
//!
//! VLF documents are never fully materialised in RAM.  The save path must
//! stream the piece sequence — interleaving `Original` file ranges and
//! `Inserted` buffer slices — through a **temp file then atomic rename** so
//! that a crash before the rename leaves the original file intact.
//!
//! ## Durability policy
//!
//! 1. Write all piece bytes to a temp file in the same directory as the
//!    destination (or in an explicit `temp_dir`).
//! 2. Call `fsync` / `sync_all` on the temp file before rename.
//! 3. Rename the temp file over the destination atomically.
//! 4. On Unix, `fsync` the parent directory entry to make the rename durable.
//!
//! This matches the durability policy used by the normal/constrained rope save
//! path in `crates/xi-core-lib/src/file.rs::try_save`.
//!
//! ## Cancellation
//!
//! The `on_progress` callback receives a [`SaveProgress`] snapshot after each
//! chunk is written.  Returning `false` from the callback cancels the save:
//! the temp file is deleted and `Err(VlfSaveError::Cancelled)` is returned.
//!
//! **After the rename (commit point)** the file is already durably saved.
//! If the caller returns `false` from `on_progress` after that point (which
//! cannot happen because `on_progress` is never called after the rename),
//! the save is still considered successful.  Callers that lose the desire to
//! save should always check the return value of [`stream_save_pieces`] and
//! treat `Ok(())` as success regardless of any later state change.
//!
//! ## In-place overwrite and tail-shift
//!
//! Same-size edits can overwrite only the changed byte window.  Byte-length
//! changing edits can shift the unchanged file tail in-place, then overwrite
//! the changed window.  Both optimizations stage the changed window in a
//! bounded buffer first.  If the optimization cannot run safely, callers can
//! use the temp-file rewrite fallback.
//!
//! ## Normal / constrained mode
//!
//! Normal and constrained rope buffers use the existing `try_save` function in
//! `file.rs` (rope chunks → temp file → rename).  Sharing the durability policy
//! is sufficient for the first milestone; a unified streaming abstraction over
//! both rope chunks and VLF pieces can be extracted later if code duplication
//! becomes a maintenance problem.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::overlay::{Piece, PieceOverlay};
use super::pager::FilePager;
use crate::text_store::ByteRange;
use crate::vlf::overlay::VlfSavePolicy;

// ---------------------------------------------------------------------------
// Read chunk size
// ---------------------------------------------------------------------------

/// Chunk size used when reading `Original` file ranges during save (1 MiB).
///
/// Smaller than the default page size so the write buffer stays bounded and
/// progress callbacks fire at a reasonable granularity.
const SAVE_CHUNK_BYTES: u64 = 1024 * 1024;

/// Maximum changed-window bytes staged for in-place optimizations (64 MiB).
const IN_PLACE_CHANGED_WINDOW_MAX_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// SaveProgress
// ---------------------------------------------------------------------------

/// Progress snapshot delivered to the `on_progress` callback during a
/// streaming VLF save.
#[derive(Debug, Clone, Copy)]
pub struct SaveProgress {
    /// Bytes durably staged or written so far for the active save policy.
    pub bytes_written: u64,
    /// Total bytes to write (logical document length).
    pub total_bytes: u64,
}

// ---------------------------------------------------------------------------
// VlfSaveError
// ---------------------------------------------------------------------------

/// Errors returned by [`stream_save_pieces`].
#[derive(Debug)]
pub enum VlfSaveError {
    /// An I/O error occurred while writing to or renaming the temp file.
    ///
    /// The `PathBuf` is the path that triggered the error.
    Io(io::Error, PathBuf),
    /// The `on_progress` callback returned `false` before the rename commit.
    ///
    /// The temp file has been removed.  The original file is intact.
    Cancelled,
    /// [`VlfStore::enable_editing`] has not been called; there is no overlay
    /// to save from.
    EditingNotEnabled,
    /// The requested optimized policy does not match the overlay byte delta.
    InvalidPolicy(&'static str),
}

impl std::fmt::Display for VlfSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VlfSaveError::Io(e, path) => {
                write!(f, "VLF save I/O error on {}: {e}", path.display())
            }
            VlfSaveError::Cancelled => write!(f, "VLF save cancelled by caller"),
            VlfSaveError::EditingNotEnabled => {
                write!(f, "VLF editing not enabled; nothing to save")
            }
            VlfSaveError::InvalidPolicy(reason) => write!(f, "invalid VLF save policy: {reason}"),
        }
    }
}

impl std::error::Error for VlfSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VlfSaveError::Io(e, _) => Some(e),
            VlfSaveError::Cancelled
            | VlfSaveError::EditingNotEnabled
            | VlfSaveError::InvalidPolicy(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// stream_save_pieces
// ---------------------------------------------------------------------------

/// Stream the VLF piece sequence to `dest` using a temp-file-then-rename
/// durability strategy.
///
/// # Parameters
///
/// - `pieces`      — Ordered piece list from [`PieceOverlay::pieces`].
/// - `overlay`     — Used to resolve `Inserted` piece bytes.
/// - `pager`       — Used to read `Original` file ranges.
/// - `dest`        — Final destination path.
/// - `policy`      — Determines temp-dir placement and whether to save-as.
/// - `on_progress` — Called after each written chunk.  Return `false` to
///   cancel **before** the rename.  After rename, not called.
///
/// # Cancellation after commit
///
/// Once `fs::rename` succeeds, the file is durably committed.  `on_progress`
/// is never called after the rename, so callers cannot cancel a completed
/// save.
pub fn stream_save_pieces(
    pieces: &[Piece],
    overlay: &PieceOverlay,
    pager: &FilePager,
    dest: &Path,
    policy: &VlfSavePolicy,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<(), VlfSaveError> {
    match policy {
        VlfSavePolicy::TempFileRewrite { temp_dir } => {
            stream_save_pieces_temp_file(pieces, overlay, pager, dest, dest, temp_dir, on_progress)
        }
        VlfSavePolicy::SaveAs(save_as_path) => stream_save_pieces_temp_file(
            pieces,
            overlay,
            pager,
            dest,
            save_as_path,
            &None,
            on_progress,
        ),
        VlfSavePolicy::SameSizeInPlaceOverwrite => {
            if overlay.signed_byte_delta() != 0 {
                return Err(VlfSaveError::InvalidPolicy(
                    "same-size overwrite requires zero byte delta",
                ));
            }
            if !same_canonical_path(dest, pager.canonical_path()) {
                return stream_save_pieces_temp_file(
                    pieces,
                    overlay,
                    pager,
                    dest,
                    dest,
                    &None,
                    on_progress,
                );
            }
            save_same_size_in_place(pieces, overlay, pager, dest, on_progress)
        }
        VlfSavePolicy::TailShift { fallback_temp_dir } => {
            if overlay.signed_byte_delta() == 0 {
                return Err(VlfSaveError::InvalidPolicy("tail-shift requires non-zero byte delta"));
            }
            if !same_canonical_path(dest, pager.canonical_path()) {
                return stream_save_pieces_temp_file(
                    pieces,
                    overlay,
                    pager,
                    dest,
                    dest,
                    fallback_temp_dir,
                    on_progress,
                );
            }
            save_with_tail_shift(pieces, overlay, pager, dest, fallback_temp_dir, on_progress)
        }
    }
}

fn stream_save_pieces_temp_file(
    pieces: &[Piece],
    overlay: &PieceOverlay,
    pager: &FilePager,
    dest: &Path,
    final_dest: &Path,
    requested_temp_dir: &Option<PathBuf>,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<(), VlfSaveError> {
    let total_bytes = overlay.total_byte_len();

    // Determine final destination and temp-file directory.
    let temp_dir =
        requested_temp_dir.as_deref().or_else(|| final_dest.parent()).unwrap_or(dest).to_owned();
    let final_dest = final_dest.to_owned();

    let temp_path = build_temp_path(&final_dest, &temp_dir);

    // Open temp file.
    let mut tmp_file =
        fs::File::create(&temp_path).map_err(|e| VlfSaveError::Io(e, temp_path.clone()))?;

    let mut bytes_written: u64 = 0;

    for piece in pieces {
        match piece {
            Piece::Original { file_range, .. } => {
                bytes_written = write_original_piece(
                    &mut tmp_file,
                    pager,
                    *file_range,
                    &temp_path,
                    bytes_written,
                    total_bytes,
                    on_progress,
                )?;
            }
            Piece::Inserted { .. } => {
                let bytes = overlay
                    .inserted_bytes_for_piece(piece)
                    .expect("Inserted piece must have valid buffer reference");
                tmp_file.write_all(bytes).map_err(|e| VlfSaveError::Io(e, temp_path.clone()))?;
                bytes_written += bytes.len() as u64;

                let progress = SaveProgress { bytes_written, total_bytes };
                if !on_progress(progress) {
                    drop(tmp_file);
                    let _ = fs::remove_file(&temp_path);
                    return Err(VlfSaveError::Cancelled);
                }
            }
        }
    }

    // Flush OS write buffers and sync to storage before rename.
    tmp_file.sync_all().map_err(|e| VlfSaveError::Io(e, temp_path.clone()))?;
    drop(tmp_file);

    // Atomic rename — the commit point.  After this line the file is durable.
    fs::rename(&temp_path, &final_dest).map_err(|e| VlfSaveError::Io(e, final_dest.clone()))?;

    // Sync parent directory so the rename entry is durable on Unix.
    #[cfg(target_family = "unix")]
    {
        if let Some(parent) = final_dest.parent() {
            // Best-effort: not all file systems support directory fsync.
            let _ = fs::File::open(parent).and_then(|d| d.sync_all());
        }
    }

    Ok(())
}

fn save_same_size_in_place(
    pieces: &[Piece],
    overlay: &PieceOverlay,
    pager: &FilePager,
    dest: &Path,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<(), VlfSaveError> {
    let plan = SaveOptimizationPlan::for_overlay(pieces, overlay);
    if plan.new_changed_len() > IN_PLACE_CHANGED_WINDOW_MAX_BYTES {
        return stream_save_pieces_temp_file(
            pieces,
            overlay,
            pager,
            dest,
            dest,
            &None,
            on_progress,
        );
    }
    if !on_progress(SaveProgress { bytes_written: 0, total_bytes: overlay.total_byte_len() }) {
        return Err(VlfSaveError::Cancelled);
    }

    let changed = collect_logical_range_bytes(pieces, overlay, pager, plan.new_changed_range())?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dest)
        .map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    pwrite_all(&file, plan.prefix_len, &changed)
        .map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    file.sync_all().map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    let _ = on_progress(SaveProgress {
        bytes_written: overlay.total_byte_len(),
        total_bytes: overlay.total_byte_len(),
    });
    Ok(())
}

fn save_with_tail_shift(
    pieces: &[Piece],
    overlay: &PieceOverlay,
    pager: &FilePager,
    dest: &Path,
    fallback_temp_dir: &Option<PathBuf>,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<(), VlfSaveError> {
    let plan = SaveOptimizationPlan::for_overlay(pieces, overlay);
    if plan.new_changed_len() > IN_PLACE_CHANGED_WINDOW_MAX_BYTES {
        return stream_save_pieces_temp_file(
            pieces,
            overlay,
            pager,
            dest,
            dest,
            fallback_temp_dir,
            on_progress,
        );
    }
    if !on_progress(SaveProgress { bytes_written: 0, total_bytes: overlay.total_byte_len() }) {
        return Err(VlfSaveError::Cancelled);
    }

    let changed = collect_logical_range_bytes(pieces, overlay, pager, plan.new_changed_range())?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dest)
        .map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    let delta = overlay.signed_byte_delta();

    if delta > 0 {
        file.set_len(overlay.total_byte_len()).map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
        shift_tail_right(&file, dest, plan.old_suffix_start, plan.old_len, delta as u64)?;
        pwrite_all(&file, plan.prefix_len, &changed)
            .map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    } else {
        pwrite_all(&file, plan.prefix_len, &changed)
            .map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
        shift_tail_left(&file, dest, plan.old_suffix_start, plan.old_len, (-delta) as u64)?;
        file.set_len(overlay.total_byte_len()).map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    }

    file.sync_all().map_err(|e| VlfSaveError::Io(e, dest.to_owned()))?;
    let _ = on_progress(SaveProgress {
        bytes_written: overlay.total_byte_len(),
        total_bytes: overlay.total_byte_len(),
    });
    Ok(())
}

struct SaveOptimizationPlan {
    prefix_len: u64,
    old_suffix_start: u64,
    new_suffix_start: u64,
    old_len: u64,
}

impl SaveOptimizationPlan {
    fn for_overlay(pieces: &[Piece], overlay: &PieceOverlay) -> Self {
        let old_len = overlay.original_file_byte_len();
        let new_len = overlay.total_byte_len();
        let prefix_len = common_original_prefix_len(pieces);
        let suffix_len = common_original_suffix_len(pieces, old_len, new_len)
            .min(old_len.saturating_sub(prefix_len))
            .min(new_len.saturating_sub(prefix_len));
        SaveOptimizationPlan {
            prefix_len,
            old_suffix_start: old_len - suffix_len,
            new_suffix_start: new_len - suffix_len,
            old_len,
        }
    }

    fn new_changed_range(&self) -> ByteRange {
        ByteRange::new(self.prefix_len, self.new_suffix_start)
    }

    fn new_changed_len(&self) -> u64 {
        self.new_suffix_start.saturating_sub(self.prefix_len)
    }
}

fn common_original_prefix_len(pieces: &[Piece]) -> u64 {
    let mut logical = 0;
    for piece in pieces {
        match piece {
            Piece::Original { file_range, .. } if file_range.start.0 == logical => {
                logical += file_range.len();
            }
            _ => break,
        }
    }
    logical
}

fn common_original_suffix_len(pieces: &[Piece], old_len: u64, new_len: u64) -> u64 {
    let mut original_end = old_len;
    let mut logical_end = new_len;
    let mut suffix_len = 0;
    for piece in pieces.iter().rev() {
        match piece {
            Piece::Original { file_range, .. } if file_range.end.0 == original_end => {
                let len = file_range.len();
                if len > logical_end {
                    break;
                }
                suffix_len += len;
                original_end = file_range.start.0;
                logical_end -= len;
            }
            _ => break,
        }
    }
    suffix_len
}

fn collect_logical_range_bytes(
    pieces: &[Piece],
    overlay: &PieceOverlay,
    pager: &FilePager,
    range: ByteRange,
) -> Result<Vec<u8>, VlfSaveError> {
    let len = range.len();
    if len > IN_PLACE_CHANGED_WINDOW_MAX_BYTES {
        return Err(VlfSaveError::InvalidPolicy("changed window exceeds optimization buffer"));
    }
    let mut out = Vec::with_capacity(len as usize);
    let mut logical_start = 0;
    for piece in pieces {
        let logical_end = logical_start + piece.byte_len();
        if logical_end <= range.start.0 {
            logical_start = logical_end;
            continue;
        }
        if logical_start >= range.end.0 {
            break;
        }
        let overlap_start = logical_start.max(range.start.0);
        let overlap_end = logical_end.min(range.end.0);
        let piece_offset = overlap_start - logical_start;
        let overlap_len = overlap_end - overlap_start;
        match piece {
            Piece::Original { file_range, .. } => {
                let start = file_range.start.0 + piece_offset;
                let bytes = pager
                    .read_for_save(ByteRange::new(start, start + overlap_len))
                    .map_err(|e| VlfSaveError::Io(e, pager.canonical_path().to_owned()))?;
                out.extend_from_slice(&bytes);
            }
            Piece::Inserted { range: insert_range, .. } => {
                let bytes = overlay
                    .inserted_bytes_for_piece(piece)
                    .expect("Inserted piece must have valid buffer reference");
                let start = piece_offset as usize;
                let end = (piece_offset + overlap_len) as usize;
                debug_assert_eq!(bytes.len() as u64, insert_range.len());
                out.extend_from_slice(&bytes[start..end]);
            }
        }
        logical_start = logical_end;
    }
    Ok(out)
}

fn shift_tail_right(
    file: &fs::File,
    path: &Path,
    tail_start: u64,
    old_len: u64,
    delta: u64,
) -> Result<(), VlfSaveError> {
    let mut end = old_len;
    while end > tail_start {
        let start = end.saturating_sub(SAVE_CHUNK_BYTES).max(tail_start);
        let bytes = super::pager::pread_exact(file, start, (end - start) as usize)
            .map_err(|e| VlfSaveError::Io(e, path.to_owned()))?;
        pwrite_all(file, start + delta, &bytes)
            .map_err(|e| VlfSaveError::Io(e, path.to_owned()))?;
        end = start;
    }
    Ok(())
}

fn shift_tail_left(
    file: &fs::File,
    path: &Path,
    tail_start: u64,
    old_len: u64,
    delta: u64,
) -> Result<(), VlfSaveError> {
    let mut start = tail_start;
    while start < old_len {
        let end = (start + SAVE_CHUNK_BYTES).min(old_len);
        let bytes = super::pager::pread_exact(file, start, (end - start) as usize)
            .map_err(|e| VlfSaveError::Io(e, path.to_owned()))?;
        pwrite_all(file, start - delta, &bytes)
            .map_err(|e| VlfSaveError::Io(e, path.to_owned()))?;
        start = end;
    }
    Ok(())
}

#[cfg(unix)]
fn pwrite_all(file: &fs::File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write file chunk"));
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(windows)]
fn pwrite_all(file: &fs::File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write file chunk"));
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Ok(())
}

fn same_canonical_path(lhs: &Path, rhs: &Path) -> bool {
    lhs.canonicalize().is_ok_and(|lhs| lhs == rhs)
}

// ---------------------------------------------------------------------------
// write_original_piece (internal)
// ---------------------------------------------------------------------------

/// Write an `Original` piece's file bytes to the temp file in
/// `SAVE_CHUNK_BYTES`-sized chunks, calling `on_progress` after each chunk.
///
/// Returns the updated `bytes_written` counter, or cancels and returns
/// `Err(Cancelled)` if `on_progress` returns `false`.
fn write_original_piece(
    tmp_file: &mut fs::File,
    pager: &FilePager,
    file_range: ByteRange,
    temp_path: &Path,
    mut bytes_written: u64,
    total_bytes: u64,
    on_progress: &mut dyn FnMut(SaveProgress) -> bool,
) -> Result<u64, VlfSaveError> {
    let mut pos = file_range.start.0;
    let end = file_range.end.0;

    while pos < end {
        let chunk_end = (pos + SAVE_CHUNK_BYTES).min(end);
        let chunk_range = ByteRange::new(pos, chunk_end);

        let raw = pager
            .read_for_save(chunk_range)
            .map_err(|e| VlfSaveError::Io(e, temp_path.to_owned()))?;

        tmp_file.write_all(&raw).map_err(|e| VlfSaveError::Io(e, temp_path.to_owned()))?;

        bytes_written += raw.len() as u64;
        pos = chunk_end;

        let progress = SaveProgress { bytes_written, total_bytes };
        if !on_progress(progress) {
            // Drop the file before removing so Windows does not refuse deletion.
            return Err(VlfSaveError::Cancelled);
        }
    }

    Ok(bytes_written)
}

// ---------------------------------------------------------------------------
// build_temp_path
// ---------------------------------------------------------------------------

/// Build a temp-file path adjacent to `dest` (or inside `temp_dir`).
///
/// The extension is the same as `dest` with `.swp` appended, matching the
/// convention used by `try_save` for rope buffers.
fn build_temp_path(dest: &Path, temp_dir: &Path) -> PathBuf {
    let tmp_extension = dest.extension().map_or_else(
        || OsString::from("swp"),
        |ext| {
            let mut e = ext.to_os_string();
            e.push(".swp");
            e
        },
    );
    let file_name = dest.with_extension(tmp_extension);
    let name = file_name.file_name().unwrap_or(file_name.as_os_str());
    temp_dir.join(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    use crate::vlf::overlay::{OverlayEditContext, PieceOverlay, TextMetrics};
    use crate::vlf::pager::FilePager;

    fn write_temp_fixture(content: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    fn read_file_bytes(path: &Path) -> Vec<u8> {
        let mut buf = Vec::new();
        fs::File::open(path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    // -----------------------------------------------------------------------
    // Unedited overlay (single Original piece = whole file)
    // -----------------------------------------------------------------------

    #[test]
    fn save_unedited_overlay_produces_identical_file() {
        let content = b"hello world\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let overlay = PieceOverlay::new(metrics);
        let out = tempfile::NamedTempFile::new().unwrap();
        let policy = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        let mut progress_calls = 0u32;
        stream_save_pieces(overlay.pieces(), &overlay, &pager, out.path(), &policy, &mut |_| {
            progress_calls += 1;
            true
        })
        .unwrap();
        assert_eq!(read_file_bytes(out.path()), content);
        assert!(progress_calls > 0, "progress callback must fire");
    }

    // -----------------------------------------------------------------------
    // Edited overlay (insert in middle)
    // -----------------------------------------------------------------------

    #[test]
    fn save_with_inserted_piece_interleaves_correctly() {
        let content = b"hello world\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        // Insert " beautiful" after "hello" (offset 5).
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.insert_in_group(5, " beautiful", ctx).unwrap();

        let out = tempfile::NamedTempFile::new().unwrap();
        let policy = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        stream_save_pieces(overlay.pieces(), &overlay, &pager, out.path(), &policy, &mut |_| true)
            .unwrap();
        assert_eq!(read_file_bytes(out.path()), b"hello beautiful world\n");
    }

    // -----------------------------------------------------------------------
    // Delete then save
    // -----------------------------------------------------------------------

    #[test]
    fn save_after_delete_writes_shorter_file() {
        let content = b"abcdefghij\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        // Delete bytes [3,7) = "defg".
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        use crate::text_store::ByteRange;
        overlay.delete_in_group(ByteRange::new(3, 7), ctx).unwrap();

        let out = tempfile::NamedTempFile::new().unwrap();
        let policy = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        stream_save_pieces(overlay.pieces(), &overlay, &pager, out.path(), &policy, &mut |_| true)
            .unwrap();
        assert_eq!(read_file_bytes(out.path()), b"abchij\n");
    }

    // -----------------------------------------------------------------------
    // Cancellation before commit
    // -----------------------------------------------------------------------

    #[test]
    fn cancellation_before_commit_removes_temp_and_leaves_original_intact() {
        let content = b"original content\n";
        let fixture = write_temp_fixture(content);
        // Write a second "original" file to save over.
        let out = write_temp_fixture(b"old output\n");
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let overlay = PieceOverlay::new(metrics);
        let policy = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        // Cancel immediately on first progress call.
        let result = stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            out.path(),
            &policy,
            &mut |_| false, // cancel immediately
        );
        assert!(matches!(result, Err(VlfSaveError::Cancelled)));
        // Original output file content must be unchanged.
        assert_eq!(read_file_bytes(out.path()), b"old output\n");
    }

    // -----------------------------------------------------------------------
    // SaveAs policy
    // -----------------------------------------------------------------------

    #[test]
    fn save_as_policy_writes_to_alternate_path() {
        let content = b"data\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let overlay = PieceOverlay::new(metrics);
        let save_as_target = tempfile::NamedTempFile::new().unwrap();
        let policy = VlfSavePolicy::SaveAs(save_as_target.path().to_owned());
        stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            save_as_target.path(), // `dest` ignored for SaveAs; policy path is used
            &policy,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(read_file_bytes(save_as_target.path()), content);
    }

    #[test]
    fn same_size_in_place_overwrites_changed_window() {
        let content = b"abcdef\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.delete_in_group(ByteRange::new(2, 4), ctx).unwrap();
        overlay.insert_in_group(2, "XY", ctx).unwrap();

        let policy = VlfSavePolicy::SameSizeInPlaceOverwrite;
        stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            fixture.path(),
            &policy,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(read_file_bytes(fixture.path()), b"abXYef\n");
    }

    #[test]
    fn tail_shift_grows_file_in_place() {
        let content = b"abcdef\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.insert_in_group(3, "XY", ctx).unwrap();

        let policy = VlfSavePolicy::TailShift { fallback_temp_dir: None };
        stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            fixture.path(),
            &policy,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(read_file_bytes(fixture.path()), b"abcXYdef\n");
    }

    #[test]
    fn tail_shift_shrinks_file_in_place() {
        let content = b"abcdef\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.delete_in_group(ByteRange::new(1, 4), ctx).unwrap();

        let policy = VlfSavePolicy::TailShift { fallback_temp_dir: None };
        stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            fixture.path(),
            &policy,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(read_file_bytes(fixture.path()), b"aef\n");
    }

    #[test]
    fn optimized_save_cancellation_before_commit_leaves_file_intact() {
        let content = b"abcdef\n";
        let fixture = write_temp_fixture(content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let mut overlay = PieceOverlay::new(metrics);
        let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
        overlay.insert_in_group(3, "XY", ctx).unwrap();

        let policy = VlfSavePolicy::TailShift { fallback_temp_dir: None };
        let result = stream_save_pieces(
            overlay.pieces(),
            &overlay,
            &pager,
            fixture.path(),
            &policy,
            &mut |_| false,
        );
        assert!(matches!(result, Err(VlfSaveError::Cancelled)));
        assert_eq!(read_file_bytes(fixture.path()), content);
    }

    // -----------------------------------------------------------------------
    // Progress reporting
    // -----------------------------------------------------------------------

    #[test]
    fn progress_bytes_written_increases_monotonically() {
        let content = vec![b'x'; 4 * 1024 * 1024]; // 4 MiB > SAVE_CHUNK_BYTES
        let fixture = write_temp_fixture(&content);
        let pager = FilePager::open(fixture.path()).unwrap();
        let metrics = TextMetrics { byte_len: content.len() as u64, ..TextMetrics::default() };
        let overlay = PieceOverlay::new(metrics);
        let out = tempfile::NamedTempFile::new().unwrap();
        let policy = VlfSavePolicy::TempFileRewrite { temp_dir: None };
        let mut prev: u64 = 0;
        let mut calls: u32 = 0;
        stream_save_pieces(overlay.pieces(), &overlay, &pager, out.path(), &policy, &mut |p| {
            assert!(
                p.bytes_written >= prev,
                "bytes_written must not decrease: {} < {}",
                p.bytes_written,
                prev
            );
            assert_eq!(p.total_bytes, content.len() as u64);
            prev = p.bytes_written;
            calls += 1;
            true
        })
        .unwrap();
        // 4 MiB / 1 MiB = 4 chunks → 4 progress calls.
        assert!(calls >= 4, "expected at least 4 progress calls, got {calls}");
    }
}
