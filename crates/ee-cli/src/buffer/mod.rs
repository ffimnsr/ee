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
    CoreUpdate, CoreUpdateKind, LineSlot, NavigationTarget, PendingRequests, PendingUiAction,
    VlfSearchRange, block_for_response, checked_advance, coalesce_backend_events,
    drain_sync_notifications, invalid_line_ranges, invalid_line_ranges_bounded,
    normalize_line_text, parse_response, recv_with_timeout, send_rpc_notification,
    send_rpc_request, startup_render_ready, xi_reader_thread,
};
pub(crate) use crate::text::previous_char_boundary;
pub(crate) use crate::vlf_viewport::{VlfViewportRequest, VlfViewportScheduler};

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
    /// Generic LSP agent-tool results awaiting proxy/tool consumers.
    pub(crate) pending_agent_tool_results: Vec<(String, String, Value)>,
    available_plugins_by_view: HashMap<String, Vec<ClientPluginInfo>>,
    pub(crate) pending_ui_actions: Vec<PendingUiAction>,
    startup_profile: StartupProfile,
    startup_profile_active: bool,
    vlf_viewports: VlfViewportScheduler,
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
