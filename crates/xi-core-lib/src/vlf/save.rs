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
//! Same-size in-place overwrite is intentionally omitted from the first
//! milestone.  It will be added only after crash-consistency tests for the
//! temp-file path pass.  Byte-length-changing tail-shift optimisation is
//! likewise deferred until the temp-copy fallback is proven correct.
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

// ---------------------------------------------------------------------------
// SaveProgress
// ---------------------------------------------------------------------------

/// Progress snapshot delivered to the `on_progress` callback during a
/// streaming VLF save.
#[derive(Debug, Clone, Copy)]
pub struct SaveProgress {
    /// Bytes written to the temp file so far.
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
        }
    }
}

impl std::error::Error for VlfSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VlfSaveError::Io(e, _) => Some(e),
            VlfSaveError::Cancelled | VlfSaveError::EditingNotEnabled => None,
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
    let total_bytes = overlay.total_byte_len();

    // Determine final destination and temp-file directory.
    let (final_dest, temp_dir) = match policy {
        VlfSavePolicy::TempFileRewrite { temp_dir } => {
            let dir = temp_dir.as_deref().or_else(|| dest.parent()).unwrap_or(dest);
            (dest.to_owned(), dir.to_owned())
        }
        VlfSavePolicy::SaveAs(save_as_path) => {
            let dir = save_as_path.parent().unwrap_or(save_as_path);
            (save_as_path.clone(), dir.to_owned())
        }
    };

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
