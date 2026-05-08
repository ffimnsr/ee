use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use xi_core_lib::XiCore;
use xi_core_lib::config::Table;
use xi_core_lib::plugin_rpc::{Diagnostic, SelectionRange, SymbolItem};
use xi_core_lib::rpc::LineReplacement;
use xi_rpc::RpcLoop;

use crate::backend::{
    BackendEvent, CachedLine, ChannelReader, ChannelWriter, CoreAnnotation, CoreUpdate,
    CoreUpdateKind, LineSlot, NavigationTarget, PendingRequests, PendingUiAction,
    block_for_response, checked_advance, drain_sync_notifications, invalid_line_ranges,
    normalize_line_text, parse_response, recv_with_timeout, send_rpc_notification,
    send_rpc_request, xi_reader_thread,
};
use crate::text::previous_char_boundary;

pub(crate) type BufferId = u32;

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
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_line: usize,
    pub(crate) cursor_col: usize,
    pub(crate) pristine: bool,
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
    /// Monotone counter incremented on every VLF viewport scroll.
    ///
    /// Each `vlf_viewport` request carries this counter; `vlf_chunks` responses
    /// with a different generation are discarded so stale out-of-order data
    /// never overwrites a newer scroll position in the line cache.
    pub(crate) vlf_generation: u64,
    /// Last approximate total line count reported by a `vlf_chunks` response.
    /// Used to pre-size the line cache while the background index is still
    /// scanning the file.
    pub(crate) vlf_approx_line_count: u64,
}

impl BufState {
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

    pub(crate) fn apply_update(&mut self, update: CoreUpdate) -> io::Result<()> {
        let CoreUpdate { ops, pristine, annotations } = update;
        let previous = std::mem::take(&mut self.line_cache);
        let mut next_cache = Vec::new();
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
                    next_cache.extend(op.lines.into_iter().map(LineSlot::from));
                }
                CoreUpdateKind::Skip => {
                    source_index = checked_advance(source_index, op.n, previous.len(), "skip")?;
                }
                CoreUpdateKind::Invalidate => {
                    next_cache.extend(std::iter::repeat_n(LineSlot::Invalid, op.n));
                }
                CoreUpdateKind::Copy => {
                    let end = checked_advance(source_index, op.n, previous.len(), "copy")?;
                    // Clear cursor data from copied slots: Copy op means content is
                    // unchanged from xi-core's perspective, but cursor positions may
                    // have moved. Only Insert/Update ops carry authoritative cursor data.
                    for slot in &previous[source_index..end] {
                        match slot.clone() {
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
                    for (slot, line) in
                        previous[source_index..end].iter().cloned().zip(op.lines.into_iter())
                    {
                        next_cache.push(slot.merge(line)?);
                    }
                    source_index = end;
                }
            }
        }

        self.line_cache = next_cache;
        self.rebuild_lines();
        self.sync_cursor_from_cache();
        Ok(())
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
                self.cursor_line = line_index;
                self.cursor_col = previous_char_boundary(&line.text, cursor_col);
                self.clamp_cursor();
                return;
            }
        }
        self.clamp_cursor();
    }

    pub(crate) fn clamp_cursor(&mut self) {
        if self.is_vlf {
            // In VLF mode `lines` is empty; clamp against the cache length.
            self.cursor_line = self.cursor_line.min(self.line_cache.len().saturating_sub(1));
            if let Some(LineSlot::Known(line)) = self.line_cache.get(self.cursor_line) {
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
        self.line_cache.iter().all(|slot| matches!(slot, LineSlot::Known(_)))
    }

    /// Return the total line count regardless of mode.
    ///
    /// In VLF mode `lines` is empty; use `line_cache.len()` instead.
    pub(crate) fn line_count(&self) -> usize {
        if self.is_vlf { self.line_cache.len() } else { self.lines.len() }
    }

    /// Return the text of a line by logical index, or `None` if the slot is not loaded.
    ///
    /// In normal mode reads from `lines`.  In VLF mode reads from `line_cache`
    /// and returns `None` for `LineSlot::Invalid` (show a loading indicator).
    pub(crate) fn get_line(&self, idx: usize) -> Option<&str> {
        if self.is_vlf {
            match self.line_cache.get(idx)? {
                LineSlot::Known(line) => Some(&line.text),
                LineSlot::Invalid => None,
            }
        } else {
            self.lines.get(idx).map(|s| s.as_str())
        }
    }

    /// Apply a `vlf_chunks` response to the line cache.
    ///
    /// Silently drops the response when `generation` does not match
    /// `vlf_generation`; this prevents an out-of-order reply from a superseded
    /// viewport scroll from overwriting data for the current position.
    pub(crate) fn apply_vlf_chunks(
        &mut self,
        generation: u64,
        line_start: u64,
        lines: &[String],
        approximate_line_count: u64,
        _line_count_exact: bool,
        _index_progress: f64,
    ) {
        if generation != self.vlf_generation {
            return;
        }

        self.vlf_approx_line_count = approximate_line_count;

        // Grow the line cache to fit the approximate document size.
        let target_len = (approximate_line_count as usize).max(self.line_cache.len());
        if target_len > self.line_cache.len() {
            self.line_cache.resize(target_len, LineSlot::Invalid);
        }

        // Write the received lines into the cache.
        let start = line_start as usize;
        for (i, text) in lines.iter().enumerate() {
            let idx = start + i;
            if idx < self.line_cache.len() {
                self.line_cache[idx] = LineSlot::Known(CachedLine {
                    text: text.clone(),
                    cursors: Vec::new(),
                    syntax_spans: Vec::new(),
                });
            }
        }
    }
}

impl PartialEq for BufState {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.path == other.path
            && self.display_name == other.display_name
            && self.view_id == other.view_id
            && self.editor_config_synced == other.editor_config_synced
            && self.lines == other.lines
            && self.cursor_line == other.cursor_line
            && self.cursor_col == other.cursor_col
            && self.pristine == other.pristine
            && self.status_message == other.status_message
            && self.externally_modified == other.externally_modified
            && self.annotations == other.annotations
    }
}

impl Eq for BufState {}

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

    /// Create a new xi-core process and open `path` (or a scratch buffer) as
    /// the initial view.
    pub(crate) fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>(256);
        let (from_core_tx, from_core_rx) = std_mpsc::channel::<String>();
        let (backend_tx, backend_rx) = std_mpsc::channel::<BackendEvent>();

        let core_thread = thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        send_rpc_notification(&to_core_tx, "client_started", json!({}))?;

        let (_, general_config, _) = crate::config::xi_config_tables_for_file(path.as_deref());
        send_config_notification(&to_core_tx, json!("general"), general_config)?;

        let new_view_id = 1_u64;
        send_rpc_request(
            &to_core_tx,
            new_view_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let mut from_core_rx = from_core_rx;
        let view_id_val = block_for_response(&mut from_core_rx, &to_core_tx, new_view_id)?;
        let view_id = view_id_val
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let init_events = drain_sync_notifications(&mut from_core_rx, &to_core_tx);

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
            editor_config_synced: false,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
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
            vlf_generation: 0,
            vlf_approx_line_count: 0,
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
        };

        for event in init_events {
            mgr.apply_event_to_buffer(event)?;
        }
        mgr.pump_init()?;
        Ok(mgr)
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

    pub(crate) fn reload_editor_config(&mut self) -> io::Result<()> {
        let (_, general_config, _) = crate::config::xi_config_tables_for_file(None);
        send_config_notification(&self.tx, json!("general"), general_config)?;
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
        if buf.last_scroll == Some(range) || buf.view_id.is_empty() {
            return Ok(());
        }
        buf.last_scroll = Some(range);
        let view_id = buf.view_id.clone();

        if buf.is_vlf {
            // VLF mode: use the dedicated viewport protocol so the backend
            // only decodes the visible line range from disk.  Increment the
            // generation counter so any in-flight response from the previous
            // scroll position is discarded when it arrives.
            let generation = buf.vlf_generation.wrapping_add(1);
            buf.vlf_generation = generation;
            send_xi_notification(
                &self.tx,
                "edit",
                json!({
                    "view_id": view_id,
                    "method": "vlf_viewport",
                    "params": {
                        "line_start": first_line as u64,
                        "line_end": last_line as u64,
                        "generation": generation,
                    },
                }),
            )
        } else {
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

    pub(crate) fn drain_events(&mut self) -> io::Result<()> {
        while let Ok(event) = self.backend_rx.try_recv() {
            self.apply_event_to_buffer(event)?;
        }
        Ok(())
    }

    fn pump_init(&mut self) -> io::Result<()> {
        let mut idle_rounds = 0;
        loop {
            if invalid_line_ranges(&self.bufs[self.current].line_cache).is_empty() {
                break;
            }
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(20)) {
                Some(event) => {
                    idle_rounds = 0;
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
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

    fn apply_event_to_buffer(&mut self, event: BackendEvent) -> io::Result<()> {
        let current = self.current;
        match event {
            BackendEvent::Update { ref view_id, .. }
            | BackendEvent::ScrollTo { ref view_id, .. } => {
                let idx = self.view_to_idx.get(view_id).copied().unwrap_or(current);
                match event {
                    BackendEvent::Update { update, .. } => {
                        let buf = &mut self.bufs[idx];
                        buf.pending_line_request = false;
                        buf.apply_update(update)?;
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
                    self.sync_buffer_editor_config(idx)?;
                }
            }
            BackendEvent::Alert(msg) => {
                self.bufs[current].status_message = Some(msg);
            }
            BackendEvent::Hover { view_id, content } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                self.pending_ui_actions.push(PendingUiAction::Hover { view_id, content });
                self.bufs[idx].status_message = Some(String::from("hover ready"));
            }
            BackendEvent::Completions { view_id, items } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                let count = items.len();
                self.pending_ui_actions.push(PendingUiAction::Completions { view_id, items });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no completions")
                } else {
                    format!("completions: {count}")
                });
            }
            BackendEvent::Locations { view_id, title, locations } => {
                // Collect for the App to dispatch to the quickfix list.
                self.pending_locations.push((view_id, title, locations));
            }
            BackendEvent::Symbols { view_id, title, symbols } => {
                // Collect for the App to dispatch to the symbols picker.
                self.pending_symbols.push((view_id, title, symbols));
            }
            BackendEvent::Diagnostics { view_id, diagnostics } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                let count = diagnostics.len();
                self.bufs[idx].diagnostics = diagnostics;
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("diagnostics cleared")
                } else {
                    format!("diagnostics: {count}")
                });
            }
            BackendEvent::CodeActions { view_id, actions } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                let count = actions.len();
                self.pending_ui_actions.push(PendingUiAction::CodeActions { view_id, actions });
                self.bufs[idx].status_message = Some(if count == 0 {
                    String::from("no code actions")
                } else {
                    format!("code actions: {count}")
                });
            }
            BackendEvent::DocumentMode { view_id, is_vlf } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                self.bufs[idx].is_vlf = is_vlf;
                // Rebuild lines now that mode is set (no-op in VLF mode).
                self.bufs[idx].rebuild_lines();
            }
            BackendEvent::VlfChunks {
                view_id,
                generation,
                line_start,
                lines,
                approximate_line_count,
                line_count_exact,
                index_progress,
            } => {
                let idx = self.view_to_idx.get(&view_id).copied().unwrap_or(current);
                self.bufs[idx].apply_vlf_chunks(
                    generation,
                    line_start,
                    &lines,
                    approximate_line_count,
                    line_count_exact,
                    index_progress,
                );
            }
        }
        Ok(())
    }

    fn request_invalid_lines(&mut self, idx: usize) -> io::Result<()> {
        let Some(buf) = self.bufs.get_mut(idx) else { return Ok(()) };
        if buf.pending_line_request || buf.view_id.is_empty() {
            return Ok(());
        }

        let invalid_ranges = invalid_line_ranges(&buf.line_cache);
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

    fn has_pending_line_work(&self) -> bool {
        self.bufs
            .iter()
            .any(|buf| buf.pending_line_request || !invalid_line_ranges(&buf.line_cache).is_empty())
    }

    fn request_all_invalid_lines(&mut self) -> io::Result<()> {
        for idx in 0..self.bufs.len() {
            self.request_invalid_lines(idx)?;
        }
        Ok(())
    }

    // ── Multi-buffer management ───────────────────────────────────────────

    /// Open a new xi view for `path` (or scratch) and add it as an inactive
    /// buffer.  Returns the new buffer's [`BufferId`].
    pub(crate) fn open_buffer(&mut self, path: Option<PathBuf>) -> io::Result<BufferId> {
        let rpc_id = self.next_rpc_id;
        self.next_rpc_id += 1;

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
            status_message: None,
            last_scroll: None,
            mtime,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
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
        let mut idle_rounds = 0;
        for _ in 0..24 {
            self.request_all_invalid_lines()?;
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(10)) {
                Some(event) => {
                    idle_rounds = 0;
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
                    }
                }
                None if self.has_pending_line_work() => continue,
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
            status_message: None,
            last_scroll: None,
            mtime: None,
            externally_modified: false,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            is_vlf: false,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
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
            if let (Some(stored), Some(current)) = (buf.mtime, current_mtime) {
                if current > stored {
                    buf.externally_modified = true;
                }
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
        buf.status_message = Some("reloaded".to_owned());
        buf.mtime = mtime;
        buf.externally_modified = false;

        self.view_to_idx.insert(new_view_id, idx);
        Ok(())
    }
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
