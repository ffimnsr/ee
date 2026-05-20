use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use xi_core_lib::XiCore;
use xi_core_lib::config::Table;
use xi_core_lib::plugin_rpc::{Diagnostic, SelectionRange, SymbolItem};
use xi_core_lib::rpc::{FoldRangePreview, LineReplacement};
use xi_rpc::RpcLoop;

use crate::backend::{
    BackendEvent, CachedLine, ChannelReader, ChannelWriter, CoreAnnotation, CoreSyntaxSpan,
    CoreUpdate, CoreUpdateKind, LineSlot, NavigationTarget, PendingRequests, PendingUiAction,
    VlfSearchRange, block_for_response, checked_advance, coalesce_backend_events,
    drain_sync_notifications, invalid_line_ranges, invalid_line_ranges_bounded,
    normalize_line_text, parse_response, recv_with_timeout, send_rpc_notification,
    send_rpc_request, startup_render_ready, xi_reader_thread,
};
use crate::text::previous_char_boundary;

pub(crate) type BufferId = u32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StartupProfile {
    pub(crate) new_view_rpc: Duration,
    pub(crate) init_notification_drain: Duration,
    pub(crate) init_event_apply: Duration,
    pub(crate) pump_init: Duration,
    pub(crate) update_apply: Duration,
    pub(crate) rebuild_lines: Duration,
    pub(crate) config_sync: Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ApplyUpdateStats {
    pub(crate) rebuild_lines: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineRequestScope {
    Viewport,
    WholeDocument,
}

const NORMAL_INVALID_LINE_OVERSCAN: usize = 64;
const NORMAL_INVALID_LINE_DEFAULT_WINDOW: usize = 256;

// ── Per-view buffer state ─────────────────────────────────────────────────────

/// All state associated with one open xi view (no connection fields).
#[derive(Debug)]
pub(crate) struct BufState {
    pub(crate) id: BufferId,
    pub(crate) path: Option<PathBuf>,
    pub(crate) display_name: Option<String>,
    pub(crate) view_id: String,
    pub(crate) editor_config_synced: bool,
    pub(crate) pending_line_request: bool,
    pub(crate) line_cache: Vec<LineSlot>,
    /// Whole-buffer text mirror kept in sync with `line_cache` for normal/constrained mode.
    ///
    /// **Policy**: prefer `get_line()`, `line_len()`, and `line_start_offset()` for
    /// single-line or bounded reads.  Access `lines` directly only where command policy
    /// explicitly requires whole-document access (diagnostics offset lookup, crash
    /// recovery, source-control diff, whole-document transforms, fold operations).
    /// In VLF mode this field is always empty; never read it in VLF-aware paths.
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_line: usize,
    pub(crate) cursor_col: usize,
    pub(crate) pristine: bool,
    pub(crate) save_complete: bool,
    pub(crate) last_save_generation: u64,
    pub(crate) completed_save_generation: u64,
    pub(crate) status_message: Option<String>,
    pub(crate) last_scroll: Option<(usize, usize)>,
    /// Last-known mtime of the backing file; `None` for scratch buffers.
    pub(crate) mtime: Option<SystemTime>,
    /// Set when the backing file has been modified by another process.
    pub(crate) externally_modified: bool,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) annotations: Vec<CoreAnnotation>,
    /// True when the backend opened this buffer in VLF (Very Large File) mode.
    /// In VLF mode `lines` is never materialized; rendering reads `line_cache`
    /// directly for the visible viewport range only.
    pub(crate) is_vlf: bool,
    /// Logical line number represented by `line_cache[0]` in VLF mode.
    ///
    /// VLF keeps only the loaded viewport window here; `vlf_approx_line_count`
    /// carries document size so huge files do not allocate one slot per line.
    pub(crate) vlf_cache_start_line: usize,
    pub(crate) vlf_previous_viewport: Option<(usize, Vec<LineSlot>)>,
    /// Monotone counter incremented on every VLF viewport scroll.
    ///
    /// Each `vlf_viewport` request carries this counter; `vlf_chunks` responses
    /// with a different generation are discarded so stale out-of-order data
    /// never overwrites a newer scroll position in the line cache.
    pub(crate) vlf_generation: u64,
    /// Last approximate total line count reported by a `vlf_chunks` response.
    /// Used for reported document size while the background index is still
    /// scanning the file.
    pub(crate) vlf_approx_line_count: u64,
    /// True when `vlf_approx_line_count` is backend-confirmed exact.
    pub(crate) vlf_line_count_exact: bool,
    /// True when the next matching VLF response should move cursor to returned tail.
    pub(crate) pending_vlf_tail_jump: bool,
    /// Backend-authoritative visible VLF match ranges for current search.
    pub(crate) vlf_search_ranges: Vec<VlfSearchRange>,
}

pub(crate) struct VlfChunkUpdate<'a> {
    pub(crate) generation: u64,
    pub(crate) line_start: u64,
    pub(crate) lines: &'a [String],
    pub(crate) syntax_spans: &'a [Vec<CoreSyntaxSpan>],
    pub(crate) approximate_line_count: u64,
    pub(crate) line_count_exact: bool,
}

impl BufState {
    const VLF_PREVIOUS_VIEWPORT_MAX_BYTES: usize = 32 * 1024 * 1024;

    pub(crate) fn title(&self) -> String {
        if let Some(name) = &self.display_name {
            return name.clone();
        }
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
            .to_owned()
    }

    pub(crate) fn apply_update(&mut self, update: CoreUpdate) -> io::Result<ApplyUpdateStats> {
        let CoreUpdate { ops, pristine, annotations } = update;
        let previous = std::mem::take(&mut self.line_cache);
        let previous_lines = std::mem::take(&mut self.lines);
        let mut next_cache = Vec::new();
        let mut next_lines = Vec::new();
        let mut source_index = 0;

        self.pristine = pristine;
        self.annotations = annotations;

        for op in ops {
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
                        let slot = LineSlot::from(line);
                        next_lines.push(line_text_for_slot(&slot));
                        next_cache.push(slot);
                    }
                }
                CoreUpdateKind::Skip => {
                    source_index = checked_advance(source_index, op.n, previous.len(), "skip")?;
                }
                CoreUpdateKind::Invalidate => {
                    next_cache.extend(std::iter::repeat_n(LineSlot::Invalid, op.n));
                    next_lines.extend(std::iter::repeat_n(String::new(), op.n));
                }
                CoreUpdateKind::Copy => {
                    let end = checked_advance(source_index, op.n, previous.len(), "copy")?;
                    // Clear cursor data from copied slots: Copy op means content is
                    // unchanged from xi-core's perspective, but cursor positions may
                    // have moved. Only Insert/Update ops carry authoritative cursor data.
                    for (offset, slot) in previous[source_index..end].iter().enumerate() {
                        match slot.clone() {
                            LineSlot::Known(mut line) => {
                                line.cursors.clear();
                                next_cache.push(LineSlot::Known(line));
                            }
                            invalid => next_cache.push(invalid),
                        }
                        next_lines.push(
                            previous_lines
                                .get(source_index + offset)
                                .cloned()
                                .unwrap_or_else(|| line_text_for_slot(slot)),
                        );
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
                        let slot = slot.merge(line)?;
                        next_lines.push(line_text_for_slot(&slot));
                        next_cache.push(slot);
                    }
                    source_index = end;
                }
            }
        }

        self.line_cache = next_cache;
        self.lines = next_lines;
        if matches!(
            self.line_cache.as_slice(),
            [LineSlot::Known(CachedLine { text, .. })] if text.is_empty()
        ) {
            self.lines.clear();
        }
        self.sync_cursor_from_cache();
        Ok(ApplyUpdateStats { rebuild_lines: Duration::ZERO })
    }

    pub(crate) fn rebuild_lines(&mut self) {
        // VLF mode: skip full-buffer clone; `lines` stays empty.
        // Rendering reads `line_cache` directly for the visible viewport range.
        if self.is_vlf {
            return;
        }

        self.lines = self
            .line_cache
            .iter()
            .map(|slot| match slot {
                LineSlot::Known(line) => line.text.clone(),
                LineSlot::Invalid => String::new(),
            })
            .collect();

        if matches!(
            self.line_cache.as_slice(),
            [LineSlot::Known(CachedLine { text, .. })] if text.is_empty()
        ) {
            self.lines.clear();
        }
    }

    fn sync_cursor_from_cache(&mut self) {
        for (line_index, slot) in self.line_cache.iter().enumerate() {
            let LineSlot::Known(line) = slot else { continue };
            if let Some(&cursor_col) = line.cursors.first() {
                self.cursor_line = if self.is_vlf {
                    self.vlf_cache_start_line.saturating_add(line_index)
                } else {
                    line_index
                };
                self.cursor_col = previous_char_boundary(&line.text, cursor_col);
                self.clamp_cursor();
                return;
            }
        }
        self.clamp_cursor();
    }

    pub(crate) fn clamp_cursor(&mut self) {
        if self.is_vlf {
            self.cursor_line = self.cursor_line.min(self.line_count().saturating_sub(1));
            if let Some(LineSlot::Known(line)) = self.line_slot(self.cursor_line) {
                self.cursor_col = previous_char_boundary(&line.text, self.cursor_col);
            }
            return;
        }

        if self.lines.is_empty() {
            self.cursor_line = 0;
            self.cursor_col = 0;
            return;
        }
        self.cursor_line = self.cursor_line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
    }

    pub(crate) fn is_fully_cached(&self) -> bool {
        if self.is_vlf {
            return self.vlf_line_count_exact
                && self.vlf_cache_start_line == 0
                && self.line_cache.len() == self.line_count()
                && self.line_cache.iter().all(|slot| matches!(slot, LineSlot::Known(_)));
        }
        self.line_cache.iter().all(|slot| matches!(slot, LineSlot::Known(_)))
    }

    /// Return the total line count regardless of mode.
    ///
    /// In VLF mode `lines` is empty; use `line_cache.len()` instead.
    pub(crate) fn line_count(&self) -> usize {
        if self.is_vlf {
            let reported = usize::try_from(self.vlf_approx_line_count).unwrap_or(usize::MAX);
            if self.vlf_line_count_exact && reported > 0 {
                reported
            } else {
                reported.max(self.vlf_cache_start_line.saturating_add(self.line_cache.len()))
            }
        } else {
            self.lines.len()
        }
    }

    /// Return the text of a line by logical index, or `None` if the slot is not loaded.
    ///
    /// In normal mode reads from `lines`.  In VLF mode reads from `line_cache`
    /// and returns `None` for `LineSlot::Invalid` (show a loading indicator).
    pub(crate) fn get_line(&self, idx: usize) -> Option<&str> {
        if self.is_vlf {
            match self.line_slot(idx)? {
                LineSlot::Known(line) => Some(&line.text),
                LineSlot::Invalid => None,
            }
        } else {
            self.lines.get(idx).map(|s| s.as_str())
        }
    }

    pub(crate) fn line_slot(&self, idx: usize) -> Option<&LineSlot> {
        if self.is_vlf {
            let local = idx.checked_sub(self.vlf_cache_start_line)?;
            self.line_cache.get(local)
        } else {
            self.line_cache.get(idx)
        }
    }

    pub(crate) fn line_len(&self, idx: usize) -> Option<usize> {
        self.get_line(idx).map(str::len)
    }

    pub(crate) fn line_range_owned(&self, start: usize, end: usize) -> Option<Vec<String>> {
        if start > end {
            return Some(Vec::new());
        }
        (start..=end).map(|idx| self.get_line(idx).map(str::to_owned)).collect()
    }

    pub(crate) fn line_start_offset(&self, line: usize) -> Option<usize> {
        let mut offset = 0usize;
        for idx in 0..line {
            offset = offset.checked_add(self.get_line(idx)?.len() + 1)?;
        }
        Some(offset)
    }

    pub(crate) fn whole_text(&self) -> Option<String> {
        if self.is_vlf {
            return None;
        }
        if self.line_count() == 0 {
            return Some(String::new());
        }
        let mut text = String::new();
        for idx in 0..self.line_count() {
            if idx > 0 {
                text.push('\n');
            }
            text.push_str(self.get_line(idx)?);
        }
        Some(text)
    }

    pub(crate) fn apply_local_vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> bool {
        if !self.is_vlf || start_line > end_line {
            return false;
        }

        let Some(start_local) = start_line.checked_sub(self.vlf_cache_start_line) else {
            return false;
        };
        let Some(end_local) = end_line.checked_sub(self.vlf_cache_start_line) else {
            return false;
        };
        if end_local >= self.line_cache.len() {
            return false;
        }

        let (LineSlot::Known(first_line), LineSlot::Known(last_line)) =
            (&self.line_cache[start_local], &self.line_cache[end_local])
        else {
            return false;
        };

        if start_col > first_line.text.len()
            || end_col > last_line.text.len()
            || !first_line.text.is_char_boundary(start_col)
            || !last_line.text.is_char_boundary(end_col)
        {
            return false;
        }

        let mut combined = first_line.text[..start_col].to_owned();
        combined.push_str(text);
        combined.push_str(&last_line.text[end_col..]);

        let replacement_lines: Vec<String> = combined.split('\n').map(str::to_owned).collect();
        let replacement_spans = build_optimistic_vlf_spans(
            first_line,
            last_line,
            start_col,
            end_col,
            &replacement_lines,
        );
        let replacement_count = replacement_lines.len();
        let replacement =
            replacement_lines.into_iter().zip(replacement_spans).map(|(line, syntax_spans)| {
                LineSlot::Known(CachedLine { text: line, cursors: Vec::new(), syntax_spans })
            });
        let replaced_count = end_local - start_local + 1;
        self.line_cache.splice(start_local..=end_local, replacement);

        match replacement_count.cmp(&replaced_count) {
            std::cmp::Ordering::Greater => {
                self.vlf_approx_line_count = self
                    .vlf_approx_line_count
                    .saturating_add((replacement_count - replaced_count) as u64);
            }
            std::cmp::Ordering::Less => {
                self.vlf_approx_line_count = self
                    .vlf_approx_line_count
                    .saturating_sub((replaced_count - replacement_count) as u64);
            }
            std::cmp::Ordering::Equal => {}
        }

        true
    }

    fn save_current_vlf_viewport(&mut self) {
        if self.line_cache.is_empty()
            || vlf_cache_text_bytes(&self.line_cache) > Self::VLF_PREVIOUS_VIEWPORT_MAX_BYTES
        {
            self.vlf_previous_viewport = None;
            return;
        }
        self.vlf_previous_viewport = Some((self.vlf_cache_start_line, self.line_cache.clone()));
    }

    fn restore_previous_vlf_viewport_if_ready(
        &mut self,
        first_line: usize,
        last_line: usize,
    ) -> bool {
        let Some((previous_start, previous_cache)) = self.vlf_previous_viewport.as_ref() else {
            return false;
        };
        if !vlf_cache_ready(previous_start, previous_cache, first_line, last_line) {
            return false;
        }

        let Some((previous_start, previous_cache)) = self.vlf_previous_viewport.take() else {
            return false;
        };
        let current_start = self.vlf_cache_start_line;
        let current_cache = std::mem::replace(&mut self.line_cache, previous_cache);
        self.vlf_cache_start_line = previous_start;
        self.last_scroll =
            Some((previous_start, previous_start.saturating_add(self.line_cache.len())));

        if !current_cache.is_empty()
            && vlf_cache_text_bytes(&current_cache) <= Self::VLF_PREVIOUS_VIEWPORT_MAX_BYTES
        {
            self.vlf_previous_viewport = Some((current_start, current_cache));
        }
        true
    }

    /// Apply a `vlf_chunks` response to the line cache.
    ///
    /// Silently drops the response when `generation` does not match
    /// `vlf_generation`; this prevents an out-of-order reply from a superseded
    /// viewport scroll from overwriting data for the current position.
    pub(crate) fn apply_vlf_chunks(&mut self, update: VlfChunkUpdate<'_>) {
        let VlfChunkUpdate {
            generation,
            line_start,
            lines,
            syntax_spans,
            approximate_line_count,
            line_count_exact,
        } = update;

        if generation != self.vlf_generation {
            return;
        }

        self.vlf_approx_line_count = approximate_line_count;
        self.vlf_line_count_exact = line_count_exact;
        let tail_jump = self.pending_vlf_tail_jump;

        if lines.is_empty() {
            return;
        }

        let Ok(start) = usize::try_from(line_start) else {
            self.line_cache.clear();
            return;
        };
        if start != self.vlf_cache_start_line {
            self.save_current_vlf_viewport();
        }
        self.vlf_cache_start_line = start;

        self.line_cache = lines
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let spans = syntax_spans.get(i).cloned().unwrap_or_default();
                LineSlot::Known(CachedLine {
                    text: normalize_line_text(Some(text.clone())),
                    cursors: Vec::new(),
                    syntax_spans: spans,
                })
            })
            .collect();
        self.last_scroll = Some((start, start.saturating_add(lines.len())));
        if tail_jump && !lines.is_empty() {
            self.pending_vlf_tail_jump = false;
            self.cursor_line = start + lines.len() - 1;
            self.cursor_col = 0;
        }
    }
}

fn build_optimistic_vlf_spans(
    first_line: &CachedLine,
    last_line: &CachedLine,
    start_col: usize,
    end_col: usize,
    replacement_lines: &[String],
) -> Vec<Vec<CoreSyntaxSpan>> {
    let mut spans = vec![Vec::new(); replacement_lines.len()];
    if replacement_lines.is_empty() {
        return spans;
    }

    spans[0]
        .extend(first_line.syntax_spans.iter().filter(|span| span.end_byte <= start_col).cloned());

    let Some(last_result_line) = replacement_lines.last() else {
        return spans;
    };
    let suffix_len = last_line.text.len().saturating_sub(end_col);
    let suffix_start = last_result_line.len().saturating_sub(suffix_len);
    let suffix_shift = suffix_start as isize - end_col as isize;
    let last_index = spans.len() - 1;
    spans[last_index].extend(last_line.syntax_spans.iter().filter_map(|span| {
        if span.start_byte < end_col {
            return None;
        }
        let start_byte = span.start_byte.checked_add_signed(suffix_shift)?;
        let end_byte = span.end_byte.checked_add_signed(suffix_shift)?;
        Some(CoreSyntaxSpan { start_byte, end_byte, scope: span.scope.clone() })
    }));

    spans
}

impl PartialEq for BufState {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.path == other.path
            && self.display_name == other.display_name
            && self.view_id == other.view_id
            && self.editor_config_synced == other.editor_config_synced
            && self.pending_line_request == other.pending_line_request
            && self.line_cache == other.line_cache
            && self.lines == other.lines
            && self.cursor_line == other.cursor_line
            && self.cursor_col == other.cursor_col
            && self.pristine == other.pristine
            && self.save_complete == other.save_complete
            && self.last_save_generation == other.last_save_generation
            && self.completed_save_generation == other.completed_save_generation
            && self.status_message == other.status_message
            && self.last_scroll == other.last_scroll
            && self.mtime == other.mtime
            && self.externally_modified == other.externally_modified
            && self.diagnostics == other.diagnostics
            && self.annotations == other.annotations
            && self.is_vlf == other.is_vlf
            && self.vlf_cache_start_line == other.vlf_cache_start_line
            && self.vlf_generation == other.vlf_generation
            && self.vlf_approx_line_count == other.vlf_approx_line_count
            && self.vlf_line_count_exact == other.vlf_line_count_exact
            && self.pending_vlf_tail_jump == other.pending_vlf_tail_jump
            && self.vlf_search_ranges == other.vlf_search_ranges
    }
}

impl Eq for BufState {}

fn vlf_cache_text_bytes(cache: &[LineSlot]) -> usize {
    cache
        .iter()
        .map(|slot| match slot {
            LineSlot::Known(line) => line.text.len(),
            LineSlot::Invalid => 0,
        })
        .sum()
}

fn vlf_cache_ready(
    cache_start_line: &usize,
    cache: &[LineSlot],
    first_line: usize,
    last_line: usize,
) -> bool {
    if first_line >= last_line {
        return true;
    }
    let Some(start) = first_line.checked_sub(*cache_start_line) else {
        return false;
    };
    let Some(end) = last_line.checked_sub(*cache_start_line) else {
        return false;
    };
    if end > cache.len() {
        return false;
    }
    cache[start..end].iter().all(|slot| matches!(slot, LineSlot::Known(_)))
}

fn vlf_viewport_ready(buf: &BufState, first_line: usize, last_line: usize) -> bool {
    vlf_cache_ready(&buf.vlf_cache_start_line, &buf.line_cache, first_line, last_line)
}

fn line_text_for_slot(slot: &LineSlot) -> String {
    match slot {
        LineSlot::Known(line) => line.text.clone(),
        LineSlot::Invalid => String::new(),
    }
}

// ── BufferManager ─────────────────────────────────────────────────────────────

/// Manages one xi-core process and all open views/buffers.
///
/// Derefs to the currently active [`BufState`] so callers can access
/// `buffer_manager.lines`, `buffer_manager.cursor_line`, etc. directly.
#[derive(Debug)]
pub(crate) struct BufferManager {
    /// Send side of the channel to xi-core.
    pub(crate) tx: mpsc::Sender<String>,
    /// Receive side of events coming from the xi-core reader thread.
    pub(crate) backend_rx: std_mpsc::Receiver<BackendEvent>,
    core_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
    reader_shutdown: Arc<AtomicBool>,
    /// All open buffers.
    bufs: Vec<BufState>,
    /// Maps xi view_id strings to indices in `bufs`.
    view_to_idx: HashMap<String, usize>,
    /// Index of the currently active buffer in `bufs`.
    current: usize,
    /// Index of the alternate buffer (for Ctrl-^ / `:b#`).
    alternate: Option<usize>,
    access_history: Vec<BufferId>,
    modified_history: Vec<BufferId>,
    next_buf_id: BufferId,
    next_rpc_id: u64,
    /// Pending synchronous RPC responses keyed by request id.
    pending: PendingRequests,
    /// Locations reported by the backend (definition, references, …) awaiting
    /// dispatch to the App-level quickfix list.
    pub(crate) pending_locations: Vec<(String, String, Vec<NavigationTarget>)>,
    /// Symbol results awaiting dispatch to the App-level picker.
    pub(crate) pending_symbols: Vec<(String, String, Vec<SymbolItem>)>,
    pub(crate) pending_ui_actions: Vec<PendingUiAction>,
    startup_profile: StartupProfile,
    startup_profile_active: bool,
}

impl std::ops::Deref for BufferManager {
    type Target = BufState;
    fn deref(&self) -> &BufState {
        &self.bufs[self.current]
    }
}

impl std::ops::DerefMut for BufferManager {
    fn deref_mut(&mut self) -> &mut BufState {
        &mut self.bufs[self.current]
    }
}

impl Drop for BufferManager {
    fn drop(&mut self) {
        for view_id in self.bufs.iter().map(|buf| buf.view_id.clone()) {
            let _ = send_rpc_notification(&self.tx, "close_view", json!({ "view_id": view_id }));
        }

        self.reader_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        let (dummy_tx, _dummy_rx) = mpsc::channel::<String>(1);
        let core_tx = std::mem::replace(&mut self.tx, dummy_tx);
        drop(core_tx);

        if let Some(handle) = self.core_thread.take() {
            let _ = handle.join();
        }
    }
}

impl BufferManager {
    const SYNC_IDLE_LIMIT: usize = 6;
    const STARTUP_VLF_VIEWPORT_LINES: usize = 200;
    const VLF_VIEWPORT_OVERSCAN_LINES: usize = 200;
    const TAIL_VLF_PREFETCH_LINES: usize = 4096;

    fn buffer_index_for_view(&self, view_id: &str) -> Option<usize> {
        self.view_to_idx.get(view_id).copied()
    }

    /// Create a new xi-core process using already-computed config tables for
    /// the initial buffer. Reusing these tables avoids repeating config and
    /// editorconfig scans during startup.
    pub(crate) fn new_with_initial_config(
        path: Option<PathBuf>,
        general_config: Table,
        initial_overrides: Table,
        lsp_config: Table,
    ) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>(256);
        let (from_core_tx, from_core_rx) = std_mpsc::channel::<String>();
        let (backend_tx, backend_rx) = std_mpsc::channel::<BackendEvent>();

        let core_thread = thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        send_rpc_notification(
            &to_core_tx,
            "client_started",
            json!({
                "config_dir": crate::config::xi_core_config_dir(),
                "client_extras_dir": crate::config::xi_core_client_extras_dir(),
            }),
        )?;

        send_config_notification(&to_core_tx, json!("general"), general_config)?;
        send_config_notification(
            &to_core_tx,
            json!({ "plugin": crate::config::LSP_PLUGIN_NAME }),
            lsp_config,
        )?;

        let new_view_id = 1_u64;
        let new_view_started = Instant::now();
        send_rpc_request(
            &to_core_tx,
            new_view_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let mut from_core_rx = from_core_rx;
        let view_id_val = block_for_response(&mut from_core_rx, &to_core_tx, new_view_id)?;
        let new_view_rpc = new_view_started.elapsed();
        let view_id = view_id_val
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        send_config_notification(
            &to_core_tx,
            json!({ "user_override": view_id }),
            initial_overrides,
        )?;

        let init_drain_started = Instant::now();
        let init_events = drain_sync_notifications(&mut from_core_rx, &to_core_tx);
        let init_notification_drain = init_drain_started.elapsed();

        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = Arc::clone(&pending);
        let tx_clone = to_core_tx.clone();
        let reader_shutdown = Arc::new(AtomicBool::new(false));
        let reader_shutdown_clone = Arc::clone(&reader_shutdown);
        let reader_thread = thread::spawn(move || {
            xi_reader_thread(
                from_core_rx,
                tx_clone,
                backend_tx,
                pending_clone,
                Some(reader_shutdown_clone),
            )
        });

        let buf = BufState {
            id: 1,
            path: path.clone(),
            display_name: None,
            view_id: view_id.clone(),
            editor_config_synced: true,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            save_complete: true,
            last_save_generation: 0,
            completed_save_generation: 0,
            status_message: None,
            last_scroll: None,
            mtime: path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_cache_start_line: 0,
            vlf_previous_viewport: None,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
            pending_vlf_tail_jump: false,
            vlf_search_ranges: Vec::new(),
        };

        let mut view_to_idx = HashMap::new();
        view_to_idx.insert(view_id, 0);

        let mut mgr = Self {
            tx: to_core_tx,
            backend_rx,
            core_thread: Some(core_thread),
            reader_thread: Some(reader_thread),
            reader_shutdown,
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
            access_history: Vec::new(),
            modified_history: Vec::new(),
            next_buf_id: 2,
            next_rpc_id: 2,
            pending,
            pending_locations: Vec::new(),
            pending_symbols: Vec::new(),
            pending_ui_actions: Vec::new(),
            startup_profile: StartupProfile {
                new_view_rpc,
                init_notification_drain,
                ..StartupProfile::default()
            },
            startup_profile_active: true,
        };

        let init_apply_started = Instant::now();
        for event in init_events {
            mgr.apply_event_to_buffer(event)?;
        }
        mgr.startup_profile.init_event_apply = init_apply_started.elapsed();
        let pump_init_started = Instant::now();
        mgr.pump_init()?;
        mgr.startup_profile.pump_init = pump_init_started.elapsed();
        mgr.startup_profile_active = false;
        Ok(mgr)
    }

    #[cfg(test)]
    pub(crate) fn startup_profile(&self) -> &StartupProfile {
        &self.startup_profile
    }

    pub(crate) fn active(&self) -> &BufState {
        &self.bufs[self.current]
    }

    pub(crate) fn set_buffer_path(&mut self, id: BufferId, path: PathBuf) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|buf| buf.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let mtime = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
        let buf = &mut self.bufs[idx];
        buf.path = Some(path);
        buf.display_name = None;
        buf.mtime = mtime;
        buf.externally_modified = false;
        buf.editor_config_synced = false;
        Ok(())
    }

    /// Slice of all open buffers (for window/UI enumeration).
    pub(crate) fn all_bufs(&self) -> &[BufState] {
        &self.bufs
    }

    pub(crate) fn buf_count(&self) -> usize {
        self.bufs.len()
    }

    pub(crate) fn current_idx(&self) -> usize {
        self.current
    }

    // ── Connection methods ────────────────────────────────────────────────

    pub(crate) fn send_edit(&self, method: &str, params: Value) -> io::Result<()> {
        let view_id = &self.bufs[self.current].view_id;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": method,
                "params": params,
            }),
        )
    }

    fn send_request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id = self.next_rpc_id.saturating_add(1);

        let (resp_tx, resp_rx) = std_mpsc::sync_channel::<Value>(1);
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(rpc_id, resp_tx);
        }

        send_rpc_request(&self.tx, rpc_id, method, params)?;

        let response = resp_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{method} timed out")))?;
        parse_response(response)
    }

    pub(crate) fn save(&mut self) -> io::Result<()> {
        let id = self.bufs[self.current].id;
        self.save_buffer(id)
    }

    pub(crate) fn save_buffer(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|buf| buf.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        self.flush_view_edits(idx)?;
        let buf = &self.bufs[idx];
        let Some(path) = buf.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "scratch buffer has no path"));
        };
        let view_id = buf.view_id.clone();
        send_xi_notification(
            &self.tx,
            "save",
            json!({
                "view_id": view_id,
                "file_path": path.to_string_lossy().to_string(),
            }),
        )?;
        let baseline_generation = self.bufs[idx].last_save_generation;
        self.bufs[idx].save_complete = false;
        self.wait_for_buffer_save(id, &path, baseline_generation)?;
        let display = path.display().to_string();
        // Refresh mtime after the save so external-change detection stays accurate.
        let new_mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        let buf = &mut self.bufs[idx];
        buf.mtime = new_mtime;
        buf.externally_modified = false;
        buf.status_message = Some(format!("saved {display}"));
        // Remove crash-recovery artifact now that the file is persisted.
        if let Some(rp) = recovery_file_path(&path) {
            let _ = std::fs::remove_file(rp);
        }
        Ok(())
    }

    pub(crate) fn flush_all_pending_edits(&mut self) -> io::Result<()> {
        for idx in 0..self.bufs.len() {
            self.flush_view_edits(idx)?;
        }
        Ok(())
    }

    pub(crate) fn buffer_pristine(&mut self, id: BufferId) -> io::Result<bool> {
        let view_id = self
            .bufs
            .iter()
            .find(|buf| buf.id == id)
            .map(|buf| buf.view_id.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let response = self.send_request("buffer_pristine", json!({ "view_id": view_id }))?;
        response.as_bool().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "buffer_pristine returned non-bool")
        })
    }

    fn poll_buffer_save_status(&mut self, id: BufferId) -> io::Result<(u64, bool)> {
        let view_id = self
            .bufs
            .iter()
            .find(|buf| buf.id == id)
            .map(|buf| buf.view_id.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let response = self.send_request("save_status", json!({ "view_id": view_id }))?;
        let generation = response.get("generation").and_then(Value::as_u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "save_status missing generation")
        })?;
        let complete = response.get("complete").and_then(Value::as_bool).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "save_status missing complete")
        })?;
        Ok((generation, complete))
    }

    fn wait_for_buffer_save(
        &mut self,
        id: BufferId,
        path: &std::path::Path,
        baseline_generation: u64,
    ) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut target_generation = None;
        loop {
            self.sync_pending_events_for_whole_document()?;

            let idx = self
                .bufs
                .iter()
                .position(|buf| buf.id == id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
            if target_generation.is_none()
                && self.bufs[idx].last_save_generation > baseline_generation
            {
                target_generation = Some(self.bufs[idx].last_save_generation);
            }
            let (status_generation, status_complete) = self.poll_buffer_save_status(id)?;
            if target_generation.is_none() && status_generation > baseline_generation {
                target_generation = Some(status_generation);
            }
            if status_complete
                && target_generation.is_some_and(|generation| status_generation >= generation)
            {
                let idx =
                    self.bufs.iter().position(|buf| buf.id == id).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "buffer not found")
                    })?;
                self.bufs[idx].last_save_generation =
                    self.bufs[idx].last_save_generation.max(status_generation);
                self.bufs[idx].completed_save_generation =
                    self.bufs[idx].completed_save_generation.max(status_generation);
                self.bufs[idx].save_complete = true;
                return Ok(());
            }
            if target_generation
                .is_some_and(|generation| self.bufs[idx].completed_save_generation >= generation)
            {
                self.bufs[idx].save_complete = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("save timed out: {}", path.display()),
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    fn flush_view_edits(&mut self, idx: usize) -> io::Result<()> {
        let Some(view_id) = self.bufs.get(idx).map(|buf| buf.view_id.clone()) else {
            return Ok(());
        };
        let _ = self.send_request("selections_preview", json!({ "view_id": view_id }))?;
        self.sync_pending_events_for_whole_document()
    }

    pub(crate) fn reload_editor_config(&mut self) -> io::Result<()> {
        let (_, general_config, _) = crate::config::xi_config_tables_for_file(None);
        send_config_notification(&self.tx, json!("general"), general_config)?;
        send_lsp_config_notification(&self.tx, self.active().path.as_deref())?;
        for idx in 0..self.bufs.len() {
            self.sync_buffer_editor_config(idx)?;
        }
        Ok(())
    }

    pub(crate) fn request_completion(&mut self, index: Option<usize>) -> io::Result<()> {
        self.send_edit("request_completion", json!({ "index": index }))
    }

    pub(crate) fn request_definition(&mut self) -> io::Result<()> {
        self.send_edit("request_definition", json!({}))
    }

    pub(crate) fn request_declaration(&mut self) -> io::Result<()> {
        self.send_edit("request_declaration", json!({}))
    }

    pub(crate) fn request_type_definition(&mut self) -> io::Result<()> {
        self.send_edit("request_type_definition", json!({}))
    }

    pub(crate) fn request_references(&mut self) -> io::Result<()> {
        self.send_edit("request_references", json!({}))
    }

    pub(crate) fn request_implementation(&mut self) -> io::Result<()> {
        self.send_edit("request_implementation", json!({}))
    }

    pub(crate) fn request_document_symbols(&mut self) -> io::Result<()> {
        self.send_edit("request_document_symbols", json!({}))
    }

    pub(crate) fn request_workspace_symbols(&mut self, query: &str) -> io::Result<()> {
        self.send_edit("request_workspace_symbols", json!({ "query": query }))
    }

    pub(crate) fn format_document(&mut self) -> io::Result<()> {
        self.send_edit("format_document", json!({}))
    }

    pub(crate) fn request_code_actions(&mut self, index: Option<usize>) -> io::Result<()> {
        self.send_edit("request_code_actions", json!({ "index": index }))
    }

    pub(crate) fn request_rename(&mut self, new_name: &str) -> io::Result<()> {
        self.send_edit("request_rename", json!({ "new_name": new_name }))
    }

    pub(crate) fn stop_plugin(&mut self, plugin_name: &str) -> io::Result<()> {
        send_xi_notification(
            &self.tx,
            "plugin",
            json!({
                "command": "stop",
                "view_id": self.bufs[self.current].view_id,
                "plugin_name": plugin_name,
            }),
        )
    }

    pub(crate) fn restart_plugin(&mut self, plugin_name: &str) -> io::Result<()> {
        send_xi_notification(
            &self.tx,
            "plugin",
            json!({
                "command": "restart",
                "view_id": self.bufs[self.current].view_id,
                "plugin_name": plugin_name,
            }),
        )
    }

    pub(crate) fn delete_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
    ) -> io::Result<()> {
        self.send_edit(
            "delete_line_range",
            json!({
                "start_line": start_line,
                "end_line": end_line,
            }),
        )
    }

    pub(crate) fn goto_column(
        &mut self,
        display_col: usize,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "goto_column",
            json!({
                "display_col": display_col,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn add_newline_above(&mut self) -> io::Result<()> {
        self.send_edit("add_newline_above", json!({}))
    }

    pub(crate) fn add_newline_below(&mut self) -> io::Result<()> {
        self.send_edit("add_newline_below", json!({}))
    }

    pub(crate) fn join_selections(&mut self, select_space: bool) -> io::Result<()> {
        self.send_edit("join_selections", json!({ "select_space": select_space }))
    }

    pub(crate) fn extend_line_below(&mut self, count: usize) -> io::Result<()> {
        self.send_edit("extend_line_below", json!({ "count": count }))
    }

    pub(crate) fn extend_to_line_bounds(&mut self) -> io::Result<()> {
        self.send_edit("extend_to_line_bounds", json!({}))
    }

    pub(crate) fn shrink_to_line_bounds(&mut self) -> io::Result<()> {
        self.send_edit("shrink_to_line_bounds", json!({}))
    }

    pub(crate) fn move_word_start(
        &mut self,
        forward: bool,
        long_word: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "move_word_start",
            json!({
                "forward": forward,
                "long_word": long_word,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn move_word_end(
        &mut self,
        long_word: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "move_word_end",
            json!({
                "long_word": long_word,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn find_char(
        &mut self,
        target: char,
        forward: bool,
        inclusive: bool,
        modify_selection: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "find_char",
            json!({
                "target": target,
                "forward": forward,
                "inclusive": inclusive,
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn move_to_matching_bracket(&mut self, modify_selection: bool) -> io::Result<()> {
        self.send_edit(
            "move_to_matching_bracket",
            json!({
                "modify_selection": modify_selection,
            }),
        )
    }

    pub(crate) fn delete_block(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> io::Result<()> {
        self.send_edit(
            "delete_block",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "left_col": left_col,
                "right_col": right_col,
            }),
        )
    }

    pub(crate) fn replay_block_insert(
        &mut self,
        start_line: usize,
        end_line: usize,
        column: usize,
        text: &str,
        append: bool,
    ) -> io::Result<()> {
        self.send_edit(
            "replay_block_insert",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "column": column,
                "text": text,
                "append": append,
            }),
        )
    }

    pub(crate) fn substitute_preview(
        &mut self,
        start_line: usize,
        end_line: usize,
        pattern: &str,
        replacement: &str,
        global: bool,
        case_sensitive: bool,
    ) -> io::Result<Vec<LineReplacement>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "substitute_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
                "pattern": pattern,
                "replacement": replacement,
                "global": global,
                "case_sensitive": case_sensitive,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn filter_selections_preview(
        &mut self,
        pattern: &str,
        remove: bool,
    ) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "filter_selections_preview",
            json!({
                "view_id": view_id,
                "pattern": pattern,
                "remove": remove,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn selected_text_preview(&mut self, linewise: bool) -> io::Result<String> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "selected_text_preview",
            json!({
                "view_id": view_id,
                "linewise": linewise,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn selections_preview(&mut self) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request("selections_preview", json!({ "view_id": view_id }))?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn block_text_preview(
        &mut self,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> io::Result<String> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "block_text_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
                "left_col": left_col,
                "right_col": right_col,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn fold_ranges_preview(
        &mut self,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> io::Result<Vec<(usize, usize)>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "fold_ranges_preview",
            json!({
                "view_id": view_id,
                "start_line": start_line,
                "end_line": end_line,
            }),
        )?;
        let ranges: Vec<FoldRangePreview> = serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(ranges
            .into_iter()
            .filter(|range| range.body_end >= range.header_line)
            .map(|range| (range.header_line, range.body_end))
            .collect())
    }

    pub(crate) fn fold_range_at_cursor(&mut self) -> io::Result<Option<(usize, usize)>> {
        let line = self.active().cursor_line;
        Ok(self.fold_ranges_preview(Some(line), Some(line))?.into_iter().next())
    }

    pub(crate) fn select_chars_preview(&mut self, count: usize) -> io::Result<Vec<SelectionRange>> {
        let view_id = self.bufs[self.current].view_id.clone();
        let response = self.send_request(
            "select_chars_preview",
            json!({
                "view_id": view_id,
                "count": count,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub(crate) fn set_selections(&mut self, selections: &[SelectionRange]) -> io::Result<()> {
        self.send_edit("set_selections", json!({ "selections": selections }))
    }

    pub(crate) fn apply_line_replacements(
        &mut self,
        replacements: &[LineReplacement],
    ) -> io::Result<()> {
        self.send_edit("apply_line_replacements", json!({ "replacements": replacements }))
    }

    pub(crate) fn replace_line_range(
        &mut self,
        start_line: usize,
        end_line: usize,
        lines: &[String],
    ) -> io::Result<()> {
        self.send_edit(
            "replace_line_range",
            json!({
                "start_line": start_line,
                "end_line": end_line,
                "lines": lines,
            }),
        )
    }

    pub(crate) fn vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> io::Result<()> {
        self.send_edit(
            "vlf_replace_range",
            json!({
                "start_line": start_line,
                "start_col": start_col,
                "end_line": end_line,
                "end_col": end_col,
                "text": text,
            }),
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn paste_register(&mut self, chars: &str, before: bool) -> io::Result<()> {
        self.send_edit("paste_register", json!({ "chars": chars, "before": before }))
    }

    pub(crate) fn request_hover(&mut self, position: Option<(usize, usize)>) -> io::Result<()> {
        let view_id = &self.bufs[self.current].view_id;
        let request_id = usize::try_from(self.next_rpc_id).unwrap_or(usize::MAX);
        self.next_rpc_id = self.next_rpc_id.saturating_add(1);
        let position = position.map(|(line, column)| {
            json!({
                "line": line,
                "column": column,
            })
        });
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": "request_hover",
                "params": {
                    "request_id": request_id,
                    "position": position,
                },
            }),
        )
    }

    pub(crate) fn notify_scroll(&mut self, first_line: usize, last_line: usize) -> io::Result<()> {
        let range = (first_line, last_line);
        let buf = &mut self.bufs[self.current];
        if buf.view_id.is_empty() {
            return Ok(());
        }
        let view_id = buf.view_id.clone();

        if buf.is_vlf {
            if buf.pending_vlf_tail_jump {
                return Ok(());
            }
            let visible_lines = last_line.saturating_sub(first_line);
            let mut requested_last_line = if visible_lines >= Self::STARTUP_VLF_VIEWPORT_LINES {
                last_line
            } else {
                last_line
                    .saturating_add(Self::VLF_VIEWPORT_OVERSCAN_LINES)
                    .max(first_line.saturating_add(Self::STARTUP_VLF_VIEWPORT_LINES))
            };
            let line_count = buf.line_count();
            if line_count > first_line {
                requested_last_line = requested_last_line.min(line_count);
            }
            let request_range = (first_line, requested_last_line);
            buf.restore_previous_vlf_viewport_if_ready(first_line, requested_last_line);
            if vlf_viewport_ready(buf, first_line, requested_last_line)
                || (buf.last_scroll == Some(request_range) && buf.pending_line_request)
            {
                return Ok(());
            }
            buf.last_scroll = Some(request_range);
            // VLF mode: use the dedicated viewport protocol so the backend
            // only decodes the visible line range from disk.  Increment the
            // generation counter so any in-flight response from the previous
            // scroll position is discarded when it arrives.
            let generation = buf.vlf_generation.wrapping_add(1);
            buf.vlf_generation = generation;
            buf.pending_line_request = true;
            send_xi_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "vlf_viewport",
                    "params": {
                        "line_start": first_line as u64,
                        "line_end": requested_last_line as u64,
                        "generation": generation,
                    },
                }),
            )
        } else {
            if buf.last_scroll == Some(range) {
                return Ok(());
            }
            buf.last_scroll = Some(range);
            send_xi_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "scroll",
                    "params": [first_line, last_line],
                }),
            )
        }
    }

    pub(crate) fn force_vlf_viewport_refresh(
        &mut self,
        first_line: usize,
        last_line: usize,
    ) -> io::Result<()> {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf {
            return self.notify_scroll(first_line, last_line);
        }

        if buf.view_id.is_empty() {
            return Ok(());
        }
        let generation = buf.vlf_generation.wrapping_add(1);
        buf.vlf_generation = generation;
        buf.pending_line_request = true;
        buf.last_scroll = Some((first_line, last_line));
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": buf.view_id,
                "method": "vlf_viewport",
                "params": {
                    "line_start": first_line as u64,
                    "line_end": last_line as u64,
                    "generation": generation,
                },
            }),
        )
    }
    pub(crate) fn request_vlf_tail_viewport(&mut self, line_count: usize) -> io::Result<()> {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf || buf.view_id.is_empty() {
            return Ok(());
        }

        let requested_lines = line_count.max(Self::TAIL_VLF_PREFETCH_LINES);
        let generation = buf.vlf_generation.wrapping_add(1);
        buf.vlf_generation = generation;
        buf.pending_line_request = true;
        buf.pending_vlf_tail_jump = true;
        buf.last_scroll = None;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": buf.view_id,
                "method": "vlf_viewport",
                "params": {
                    "line_start": u64::MAX,
                    "line_end": requested_lines.saturating_sub(1) as u64,
                    "generation": generation,
                },
            }),
        )
    }

    pub(crate) fn cancel_vlf_tail_jump(&mut self) {
        let buf = &mut self.bufs[self.current];
        if !buf.is_vlf || !buf.pending_vlf_tail_jump {
            return;
        }

        buf.vlf_generation = buf.vlf_generation.wrapping_add(1);
        buf.pending_line_request = false;
        buf.pending_vlf_tail_jump = false;
        buf.last_scroll = None;
    }

    pub(crate) fn apply_local_vlf_replace_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: &str,
    ) -> bool {
        self.bufs[self.current]
            .apply_local_vlf_replace_range(start_line, start_col, end_line, end_col, text)
    }
    pub(crate) fn drain_events(&mut self) -> io::Result<()> {
        let mut events = Vec::new();
        while let Ok(event) = self.backend_rx.try_recv() {
            events.push(event);
        }
        for event in coalesce_backend_events(events) {
            self.apply_event_to_buffer(event)?;
        }
        Ok(())
    }

    pub(crate) fn pump_init(&mut self) -> io::Result<()> {
        let mut idle_rounds = 0;
        loop {
            if startup_render_ready(&self.bufs[self.current].line_cache) {
                break;
            }
            if self.bufs[self.current].is_vlf
                && self.bufs[self.current].last_scroll.is_none()
                && !self.bufs[self.current].pending_line_request
            {
                self.notify_scroll(0, Self::STARTUP_VLF_VIEWPORT_LINES)?;
            }
            if invalid_line_ranges(&self.bufs[self.current].line_cache).is_empty()
                && !self.current_buffer_may_receive_initial_content()
            {
                break;
            }
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(20)) {
                Some(event) => {
                    let mut saw_critical = event.is_startup_critical();
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        saw_critical |= event.is_startup_critical();
                        self.apply_event_to_buffer(event)?;
                    }

                    if saw_critical {
                        idle_rounds = 0;
                    } else {
                        idle_rounds += 1;
                        if idle_rounds >= Self::SYNC_IDLE_LIMIT {
                            break;
                        }
                    }
                }
                None => {
                    idle_rounds += 1;
                    if idle_rounds >= Self::SYNC_IDLE_LIMIT {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn current_buffer_may_receive_initial_content(&self) -> bool {
        let buf = &self.bufs[self.current];
        buf.pending_line_request
            || buf
                .path
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() > 0)
    }

    fn apply_event_to_buffer(&mut self, event: BackendEvent) -> io::Result<()> {
        let current = self.current;
        match event {
            BackendEvent::Update { ref view_id, .. }
            | BackendEvent::ScrollTo { ref view_id, .. } => {
                let Some(idx) = self.buffer_index_for_view(view_id) else {
                    return Ok(());
                };
                match event {
                    BackendEvent::Update { update, .. } => {
                        let update_started = Instant::now();
                        let update_pristine = update.pristine;
                        let buf = &mut self.bufs[idx];
                        let was_pristine = buf.pristine;
                        if buf.is_vlf {
                            buf.pending_line_request = false;
                            return Ok(());
                        }
                        buf.pending_line_request = false;
                        let stats = buf.apply_update(update)?;
                        if was_pristine && update_pristine {
                            self.finish_external_reload(idx);
                        }
                        if self.startup_profile_active {
                            self.startup_profile.update_apply += update_started.elapsed();
                            self.startup_profile.rebuild_lines += stats.rebuild_lines;
                        }
                    }
                    BackendEvent::ScrollTo { line, col, .. } => {
                        let buf = &mut self.bufs[idx];
                        buf.cursor_line = line;
                        buf.cursor_col = col;
                        buf.clamp_cursor();
                    }
                    _ => unreachable!(),
                }

                if !self.bufs[idx].editor_config_synced {
                    let config_sync_started = Instant::now();
                    self.sync_buffer_editor_config(idx)?;
                    if self.startup_profile_active {
                        self.startup_profile.config_sync += config_sync_started.elapsed();
                    }
                }
            }
            BackendEvent::Alert(msg) => {
                self.bufs[current].status_message = Some(msg);
            }
            BackendEvent::Hover { view_id, content } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.pending_ui_actions.push(PendingUiAction::Hover { view_id, content });
                self.bufs[idx].status_message = Some(String::from("hover ready"));
            }
            BackendEvent::Completions { view_id, items } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = items.len();
                self.pending_ui_actions.push(PendingUiAction::Completions { view_id, items });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no completions")
                } else {
                    format!("completions: {count}")
                });
            }
            BackendEvent::Locations { view_id, title, locations } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                // Collect for the App to dispatch to the quickfix list.
                self.pending_locations.push((view_id, title, locations));
            }
            BackendEvent::Symbols { view_id, title, symbols } => {
                if self.buffer_index_for_view(&view_id).is_none() {
                    return Ok(());
                }
                // Collect for the App to dispatch to the symbols picker.
                self.pending_symbols.push((view_id, title, symbols));
            }
            BackendEvent::Diagnostics { view_id, diagnostics } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = diagnostics.len();
                self.bufs[idx].diagnostics = diagnostics;
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("diagnostics cleared")
                } else {
                    format!("diagnostics: {count}")
                });
            }
            BackendEvent::CodeActions { view_id, actions } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                let count = actions.len();
                self.pending_ui_actions.push(PendingUiAction::CodeActions { view_id, actions });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no code actions")
                } else {
                    format!("code actions: {count}")
                });
            }
            BackendEvent::DocumentMode { view_id, is_vlf } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].is_vlf = is_vlf;
                if is_vlf {
                    self.bufs[idx].lines.clear();
                    self.bufs[idx].line_cache =
                        vec![LineSlot::Invalid; Self::STARTUP_VLF_VIEWPORT_LINES];
                    self.bufs[idx].vlf_cache_start_line = 0;
                    self.bufs[idx].pending_line_request = false;
                    self.bufs[idx].last_scroll = None;
                    self.bufs[idx].vlf_approx_line_count = 0;
                    self.bufs[idx].vlf_line_count_exact = false;
                    self.bufs[idx].pending_vlf_tail_jump = false;
                } else {
                    // Rebuild lines now that mode is set.
                    self.bufs[idx].rebuild_lines();
                }
            }
            BackendEvent::VlfChunks {
                view_id,
                generation,
                line_start,
                lines,
                syntax_spans,
                approximate_line_count,
                line_count_exact,
                index_progress,
            } => {
                let _ = index_progress;
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                if generation == self.bufs[idx].vlf_generation {
                    self.bufs[idx].pending_line_request = false;
                    self.bufs[idx].apply_vlf_chunks(VlfChunkUpdate {
                        generation,
                        line_start,
                        lines: &lines,
                        syntax_spans: &syntax_spans,
                        approximate_line_count,
                        line_count_exact,
                    });
                }
            }
            BackendEvent::VlfSearchStatus {
                view_id,
                query,
                scanned_bytes,
                total_bytes,
                complete,
                stored_match_count,
                ranges,
            } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].vlf_search_ranges = ranges.clone();
                let preview = ranges
                    .iter()
                    .take(3)
                    .map(|range| {
                        format!("L{}:{}-{}", range.line + 1, range.start_col + 1, range.end_col + 1)
                    })
                    .collect::<Vec<_>>();
                let progress = format!("{}/{} B", scanned_bytes, total_bytes);
                self.bufs[idx].status_message = Some(if preview.is_empty() {
                    format!(
                        "search {:?}: {} matches, {}{}",
                        query,
                        stored_match_count,
                        progress,
                        if complete { " complete" } else { " scanning" }
                    )
                } else {
                    format!(
                        "search {:?}: {} matches, {}, {}{}",
                        query,
                        stored_match_count,
                        progress,
                        preview.join(", "),
                        if complete { " complete" } else { " scanning" }
                    )
                });
            }
            BackendEvent::SaveProgress { view_id, complete, generation } => {
                let Some(idx) = self.buffer_index_for_view(&view_id) else {
                    return Ok(());
                };
                self.bufs[idx].last_save_generation =
                    self.bufs[idx].last_save_generation.max(generation);
                if complete {
                    self.bufs[idx].completed_save_generation =
                        self.bufs[idx].completed_save_generation.max(generation);
                    self.bufs[idx].save_complete = true;
                }
            }
        }
        Ok(())
    }

    fn finish_external_reload(&mut self, idx: usize) {
        let Some(buf) = self.bufs.get_mut(idx) else {
            return;
        };
        if !buf.externally_modified {
            return;
        }
        let Some(path) = buf.path.as_ref() else {
            return;
        };
        let Some(mtime) = std::fs::metadata(path).ok().and_then(|meta| meta.modified().ok()) else {
            return;
        };
        buf.mtime = Some(mtime);
        buf.externally_modified = false;
        buf.status_message = Some(String::from("reloaded"));
    }

    fn request_invalid_lines(&mut self, idx: usize, scope: LineRequestScope) -> io::Result<()> {
        let Some(buf) = self.bufs.get_mut(idx) else { return Ok(()) };
        if buf.is_vlf || buf.pending_line_request || buf.view_id.is_empty() {
            return Ok(());
        }

        let invalid_ranges = match scope {
            LineRequestScope::WholeDocument => invalid_line_ranges(&buf.line_cache),
            LineRequestScope::Viewport => {
                let (start, end) = bounded_line_request_window(buf);
                invalid_line_ranges_bounded(&buf.line_cache, start, end)
            }
        };
        if invalid_ranges.is_empty() {
            return Ok(());
        }

        let view_id = buf.view_id.clone();
        for (start, end) in invalid_ranges {
            send_rpc_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "request_lines",
                    "params": [start, end],
                }),
            )?;
        }
        buf.pending_line_request = true;
        Ok(())
    }

    fn has_pending_line_work(&self, scope: LineRequestScope) -> bool {
        self.bufs.iter().any(|buf| {
            if buf.is_vlf || buf.pending_line_request {
                return buf.pending_line_request;
            }
            match scope {
                LineRequestScope::Viewport => {
                    let (start, end) = bounded_line_request_window(buf);
                    !invalid_line_ranges_bounded(&buf.line_cache, start, end).is_empty()
                }
                LineRequestScope::WholeDocument => !invalid_line_ranges(&buf.line_cache).is_empty(),
            }
        })
    }

    fn request_all_invalid_lines(&mut self, scope: LineRequestScope) -> io::Result<()> {
        for idx in 0..self.bufs.len() {
            self.request_invalid_lines(idx, scope)?;
        }
        Ok(())
    }

    // ── Multi-buffer management ───────────────────────────────────────────

    /// Open a new xi view for `path` (or scratch) and add it as an inactive
    /// buffer.  Returns the new buffer's [`BufferId`].
    pub(crate) fn open_buffer(&mut self, path: Option<PathBuf>) -> io::Result<BufferId> {
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id += 1;

        send_lsp_config_notification(&self.tx, path.as_deref())?;

        // Register a one-shot channel so the reader thread can hand us the
        // view_id response without blocking the reader loop.
        let (resp_tx, resp_rx) = std_mpsc::sync_channel::<Value>(1);
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(rpc_id, resp_tx);
        }

        send_rpc_request(
            &self.tx,
            rpc_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let response = resp_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "new_view timed out"))?;
        let view_id = parse_response(response)?
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let buf_id = self.next_buf_id;
        self.next_buf_id += 1;
        let idx = self.bufs.len();

        self.view_to_idx.insert(view_id.clone(), idx);
        let mtime =
            path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        self.bufs.push(BufState {
            id: buf_id,
            path,
            display_name: None,
            view_id,
            editor_config_synced: false,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            save_complete: true,
            last_save_generation: 0,
            completed_save_generation: 0,
            status_message: None,
            last_scroll: None,
            mtime,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_cache_start_line: 0,
            vlf_previous_viewport: None,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
            pending_vlf_tail_jump: false,
            vlf_search_ranges: Vec::new(),
        });
        Ok(buf_id)
    }

    pub(crate) fn open_named_scratch_buffer(
        &mut self,
        title: impl Into<String>,
    ) -> io::Result<BufferId> {
        let buf_id = self.open_buffer(None)?;
        if let Some(buf) = self.bufs.iter_mut().find(|buf| buf.id == buf_id) {
            buf.display_name = Some(title.into());
        }
        Ok(buf_id)
    }

    /// Close a buffer.  Fails if it would leave no open buffers.
    pub(crate) fn close_buffer(&mut self, id: BufferId) -> io::Result<()> {
        if self.bufs.len() <= 1 {
            return Err(io::Error::other("cannot close last buffer"));
        }
        let pos = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;

        let view_id = self.bufs[pos].view_id.clone();
        let _ = send_xi_notification(&self.tx, "close_view", json!({ "view_id": view_id }));

        self.bufs.remove(pos);

        // Rebuild index map since positions shifted.
        self.view_to_idx.clear();
        for (i, b) in self.bufs.iter().enumerate() {
            self.view_to_idx.insert(b.view_id.clone(), i);
        }

        // Adjust current.
        if self.bufs.is_empty() {
            self.current = 0; // unreachable, guarded above
        } else if self.current >= self.bufs.len() {
            self.current = self.bufs.len() - 1;
        } else if self.current > pos {
            self.current -= 1;
        } else if self.current == pos {
            self.current = pos.saturating_sub(1);
        }

        // Adjust alternate.
        if let Some(alt) = self.alternate {
            if alt == pos {
                self.alternate = None;
            } else if alt > pos {
                self.alternate = Some(alt - 1);
            }
        }
        self.access_history.retain(|candidate| *candidate != id);
        self.modified_history.retain(|candidate| *candidate != id);
        Ok(())
    }

    /// Switch to buffer at list index `idx`, saving current as alternate.
    pub(crate) fn switch_to_idx(&mut self, idx: usize) {
        if idx < self.bufs.len() && idx != self.current {
            self.access_history.push(self.bufs[self.current].id);
            self.alternate = Some(self.current);
            self.current = idx;
        }
    }

    /// Switch to buffer by [`BufferId`].
    pub(crate) fn switch_to_id(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        self.switch_to_idx(idx);
        Ok(())
    }

    /// Switch to the alternate buffer (`:b#` / Ctrl-^).
    pub(crate) fn switch_alternate(&mut self) -> io::Result<()> {
        let alt = self.alternate.ok_or_else(|| io::Error::other("no alternate buffer"))?;
        self.switch_to_idx(alt);
        Ok(())
    }

    pub(crate) fn switch_last_accessed(&mut self) -> io::Result<()> {
        while let Some(id) = self.access_history.pop() {
            let Some(idx) = self.bufs.iter().position(|buf| buf.id == id) else {
                continue;
            };
            if idx == self.current {
                continue;
            }
            self.switch_to_idx(idx);
            return Ok(());
        }
        Err(io::Error::other("no last accessed buffer"))
    }

    pub(crate) fn switch_last_modified(&mut self) -> io::Result<()> {
        let current_id = self.bufs[self.current].id;
        let Some(id) =
            self.modified_history.iter().copied().find(|candidate| *candidate != current_id)
        else {
            return Err(io::Error::other("no last modified buffer"));
        };
        self.switch_to_id(id)
    }

    pub(crate) fn note_buffer_modified(&mut self, id: BufferId) {
        if self.bufs.iter().all(|buf| buf.id != id) {
            return;
        }
        self.modified_history.retain(|candidate| *candidate != id);
        self.modified_history.insert(0, id);
    }

    pub(crate) fn restore_cursor(
        &mut self,
        id: BufferId,
        line: usize,
        col: usize,
    ) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let view_id = self.bufs[idx].view_id.clone();
        self.bufs[idx].cursor_line = line;
        self.bufs[idx].cursor_col = col;
        send_xi_notification(
            &self.tx,
            "edit",
            json!({
                "view_id": view_id,
                "method": "gesture",
                "params": {
                    "line": line as u64,
                    "col": col as u64,
                    "ty": "point_select",
                },
            }),
        )
    }

    /// Cycle to the next buffer (wrapping).
    pub(crate) fn next_buffer(&mut self) {
        if self.bufs.len() > 1 {
            let next = (self.current + 1) % self.bufs.len();
            self.switch_to_idx(next);
        }
    }

    /// Cycle to the previous buffer (wrapping).
    pub(crate) fn prev_buffer(&mut self) {
        if self.bufs.len() > 1 {
            let prev = if self.current == 0 { self.bufs.len() - 1 } else { self.current - 1 };
            self.switch_to_idx(prev);
        }
    }

    /// Build a `:ls`-style buffer list string.
    pub(crate) fn list_buffers_str(&self) -> String {
        self.bufs
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let flag = if i == self.current {
                    "%"
                } else if self.alternate == Some(i) {
                    "#"
                } else {
                    " "
                };
                let modified = if b.pristine { " " } else { "+" };
                format!("{flag}{modified} {} {}", b.id, b.title())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn sync_pending_events(&mut self) -> io::Result<()> {
        self.sync_pending_events_with_scope(LineRequestScope::Viewport)
    }

    pub(crate) fn sync_pending_events_for_whole_document(&mut self) -> io::Result<()> {
        self.sync_pending_events_with_scope(LineRequestScope::WholeDocument)
    }

    fn sync_pending_events_with_scope(&mut self, scope: LineRequestScope) -> io::Result<()> {
        let mut idle_rounds = 0;
        for _ in 0..24 {
            self.request_all_invalid_lines(scope)?;
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(10)) {
                Some(event) => {
                    idle_rounds = 0;
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
                    }
                }
                None if self.has_pending_line_work(scope) => continue,
                None => {
                    idle_rounds += 1;
                    if idle_rounds >= Self::SYNC_IDLE_LIMIT {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pump(&mut self) -> io::Result<()> {
        self.sync_pending_events()
    }

    /// Repeatedly drain pending events until `predicate` holds for the active
    /// buffer, or the 2-second safety deadline expires.
    ///
    /// Use this instead of bare `pump()` when a test must wait for xi-core to
    /// finish processing a batch of keystrokes before inspecting state.
    #[cfg(test)]
    pub(crate) fn pump_until<F>(&mut self, predicate: F) -> io::Result<()>
    where
        F: Fn(&BufState) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            self.sync_pending_events()?;
            if predicate(self.active()) || Instant::now() >= deadline {
                break;
            }
        }
        Ok(())
    }

    /// Test-only constructor that builds a minimal `BufferManager` around
    /// pre-existing channel ends and a known view_id.
    #[cfg(test)]
    pub(crate) fn test_new(
        tx: std_mpsc::Sender<String>,
        backend_rx: std_mpsc::Receiver<BackendEvent>,
        view_id: String,
    ) -> Self {
        let (internal_tx, mut internal_rx) = mpsc::channel::<String>(64);
        thread::spawn(move || {
            while let Some(message) = internal_rx.blocking_recv() {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        let buf = BufState {
            id: 1,
            path: None,
            display_name: None,
            view_id: view_id.clone(),
            editor_config_synced: true,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            save_complete: true,
            last_save_generation: 0,
            completed_save_generation: 0,
            status_message: None,
            last_scroll: None,
            mtime: None,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_cache_start_line: 0,
            vlf_previous_viewport: None,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
            pending_vlf_tail_jump: false,
            vlf_search_ranges: Vec::new(),
        };
        let mut view_to_idx = HashMap::new();
        view_to_idx.insert(view_id, 0);
        Self {
            tx: internal_tx,
            backend_rx,
            core_thread: None,
            reader_thread: None,
            reader_shutdown: Arc::new(AtomicBool::new(false)),
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
            access_history: Vec::new(),
            modified_history: Vec::new(),
            next_buf_id: 2,
            next_rpc_id: 2,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_locations: Vec::new(),
            pending_symbols: Vec::new(),
            pending_ui_actions: Vec::new(),
            startup_profile: StartupProfile::default(),
            startup_profile_active: false,
        }
    }

    // ── External change detection ─────────────────────────────────────────

    /// Drain accumulated location results for App-level dispatch.
    pub(crate) fn drain_pending_locations(
        &mut self,
    ) -> Vec<(String, String, Vec<NavigationTarget>)> {
        std::mem::take(&mut self.pending_locations)
    }

    pub(crate) fn drain_pending_symbols(&mut self) -> Vec<(String, String, Vec<SymbolItem>)> {
        std::mem::take(&mut self.pending_symbols)
    }

    pub(crate) fn drain_pending_ui_actions(&mut self) -> Vec<PendingUiAction> {
        std::mem::take(&mut self.pending_ui_actions)
    }

    /// Check all buffers for filesystem changes since last open/save.
    /// Sets `BufState::externally_modified` when a newer mtime is detected.
    pub(crate) fn check_external_changes(&mut self) {
        for buf in &mut self.bufs {
            let Some(path) = &buf.path else { continue };
            if buf.externally_modified {
                continue; // already notified
            }
            let current_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
            if let (Some(stored), Some(current)) = (buf.mtime, current_mtime)
                && current > stored
            {
                buf.externally_modified = true;
            }
        }
    }

    /// Reload the buffer identified by `id` from its backing file, discarding
    /// local edits.  Closes the current xi view and opens a fresh one.
    pub(crate) fn reload_buffer(&mut self, id: BufferId) -> io::Result<()> {
        let idx = self
            .bufs
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer not found"))?;
        let path = self.bufs[idx].path.clone();
        let old_view_id = self.bufs[idx].view_id.clone();

        // Close the old xi view.
        let _ = send_xi_notification(&self.tx, "close_view", json!({ "view_id": old_view_id }));
        self.view_to_idx.remove(&old_view_id);

        // Open a new xi view for the same path.
        send_lsp_config_notification(&self.tx, path.as_deref())?;
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id += 1;

        let (resp_tx, resp_rx) = std_mpsc::sync_channel::<Value>(1);
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(rpc_id, resp_tx);
        }

        send_rpc_request(
            &self.tx,
            rpc_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let response = resp_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "new_view timed out"))?;
        let new_view_id = parse_response(response)?
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let mtime =
            path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());

        let buf = &mut self.bufs[idx];
        buf.view_id = new_view_id.clone();
        buf.display_name = None;
        buf.editor_config_synced = false;
        buf.line_cache = Vec::new();
        buf.lines = Vec::new();
        buf.cursor_line = 0;
        buf.cursor_col = 0;
        buf.pristine = true;
        buf.pending_line_request = false;
        buf.last_scroll = None;
        buf.is_vlf = false;
        buf.vlf_cache_start_line = 0;
        buf.vlf_generation = 0;
        buf.vlf_approx_line_count = 0;
        buf.vlf_line_count_exact = false;
        buf.pending_vlf_tail_jump = false;
        buf.vlf_search_ranges.clear();
        buf.status_message = Some("reloaded".to_owned());
        buf.mtime = mtime;
        buf.externally_modified = false;

        self.view_to_idx.insert(new_view_id, idx);
        Ok(())
    }
}

fn bounded_line_request_window(buf: &BufState) -> (usize, usize) {
    if buf.line_cache.is_empty() {
        return (0, 0);
    }
    let (start, end) = buf.last_scroll.unwrap_or((0, NORMAL_INVALID_LINE_DEFAULT_WINDOW));
    let start = start.saturating_sub(NORMAL_INVALID_LINE_OVERSCAN);
    let end = end.saturating_add(NORMAL_INVALID_LINE_OVERSCAN).min(buf.line_cache.len());
    (start, end.max(start))
}

// ── Recovery helpers ──────────────────────────────────────────────────────────

/// Compute the crash-recovery file path for `original`.
///
/// Recovery files are stored under `{data_dir}/ee/recovery/` with the original
/// path encoded by replacing `/` with `%2F` so the whole path becomes a single
/// filename component.  Returns `None` when the platform data directory cannot
/// be determined.
pub(crate) fn recovery_file_path(original: &std::path::Path) -> Option<PathBuf> {
    let data_dir = dirs::data_dir()?;
    let recovery_dir = data_dir.join("ee").join("recovery");
    let name = original.to_string_lossy().replace('/', "%2F").replace('\\', "%5C");
    Some(recovery_dir.join(name))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn send_xi_notification(tx: &mpsc::Sender<String>, method: &str, params: Value) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    tx.blocking_send(raw).map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

fn send_config_notification(
    tx: &mpsc::Sender<String>,
    domain: Value,
    changes: Table,
) -> io::Result<()> {
    send_rpc_notification(
        tx,
        "set_config",
        json!({
            "domain": domain,
            "changes": changes,
        }),
    )
}

fn send_lsp_config_notification(tx: &mpsc::Sender<String>, path: Option<&Path>) -> io::Result<()> {
    send_config_notification(
        tx,
        json!({ "plugin": crate::config::LSP_PLUGIN_NAME }),
        crate::config::lsp_config_table_for_file(path),
    )
}

impl BufferManager {
    fn sync_buffer_editor_config(&mut self, idx: usize) -> io::Result<()> {
        let Some(buf) = self.bufs.get_mut(idx) else {
            return Ok(());
        };
        let (_, _, overrides) = crate::config::xi_config_tables_for_file(buf.path.as_deref());
        send_config_notification(&self.tx, json!({ "user_override": buf.view_id }), overrides)?;
        buf.editor_config_synced = true;
        Ok(())
    }
}

// Keep these imports satisfied for the test helper.
#[allow(dead_code)]
fn _use_normalize(text: Option<String>) -> String {
    normalize_line_text(text)
}
#[allow(dead_code)]
fn _use_nav(_: &NavigationTarget) {}
