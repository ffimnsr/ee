//! Applying an `update` payload onto the frontend line cache.
//!
//! Two frontends keep the same line-cache state in step with the core's `update`
//! stream: `BufState` for an open buffer and `Backend`, which mirrors the active
//! view. The op walk and the VLF window replacement therefore live here once, as
//! functions that compute a result without touching the caller's state, so both
//! callers commit the same way and neither can diverge from the other.
//!
//! Rejection contract: a malformed payload must leave the caller's cache exactly
//! as it was. Both callers take their caches before applying, and returning early
//! with them emptied would strand the buffer — the next payload's `copy`/`skip`
//! ops would fail against an empty cache, no range would read as invalid, and the
//! app would never re-request the rows. The VLF helper therefore restores the
//! cache itself, and the rope walk is pure.
//!
//! Scope names are resolved once per payload into shared `Arc<str>`s, so spans of
//! one scope cost one allocation rather than one per span.

use std::io;

use crate::backend::update::{scope_table, take_op_rows, validate_blob_ops};
use crate::backend::{CoreUpdateKind, CoreUpdateOp, LineSlot, checked_advance};

use super::{line_text_for_slot, vlf_window_from_ops};

/// Applies one payload's rope ops onto `previous`, producing the next line cache.
///
/// `blob` is the payload's binary text frame, when it carried one. Its rows are
/// validated and handed out positionally before anything is built, so a malformed
/// frame rejects the whole payload instead of mixing rows.
///
/// `copy` ops clear cached cursor data: content is unchanged from the core's
/// perspective while cursor positions may have moved, and only `insert`/`update`
/// ops carry authoritative cursor positions.
pub(crate) fn rope_update(
    ops: Vec<CoreUpdateOp>,
    previous: &[LineSlot],
    scopes: &[String],
    blob: Option<&[u8]>,
) -> io::Result<Vec<LineSlot>> {
    let mut frame_rows = validate_blob_ops(&ops, blob)?;
    let scopes = scope_table(scopes);
    let mut next_cache = Vec::new();
    let mut source_index = 0;

    for op in ops {
        let mut op_rows = take_op_rows(&mut frame_rows, &op).into_iter();
        match op.op {
            CoreUpdateKind::Insert => {
                if op.lines.len() != op.n {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "insert op length mismatch: expected {}, got {}",
                            op.n,
                            op.lines.len()
                        ),
                    ));
                }
                for line in op.lines {
                    let bytes = op_rows.next().flatten();
                    next_cache.push(LineSlot::from_core_line(line, &scopes, bytes)?);
                }
            }
            CoreUpdateKind::Skip => {
                source_index = checked_advance(source_index, op.n, previous.len(), "skip")?;
            }
            CoreUpdateKind::Invalidate => {
                next_cache.extend(std::iter::repeat_n(LineSlot::Invalid, op.n));
            }
            CoreUpdateKind::Copy => {
                let end = checked_advance(source_index, op.n, previous.len(), "copy")?;
                for slot in previous[source_index..end].iter().cloned() {
                    match slot {
                        LineSlot::Known(mut line) => {
                            line.cursors.clear();
                            next_cache.push(LineSlot::Known(line));
                        }
                        invalid => next_cache.push(invalid),
                    }
                }
                source_index = end;
            }
            CoreUpdateKind::Update => {
                if op.lines.len() != op.n {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "update op length mismatch: expected {}, got {}",
                            op.n,
                            op.lines.len()
                        ),
                    ));
                }
                let end = checked_advance(source_index, op.n, previous.len(), "update")?;
                for (slot, line) in previous[source_index..end].iter().cloned().zip(op.lines) {
                    let bytes = op_rows.next().flatten();
                    next_cache.push(slot.merge(line, &scopes, bytes)?);
                }
                source_index = end;
            }
        }
    }

    Ok(next_cache)
}

/// Flattens a line cache into the per-index text vector the frontend renders from.
///
/// A cache holding a single empty line yields no texts at all: that is the
/// "empty document" shape the renderer and the scroll math already treat as
/// zero-height, and materializing one empty string there shifted every row.
pub(crate) fn line_texts_for_cache(line_cache: &[LineSlot]) -> Vec<String> {
    if matches!(line_cache, [LineSlot::Known(line)] if line.text.is_empty()) {
        return Vec::new();
    }
    line_cache.iter().map(line_text_for_slot).collect()
}

/// Replaces the bounded VLF window from one payload's insert segments.
///
/// Returns the applied window bounds, or `None` when the payload carried no
/// insert rows (copy/skip-only updates keep the window and only refresh cursors
/// and annotations). On rejection the caller's cache is restored, so a malformed
/// payload cannot strand the window.
pub(crate) fn replace_vlf_window(
    line_cache: &mut Vec<LineSlot>,
    lines: &mut Vec<String>,
    ops: Vec<CoreUpdateOp>,
    window_start: usize,
    scopes: &[String],
    blob: Option<&[u8]>,
) -> io::Result<Option<(usize, usize)>> {
    validate_blob_ops(&ops, blob)?;
    let old_cache = std::mem::take(line_cache);
    match vlf_window_from_ops(ops, &old_cache, window_start, scopes, blob) {
        Ok(Some((start, end, cache))) => {
            *lines = cache.iter().map(line_text_for_slot).collect();
            *line_cache = cache;
            Ok(Some((start, end)))
        }
        Ok(None) => {
            *line_cache = old_cache;
            Ok(None)
        }
        Err(err) => {
            *line_cache = old_cache;
            Err(err)
        }
    }
}
