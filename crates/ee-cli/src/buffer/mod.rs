pub(crate) use std::collections::HashMap;
pub(crate) use std::io;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::atomic::{AtomicBool, Ordering};
pub(crate) use std::sync::mpsc as std_mpsc;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::thread;
pub(crate) use std::thread::JoinHandle;
pub(crate) use std::time::{Duration, Instant, SystemTime};

pub(crate) use serde_json::{Value, json};
pub(crate) use tokio::sync::mpsc;
pub(crate) use xi_core_lib::XiCore;
pub(crate) use xi_core_lib::config::Table;
pub(crate) use xi_core_lib::plugin_rpc::{Diagnostic, SelectionRange, SymbolItem};
pub(crate) use xi_core_lib::plugins::rpc::ClientPluginInfo;
pub(crate) use xi_core_lib::rpc::{FoldRangePreview, LineReplacement};
pub(crate) use xi_rpc::RpcLoop;

pub(crate) use crate::backend::{
    BackendEvent, CachedLine, ChannelReader, ChannelWriter, CoreAnnotation, CoreSyntaxSpan,
    CoreUpdate, CoreUpdateKind, CoreUpdateOp, LineSlot, NavigationTarget, PendingRequests,
    PendingUiAction, VlfSearchRange, block_for_response, checked_advance, coalesce_backend_events,
    drain_sync_notifications, invalid_line_ranges, invalid_line_ranges_bounded,
    normalize_line_text, parse_response, recv_with_timeout, send_rpc_notification,
    send_rpc_request, startup_render_ready, xi_reader_thread,
};
pub(crate) use crate::text::previous_char_boundary;

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
    pub(crate) last_save_result_generation: u64,
    pub(crate) last_save_succeeded: bool,
    pub(crate) last_save_permission_denied: bool,
    pub(crate) last_save_error_message: Option<String>,
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
    /// Last approximate total line count reported by a VLF `update` payload.
    /// Used for reported document size while the background index is still
    /// scanning the file.
    pub(crate) vlf_approx_line_count: u64,
    /// True when `vlf_approx_line_count` is backend-confirmed exact.
    pub(crate) vlf_line_count_exact: bool,
    /// Page-index scan fraction (0..1) reported by the core render; 1.0 when
    /// every page is scanned and exact base-index lookups are available.
    pub(crate) vlf_index_progress: f64,
    /// True when the next matching VLF window update should move the cursor to
    /// the returned tail (goto-end with an inexact line count).
    pub(crate) pending_vlf_tail_jump: bool,
    /// Viewport height the pending tail jump was issued with; consumed when
    /// the tail window lands to preload the page above the tail.
    pub(crate) vlf_tail_jump_viewport: Option<usize>,
    /// Last viewport range the UI asked the core to render. Unlike
    /// `last_scroll` (overwritten with the landed window's range for dedupe),
    /// this is the actual visible range used for coverage repairs.
    pub(crate) vlf_requested_viewport: Option<(usize, usize)>,
    /// Backend-authoritative visible VLF match ranges for current search.
    pub(crate) vlf_search_ranges: Vec<VlfSearchRange>,
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
            && self.vlf_approx_line_count == other.vlf_approx_line_count
            && self.vlf_line_count_exact == other.vlf_line_count_exact
            && self.vlf_index_progress == other.vlf_index_progress
            && self.pending_vlf_tail_jump == other.pending_vlf_tail_jump
            && self.vlf_tail_jump_viewport == other.vlf_tail_jump_viewport
            && self.vlf_requested_viewport == other.vlf_requested_viewport
            && self.vlf_search_ranges == other.vlf_search_ranges
    }
}

impl Eq for BufState {}

pub(crate) fn line_text_for_slot(slot: &LineSlot) -> String {
    match slot {
        LineSlot::Known(line) => line.text.clone(),
        LineSlot::Invalid => String::new(),
    }
}

/// Extract the VLF window from an `update` op stream, positioned as a dense
/// cache slice over absolute logical lines.
///
/// Inserted rows carry absolute `ln` fields; `copy`/`update`/`skip` ops
/// reference rows the frontend already holds (the core shadow is
/// authoritative), so copied rows are re-slotted from the previous window at
/// their absolute positions. Gaps (unreferenced lines) stay `Invalid`.
///
/// Returns `None` when the update carries no inserted lines (copy/skip-only:
/// the window is preserved, only cursors/annotations refresh). Shared by
/// `BufState` and the `XiClient` active-view mirror.
pub(crate) fn vlf_window_from_ops(
    ops: Vec<CoreUpdateOp>,
    old: &[LineSlot],
    old_start: usize,
) -> Option<(usize, usize, Vec<LineSlot>)> {
    // Rows kept on each side of the freshly rendered insert span. Bounds the
    // window so long scroll sessions cannot grow it toward the whole file
    // (copies re-emit rows the client already holds, and a document-wide
    // window makes every later apply, repair, and teardown O(document)).
    const WINDOW_OVERSCAN: usize = 512;
    // Collect positioned rows in view order. Wrapped VLF inserts carry `ln`
    // only on logical-line heads (continuation rows omit it), so wrapped
    // windows must be slotted as dense display rows — a naive ln fallback
    // would collide the continuation with the next logical line's head and
    // drop rows.
    // Cheap pre-scan: copy/skip/invalidate-only updates re-assert the current
    // window (the core's "nothing changed" shortcut emits a whole-document
    // copy sized by the *approximate* line count, which can be far larger than
    // the real one), so bail before materializing any rows.
    if !ops.iter().any(|op| op.op == CoreUpdateKind::Insert) {
        return None;
    }
    let mut rows: Vec<(Option<usize>, LineSlot)> = Vec::new();
    let mut source_index: usize = 0; // cursor into the previous window for copy ops
    let mut insert_min: Option<usize> = None;
    let mut insert_max: Option<usize> = None;
    let mut insert_count = 0usize;
    for op in ops {
        match op.op {
            CoreUpdateKind::Insert => {
                for line in op.lines {
                    if let Some(ln) = line.logical_line {
                        insert_min = Some(insert_min.map_or(ln, |min| min.min(ln)));
                        insert_max = Some(insert_max.map_or(ln, |max| max.max(ln)));
                    }
                    insert_count += 1;
                    rows.push((line.logical_line, LineSlot::from(line)));
                }
            }
            CoreUpdateKind::Skip => {
                source_index = source_index.saturating_add(op.n);
            }
            // Copy/update re-emit rows the client already has. The rope
            // frontend positions them from `source_index` over a full-document
            // cache; the bounded VLF window instead treats them as the
            // contiguous continuation of the already-positioned rows (the
            // core's plan segments are contiguous in view order), falling back
            // to the previous window's absolute positions when nothing has
            // been positioned yet. Rows that fall outside the previous
            // window's extent carry no usable content (the core's copy sizes
            // are sized by the *approximate* line count, which can dwarf the
            // real one) — stop there so a stale copy cannot balloon the
            // bounded window.
            CoreUpdateKind::Copy | CoreUpdateKind::Update => {
                for k in 0..op.n {
                    let idx = source_index.saturating_add(k);
                    let pos = rows
                        .last()
                        .and_then(|(ln, _)| *ln)
                        .map(|prev| prev.saturating_add(1))
                        .unwrap_or_else(|| old_start.saturating_add(idx));
                    let Some(rel) = pos.checked_sub(old_start) else { break };
                    let Some(slot) = old.get(rel) else { break };
                    rows.push((Some(pos), slot.clone()));
                }
                source_index = source_index.saturating_add(op.n);
            }
            // Invalidate spans lie outside the visible window (discard
            // regions of the plan); dropping them keeps the window bounded.
            CoreUpdateKind::Invalidate => {}
        }
    }
    // Copy/skip/invalidate-only updates re-assert the current window and are
    // handled by the pre-scan above.
    if rows.is_empty() {
        return None;
    }
    if rows.iter().all(|(ln, _)| ln.is_some()) {
        // Unwrapped: every row is a logical line; position by absolute `ln`
        // (gaps between multiple inserts stay Invalid). Trim to the insert
        // span ± overscan so copied rows cannot grow the window without bound.
        let trimmed = match (insert_min, insert_max) {
            (Some(min), Some(max)) => {
                let keep_start = min.saturating_sub(WINDOW_OVERSCAN);
                let keep_end = max.saturating_add(1).saturating_add(WINDOW_OVERSCAN);
                rows.retain(|(ln, _)| ln.is_some_and(|ln| ln >= keep_start && ln < keep_end));
                true
            }
            _ => false,
        };
        if trimmed && rows.is_empty() {
            return None;
        }
        let mut window: Vec<(usize, LineSlot)> = Vec::with_capacity(rows.len());
        for (ln, slot) in rows {
            window.push((ln.expect("checked above"), slot));
        }
        window.sort_by_key(|(ln, _)| *ln);
        window.dedup_by_key(|(ln, _)| *ln);
        let start = window.first().expect("non-empty window").0;
        let end = window.last().expect("non-empty window").0 + 1;
        let mut cache = vec![LineSlot::Invalid; end - start];
        for (ln, slot) in window {
            cache[ln - start] = slot;
        }
        Some((start, end, cache))
    } else {
        // Wrapped: dense display rows anchored at the window's first head.
        // Bound the dense window by the insert rows plus overscan on each
        // side, keeping the newest rows so the freshly rendered span survives.
        let max_rows = insert_count.saturating_add(WINDOW_OVERSCAN.saturating_mul(2));
        if rows.len() > max_rows {
            let drop = rows.len() - max_rows;
            rows.drain(..drop);
        }
        let start = rows.iter().find_map(|(ln, _)| *ln).unwrap_or(0);
        let end = start + rows.len();
        Some((start, end, rows.into_iter().map(|(_, slot)| slot).collect()))
    }
}
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
    /// Last viewport size pushed to the backend, re-sent when the active view
    /// changes so word wrap uses the real width in every view.
    pub(crate) last_resize: Option<(f64, f64)>,
    /// Applied `update` notifications (core-pushed renders). Monotone counter
    /// used by the §4 render benchmarks to measure per-render latency without
    /// polling for content (empty `Pending` windows never fill slots).
    pub(crate) render_updates: u64,
    next_buf_id: BufferId,
    next_rpc_id: u64,
    /// Pending synchronous RPC responses keyed by request id.
    pending: PendingRequests,
    /// Locations reported by the backend (definition, references, …) awaiting
    /// dispatch to the App-level quickfix list.
    pub(crate) pending_locations: Vec<(String, String, Vec<NavigationTarget>)>,
    /// Symbol results awaiting dispatch to the App-level picker.
    pub(crate) pending_symbols: Vec<(String, String, Vec<SymbolItem>)>,
    /// Generic LSP agent-tool results awaiting proxy/tool consumers.
    pub(crate) pending_agent_tool_results: Vec<(String, String, Value)>,
    available_plugins_by_view: HashMap<String, Vec<ClientPluginInfo>>,
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
        if let Err(error) =
            crate::config::configure_runtime_loader_for_file(buf.path.as_deref(), true)
        {
            eprintln!("ee: warning: failed to configure runtime languages: {error}");
        }
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

mod buffers;
mod bufstate;
mod construction;
mod edits;
mod events;
mod pump;
mod requests;
mod vlf;
