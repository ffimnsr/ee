use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use xi_core_lib::XiCore;
use xi_core_lib::plugin_rpc::{Diagnostic, SymbolItem};
use xi_core_lib::rpc::LineReplacement;
use xi_rpc::RpcLoop;

use crate::backend::{
    BackendEvent, CachedLine, ChannelReader, ChannelWriter, CoreUpdate, CoreUpdateKind, LineSlot,
    NavigationTarget, PendingRequests, PendingUiAction, block_for_response, checked_advance,
    drain_sync_notifications, invalid_line_ranges, normalize_line_text, parse_response,
    send_rpc_notification, send_rpc_request, xi_reader_thread,
};
use crate::text::previous_char_boundary;

pub(crate) type BufferId = u32;

// ── Per-view buffer state ─────────────────────────────────────────────────────

/// All state associated with one open xi view (no connection fields).
#[derive(Debug)]
pub(crate) struct BufState {
    pub(crate) id: BufferId,
    pub(crate) path: Option<PathBuf>,
    pub(crate) view_id: String,
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
}

impl BufState {
    pub(crate) fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
            .to_owned()
    }

    pub(crate) fn apply_update(&mut self, update: CoreUpdate) -> io::Result<()> {
        let previous = std::mem::take(&mut self.line_cache);
        let mut next_cache = Vec::new();
        let mut source_index = 0;

        self.pristine = update.pristine;

        for op in update.ops {
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
        if self.lines.is_empty() {
            self.cursor_line = 0;
            self.cursor_col = 0;
            return;
        }
        self.cursor_line = self.cursor_line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = previous_char_boundary(&self.lines[self.cursor_line], self.cursor_col);
    }

    pub(crate) fn request_invalidated_lines(&mut self, tx: &Sender<String>) -> io::Result<()> {
        if self.pending_line_request || self.view_id.is_empty() {
            return Ok(());
        }
        let invalid_ranges = invalid_line_ranges(&self.line_cache);
        if invalid_ranges.is_empty() {
            return Ok(());
        }
        for (start, end) in invalid_ranges {
            send_xi_notification(
                tx,
                "edit",
                json!({
                    "view_id": self.view_id,
                    "method": "request_lines",
                    "params": [start, end],
                }),
            )?;
        }
        self.pending_line_request = true;
        Ok(())
    }
}

impl PartialEq for BufState {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.path == other.path
            && self.view_id == other.view_id
            && self.lines == other.lines
            && self.cursor_line == other.cursor_line
            && self.cursor_col == other.cursor_col
            && self.pristine == other.pristine
            && self.status_message == other.status_message
            && self.externally_modified == other.externally_modified
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
    pub(crate) tx: Sender<String>,
    /// Receive side of events coming from the xi-core reader thread.
    pub(crate) backend_rx: Receiver<BackendEvent>,
    /// All open buffers.
    bufs: Vec<BufState>,
    /// Maps xi view_id strings to indices in `bufs`.
    view_to_idx: HashMap<String, usize>,
    /// Index of the currently active buffer in `bufs`.
    current: usize,
    /// Index of the alternate buffer (for Ctrl-^ / `:b#`).
    alternate: Option<usize>,
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

impl BufferManager {
    /// Create a new xi-core process and open `path` (or a scratch buffer) as
    /// the initial view.
    pub(crate) fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>();
        let (from_core_tx, from_core_rx) = mpsc::channel::<String>();
        let (backend_tx, backend_rx) = mpsc::channel::<BackendEvent>();

        thread::spawn(move || {
            let mut core = XiCore::new();
            let mut rpc_loop = RpcLoop::new(ChannelWriter { tx: from_core_tx });
            let _ = rpc_loop.mainloop(|| ChannelReader { rx: to_core_rx }, &mut core);
        });

        send_rpc_notification(&to_core_tx, "client_started", json!({}))?;

        let new_view_id = 1_u64;
        send_rpc_request(
            &to_core_tx,
            new_view_id,
            "new_view",
            json!({ "file_path": path.as_ref().map(|p| p.to_string_lossy().to_string()) }),
        )?;

        let view_id_val = block_for_response(&from_core_rx, &to_core_tx, new_view_id)?;
        let view_id = view_id_val
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let init_events = drain_sync_notifications(&from_core_rx, &to_core_tx);

        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = Arc::clone(&pending);
        let tx_clone = to_core_tx.clone();
        thread::spawn(move || xi_reader_thread(from_core_rx, tx_clone, backend_tx, pending_clone));

        let buf = BufState {
            id: 1,
            path: path.clone(),
            view_id: view_id.clone(),
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
        };

        let mut view_to_idx = HashMap::new();
        view_to_idx.insert(view_id, 0);

        let mut mgr = Self {
            tx: to_core_tx,
            backend_rx,
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
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

        let (resp_tx, resp_rx) = mpsc::sync_channel::<Value>(1);
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
        let buf = &self.bufs[self.current];
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
        let buf = &mut self.bufs[self.current];
        buf.mtime = new_mtime;
        buf.externally_modified = false;
        buf.status_message = Some(format!("saved {display}"));
        // Remove crash-recovery artifact now that the file is persisted.
        if let Some(rp) = recovery_file_path(&path) {
            let _ = std::fs::remove_file(rp);
        }
        Ok(())
    }

    pub(crate) fn request_completion(&mut self, index: Option<usize>) -> io::Result<()> {
        self.send_edit("request_completion", json!({ "index": index }))
    }

    pub(crate) fn request_definition(&mut self) -> io::Result<()> {
        self.send_edit("request_definition", json!({}))
    }

    pub(crate) fn request_references(&mut self) -> io::Result<()> {
        self.send_edit("request_references", json!({}))
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

    pub(crate) fn apply_line_replacements(
        &mut self,
        replacements: &[LineReplacement],
    ) -> io::Result<()> {
        self.send_edit("apply_line_replacements", json!({ "replacements": replacements }))
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

    pub(crate) fn drain_events(&mut self) -> io::Result<()> {
        while let Ok(event) = self.backend_rx.try_recv() {
            self.apply_event_to_buffer(event)?;
        }
        Ok(())
    }

    fn pump_init(&mut self) -> io::Result<()> {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            if invalid_line_ranges(&self.bufs[self.current].line_cache).is_empty() {
                break;
            }
            match self.backend_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => {
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }

    fn apply_event_to_buffer(&mut self, event: BackendEvent) -> io::Result<()> {
        let tx = self.tx.clone();
        let current = self.current;
        match event {
            BackendEvent::Update { ref view_id, .. }
            | BackendEvent::ScrollTo { ref view_id, .. } => {
                let idx = self.view_to_idx.get(view_id).copied().unwrap_or(current);
                let buf = &mut self.bufs[idx];
                match event {
                    BackendEvent::Update { update, .. } => {
                        buf.pending_line_request = false;
                        buf.apply_update(update)?;
                        buf.request_invalidated_lines(&tx)?;
                    }
                    BackendEvent::ScrollTo { line, col, .. } => {
                        buf.cursor_line = line;
                        buf.cursor_col = col;
                        buf.clamp_cursor();
                    }
                    _ => unreachable!(),
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
        let (resp_tx, resp_rx) = mpsc::sync_channel::<Value>(1);
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
            view_id,
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
        });
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
        Ok(())
    }

    /// Switch to buffer at list index `idx`, saving current as alternate.
    pub(crate) fn switch_to_idx(&mut self, idx: usize) {
        if idx < self.bufs.len() && idx != self.current {
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

    #[cfg(test)]
    pub(crate) fn pump(&mut self) -> io::Result<()> {
        use std::sync::mpsc::RecvTimeoutError;
        for _ in 0..6 {
            match self.backend_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => {
                    self.apply_event_to_buffer(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_event_to_buffer(event)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }

    /// Test-only constructor that builds a minimal `BufferManager` around
    /// pre-existing channel ends and a known view_id.
    #[cfg(test)]
    pub(crate) fn test_new(
        tx: Sender<String>,
        backend_rx: Receiver<BackendEvent>,
        view_id: String,
    ) -> Self {
        let buf = BufState {
            id: 1,
            path: None,
            view_id: view_id.clone(),
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
        };
        let mut view_to_idx = HashMap::new();
        view_to_idx.insert(view_id, 0);
        Self {
            tx,
            backend_rx,
            bufs: vec![buf],
            view_to_idx,
            current: 0,
            alternate: None,
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

        let (resp_tx, resp_rx) = mpsc::sync_channel::<Value>(1);
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

fn send_xi_notification(tx: &Sender<String>, method: &str, params: Value) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    tx.send(raw).map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

// Keep these imports satisfied for the test helper.
#[allow(dead_code)]
fn _use_normalize(text: Option<String>) -> String {
    normalize_line_text(text)
}
#[allow(dead_code)]
fn _use_nav(_: &NavigationTarget) {}
