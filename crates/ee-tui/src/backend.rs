use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;
use xi_core_lib::XiCore;
use xi_core_lib::plugin_rpc::{CodeActionDescriptor, Diagnostic, SymbolItem};
use xi_core_lib::plugins::PluginTerminationReason;
use xi_rpc::{ReadTransport, RpcLoop, WriteTransport};

use crate::text::previous_char_boundary;

pub(crate) struct ChannelReader {
    pub(crate) rx: mpsc::Receiver<String>,
}

impl ReadTransport for ChannelReader {
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize> {
        match self.rx.blocking_recv() {
            Some(message) => {
                let len = message.len();
                buf.push_str(&message);
                Ok(len)
            }
            None => Ok(0),
        }
    }
}

pub(crate) struct ChannelWriter {
    pub(crate) tx: std_mpsc::Sender<String>,
}

impl WriteTransport for ChannelWriter {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        let message = String::from_utf8(data.to_vec())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .send(message)
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }
}

pub(crate) type PendingRequests =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, std_mpsc::SyncSender<Value>>>>;

#[derive(Debug)]
pub(crate) enum BackendEvent {
    Update {
        view_id: String,
        update: CoreUpdate,
    },
    Alert(String),
    Hover {
        view_id: String,
        content: String,
    },
    Completions {
        view_id: String,
        items: Vec<CompletionSuggestion>,
    },
    Locations {
        view_id: String,
        title: String,
        locations: Vec<NavigationTarget>,
    },
    Symbols {
        view_id: String,
        title: String,
        symbols: Vec<SymbolItem>,
    },
    Diagnostics {
        view_id: String,
        diagnostics: Vec<Diagnostic>,
    },
    CodeActions {
        view_id: String,
        actions: Vec<CodeActionDescriptor>,
    },
    ScrollTo {
        view_id: String,
        line: usize,
        col: usize,
    },
    /// Backend notified the frontend about the document mode for a view.
    /// `is_vlf` is `true` for Very Large File buffers that require sparse rendering.
    DocumentMode {
        view_id: String,
        is_vlf: bool,
    },
    /// Backend responded to a `vlf_viewport` request with decoded line content.
    ///
    /// `generation` echoes the request token; responses with a stale generation
    /// are discarded before updating the line cache.
    VlfChunks {
        view_id: String,
        generation: u64,
        line_start: u64,
        lines: Vec<String>,
        approximate_line_count: u64,
        line_count_exact: bool,
        index_progress: f64,
    },
    VlfSearchStatus {
        view_id: String,
        query: String,
        scanned_bytes: u64,
        total_bytes: u64,
        complete: bool,
        stored_match_count: usize,
        ranges: Vec<VlfSearchRange>,
    },
}

impl BackendEvent {
    pub(crate) fn is_startup_critical(&self) -> bool {
        matches!(
            self,
            Self::Update { .. }
                | Self::ScrollTo { .. }
                | Self::DocumentMode { .. }
                | Self::VlfChunks { .. }
                | Self::VlfSearchStatus { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct VlfSearchRange {
    pub(crate) line: u64,
    pub(crate) start_col: usize,
    pub(crate) end_col: usize,
}

pub(crate) fn startup_render_ready(line_cache: &[LineSlot]) -> bool {
    line_cache.first().is_some_and(|slot| matches!(slot, LineSlot::Known(_)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingUiAction {
    Hover { view_id: String, content: String },
    Completions { view_id: String, items: Vec<CompletionSuggestion> },
    CodeActions { view_id: String, actions: Vec<CodeActionDescriptor> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CompletionSuggestion {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) insert_text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct NavigationTarget {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) end_line: usize,
    pub(crate) end_column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedLine {
    pub(crate) text: String,
    pub(crate) cursors: Vec<usize>,
    pub(crate) syntax_spans: Vec<CoreSyntaxSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LineSlot {
    Known(CachedLine),
    Invalid,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreNotificationParams {
    pub(crate) view_id: String,
    pub(crate) update: CoreUpdate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreUpdate {
    pub(crate) ops: Vec<CoreUpdateOp>,
    pub(crate) pristine: bool,
    #[serde(default)]
    pub(crate) annotations: Vec<CoreAnnotation>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreAnnotation {
    #[serde(rename = "type")]
    pub(crate) annotation_type: String,
    #[serde(default)]
    pub(crate) ranges: Vec<[usize; 4]>,
    #[serde(default)]
    pub(crate) payloads: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreUpdateOp {
    pub(crate) op: CoreUpdateKind,
    pub(crate) n: usize,
    #[serde(default)]
    pub(crate) lines: Vec<CoreLine>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CoreUpdateKind {
    #[serde(rename = "ins")]
    Insert,
    Skip,
    Invalidate,
    Copy,
    Update,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreLine {
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) cursor: Vec<usize>,
    #[serde(default)]
    pub(crate) syntax_spans: Option<Vec<CoreSyntaxSpan>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreSyntaxSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) scope: String,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct XiClient {
    pub(crate) path: Option<PathBuf>,
    pub(crate) tx: mpsc::Sender<String>,
    pub(crate) pending_requests: PendingRequests,
    pub(crate) next_request_id: u64,
    pub(crate) backend_rx: std_mpsc::Receiver<BackendEvent>,
    pub(crate) view_id: String,
    pub(crate) pending_line_request: bool,
    pub(crate) line_cache: Vec<LineSlot>,
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_line: usize,
    pub(crate) cursor_col: usize,
    pub(crate) pristine: bool,
    pub(crate) status_message: Option<String>,
    pub(crate) last_scroll: Option<(usize, usize)>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) annotations: Vec<CoreAnnotation>,
    /// Pending symbol results waiting to be opened in a picker.
    pub(crate) pending_symbols: Vec<(String, String, Vec<SymbolItem>)>,
    /// True when the backend opened this buffer in VLF mode.
    /// `lines` is never materialized; rendering reads `line_cache` directly.
    pub(crate) is_vlf: bool,
    /// Monotone counter incremented on every VLF viewport scroll; see
    /// [`BufState::vlf_generation`] for the full design note.
    pub(crate) vlf_generation: u64,
    /// Last approximate total line count from a `vlf_chunks` response.
    pub(crate) vlf_approx_line_count: u64,
    /// True when `vlf_approx_line_count` is backend-confirmed exact.
    pub(crate) vlf_line_count_exact: bool,
}

#[allow(dead_code)]
impl XiClient {
    pub(crate) fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let (to_core_tx, to_core_rx) = mpsc::channel::<String>(256);
        let (from_core_tx, mut from_core_rx) = std_mpsc::channel::<String>();
        let (backend_tx, backend_rx) = std_mpsc::channel::<BackendEvent>();

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

        let view_id = block_for_response(&mut from_core_rx, &to_core_tx, new_view_id)?;
        let view_id = view_id
            .as_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "new_view returned non-string id")
            })?
            .to_owned();

        let init_events = drain_sync_notifications(&mut from_core_rx, &to_core_tx);

        let pending: PendingRequests =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let tx_clone = to_core_tx.clone();
        let pending_clone = std::sync::Arc::clone(&pending);
        thread::spawn(move || {
            xi_reader_thread(from_core_rx, tx_clone, backend_tx, pending_clone, None)
        });

        let mut client = Self {
            path,
            tx: to_core_tx,
            pending_requests: pending,
            next_request_id: 2,
            backend_rx,
            view_id,
            pending_line_request: false,
            line_cache: Vec::new(),
            lines: Vec::new(),
            cursor_line: 0,
            cursor_col: 0,
            pristine: true,
            status_message: None,
            last_scroll: None,
            diagnostics: Vec::new(),
            annotations: Vec::new(),
            pending_symbols: Vec::new(),
            is_vlf: false,
            vlf_generation: 0,
            vlf_approx_line_count: 0,
            vlf_line_count_exact: false,
        };

        for event in init_events {
            client.apply_backend_event(event)?;
        }
        client.pump_init()?;
        Ok(client)
    }

    pub(crate) fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("[scratch]")
            .to_owned()
    }

    pub(crate) fn save(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "scratch buffer has no path"));
        };

        self.send_notification(
            "save",
            json!({
                "view_id": self.view_id,
                "file_path": path.to_string_lossy().to_string(),
            }),
        )?;
        self.status_message = Some(format!("saved {}", path.display()));
        Ok(())
    }

    pub(crate) fn send_edit(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send_notification(
            "edit",
            json!({
                "view_id": self.view_id,
                "method": method,
                "params": params,
            }),
        )
    }

    pub(crate) fn send_request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let (response_tx, response_rx) = std_mpsc::sync_channel(1);
        self.pending_requests
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(request_id, response_tx);

        send_rpc_request(&self.tx, request_id, method, params)?;
        let raw = response_rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "rpc response channel closed")
        })?;
        parse_response(raw)
    }

    pub(crate) fn request_completion(&mut self) -> io::Result<()> {
        self.send_edit("request_completion", json!({}))
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

    pub(crate) fn selected_text_preview(&mut self, linewise: bool) -> io::Result<String> {
        let response = self.send_request(
            "selected_text_preview",
            json!({
                "view_id": self.view_id,
                "linewise": linewise,
            }),
        )?;
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
        let response = self.send_request(
            "block_text_preview",
            json!({
                "view_id": self.view_id,
                "start_line": start_line,
                "end_line": end_line,
                "left_col": left_col,
                "right_col": right_col,
            }),
        )?;
        serde_json::from_value(response)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
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

    pub(crate) fn drain_events(&mut self) -> io::Result<()> {
        while let Ok(event) = self.backend_rx.try_recv() {
            self.apply_backend_event(event)?;
        }
        Ok(())
    }

    fn pump_init(&mut self) -> io::Result<()> {
        let mut idle_rounds = 0;
        loop {
            if startup_render_ready(&self.line_cache) {
                break;
            }
            if invalid_line_ranges(&self.line_cache).is_empty() {
                break;
            }
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(20)) {
                Some(event) => {
                    let mut saw_critical = event.is_startup_critical();
                    self.apply_backend_event(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        saw_critical |= event.is_startup_critical();
                        self.apply_backend_event(event)?;
                    }

                    if saw_critical {
                        idle_rounds = 0;
                    } else {
                        idle_rounds += 1;
                        if idle_rounds >= 6 {
                            break;
                        }
                    }
                }
                None => {
                    idle_rounds += 1;
                    if idle_rounds >= 6 {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn apply_backend_event(&mut self, event: BackendEvent) -> io::Result<()> {
        match event {
            BackendEvent::Update { update, .. } => {
                self.pending_line_request = false;
                self.apply_update(update)?;
            }
            BackendEvent::ScrollTo { line, col, .. } => {
                self.cursor_line = line;
                self.cursor_col = col;
                self.clamp_cursor();
            }
            BackendEvent::Alert(msg) => {
                self.status_message = Some(msg);
            }
            BackendEvent::Hover { content, .. } => {
                self.status_message = Some(content);
            }
            BackendEvent::Completions { items, .. } => {
                let preview =
                    items.iter().take(8).map(|item| item.label.as_str()).collect::<Vec<_>>();
                self.status_message = Some(if preview.is_empty() {
                    String::from("no completions")
                } else if items.len() > preview.len() {
                    format!(
                        "completions: {} (+{} more)",
                        preview.join(", "),
                        items.len() - preview.len()
                    )
                } else {
                    format!("completions: {}", preview.join(", "))
                });
            }
            BackendEvent::Locations { title, locations, .. } => {
                let same_file = locations.len() == 1
                    && self
                        .path
                        .as_ref()
                        .is_some_and(|path| path.to_string_lossy() == locations[0].path);
                if same_file {
                    self.send_edit("goto_line", json!({ "line": locations[0].line }))?;
                }
                self.status_message = Some(format_location_message(&title, &locations));
            }
            BackendEvent::Symbols { view_id, title, symbols } => {
                self.status_message = Some(format!("{}: {} symbols", title, symbols.len()));
                self.pending_symbols.push((view_id, title, symbols));
            }
            BackendEvent::Diagnostics { diagnostics, .. } => {
                let count = diagnostics.len();
                self.diagnostics = diagnostics;
                self.status_message = Some(if count == 0 {
                    String::from("diagnostics cleared")
                } else {
                    format!("diagnostics: {count}")
                });
            }
            BackendEvent::CodeActions { actions, .. } => {
                self.status_message = Some(if actions.is_empty() {
                    String::from("no code actions")
                } else {
                    format!("code actions: {}", actions.len())
                });
            }
            BackendEvent::DocumentMode { is_vlf, .. } => {
                self.is_vlf = is_vlf;
                if is_vlf {
                    self.vlf_line_count_exact = false;
                }
            }
            BackendEvent::VlfChunks {
                generation,
                line_start,
                lines,
                approximate_line_count,
                line_count_exact,
                index_progress,
                ..
            } => {
                if generation == self.vlf_generation {
                    self.vlf_approx_line_count = approximate_line_count;
                    self.vlf_line_count_exact = line_count_exact;
                    let target_len = (approximate_line_count as usize).max(self.line_cache.len());
                    if target_len > self.line_cache.len() {
                        self.line_cache.resize(target_len, LineSlot::Invalid);
                    }
                    if line_count_exact {
                        let exact_len =
                            usize::try_from(approximate_line_count).unwrap_or(usize::MAX);
                        if self.line_cache.len() > exact_len {
                            self.line_cache.truncate(exact_len);
                        }
                    }
                    let start = line_start as usize;
                    for (i, text) in lines.into_iter().enumerate() {
                        let idx = start + i;
                        if idx < self.line_cache.len() {
                            self.line_cache[idx] = LineSlot::Known(CachedLine {
                                text,
                                cursors: Vec::new(),
                                syntax_spans: Vec::new(),
                            });
                        }
                    }
                    let _ = index_progress;
                }
            }
            BackendEvent::VlfSearchStatus {
                query,
                scanned_bytes,
                total_bytes,
                complete,
                stored_match_count,
                ranges,
                ..
            } => {
                let preview = ranges
                    .iter()
                    .take(3)
                    .map(|range| {
                        format!("L{}:{}-{}", range.line + 1, range.start_col + 1, range.end_col + 1)
                    })
                    .collect::<Vec<_>>();
                let progress = if total_bytes == 0 {
                    String::from("0/0 B")
                } else {
                    format!("{}/{} B", scanned_bytes, total_bytes)
                };
                self.status_message = Some(if preview.is_empty() {
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
        }
        Ok(())
    }

    pub(crate) fn notify_scroll(&mut self, first_line: usize, last_line: usize) -> io::Result<()> {
        let range = (first_line, last_line);
        if self.last_scroll == Some(range) || self.view_id.is_empty() {
            return Ok(());
        }
        self.last_scroll = Some(range);
        if self.is_vlf {
            let generation = self.vlf_generation.wrapping_add(1);
            self.vlf_generation = generation;
            self.send_notification(
                "edit",
                json!({
                    "view_id": self.view_id,
                    "method": "vlf_viewport",
                    "params": {
                        "line_start": first_line as u64,
                        "line_end": last_line as u64,
                        "generation": generation,
                    },
                }),
            )
        } else {
            self.send_notification(
                "edit",
                json!({
                    "view_id": self.view_id,
                    "method": "scroll",
                    "params": [first_line, last_line],
                }),
            )
        }
    }

    fn clamp_cursor(&mut self) {
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

    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        self.send_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn send_message(&self, value: Value) -> io::Result<()> {
        let message = serde_json::to_string(&value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .blocking_send(message)
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
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
                    next_cache.extend(previous[source_index..end].iter().cloned());
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
        // Rendering reads `line_cache` directly for the viewport range.
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

        if matches!(self.line_cache.as_slice(), [LineSlot::Known(CachedLine { text, .. })] if text.is_empty())
        {
            self.lines.clear();
        }
    }

    fn sync_cursor_from_cache(&mut self) {
        for (line_index, slot) in self.line_cache.iter().enumerate() {
            let LineSlot::Known(line) = slot else {
                continue;
            };
            if let Some(&cursor_col) = line.cursors.first() {
                self.cursor_line = line_index;
                self.cursor_col = previous_char_boundary(&line.text, cursor_col);
                self.clamp_cursor();
                return;
            }
        }

        self.clamp_cursor();
    }

    fn request_invalidated_lines(&mut self) -> io::Result<()> {
        if self.pending_line_request || self.view_id.is_empty() {
            return Ok(());
        }

        let invalid_ranges = invalid_line_ranges(&self.line_cache);
        if invalid_ranges.is_empty() {
            return Ok(());
        }

        for (start, end) in invalid_ranges {
            self.send_notification(
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

    #[cfg(test)]
    pub(crate) fn pump(&mut self) -> io::Result<()> {
        for _ in 0..6 {
            match recv_with_timeout(&mut self.backend_rx, Duration::from_millis(10)) {
                Some(event) => {
                    self.apply_backend_event(event)?;
                    while let Ok(event) = self.backend_rx.try_recv() {
                        self.apply_backend_event(event)?;
                    }
                }
                None => break,
            }
        }
        Ok(())
    }
}

impl PartialEq for XiClient {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.view_id == other.view_id
            && self.lines == other.lines
            && self.cursor_line == other.cursor_line
            && self.cursor_col == other.cursor_col
            && self.pristine == other.pristine
            && self.status_message == other.status_message
    }
}

impl Eq for XiClient {}

impl From<CoreLine> for LineSlot {
    fn from(line: CoreLine) -> Self {
        LineSlot::Known(CachedLine {
            text: normalize_line_text(line.text),
            cursors: line.cursor,
            syntax_spans: line.syntax_spans.unwrap_or_default(),
        })
    }
}

impl LineSlot {
    pub(crate) fn merge(self, update: CoreLine) -> io::Result<Self> {
        match self {
            LineSlot::Known(mut line) => {
                if let Some(text) = update.text {
                    line.text = normalize_line_text(Some(text));
                }
                line.cursors = update.cursor;
                if let Some(syntax_spans) = update.syntax_spans {
                    line.syntax_spans = syntax_spans;
                }
                Ok(LineSlot::Known(line))
            }
            LineSlot::Invalid => {
                if update.text.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "update op cannot patch invalid line without text",
                    ));
                }
                Ok(LineSlot::from(update))
            }
        }
    }
}

pub(crate) fn parse_response(message: Value) -> io::Result<Value> {
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }

    if let Some(error) = message.get("error") {
        let message =
            error.get("message").and_then(Value::as_str).unwrap_or("rpc error").to_owned();
        return Err(io::Error::other(message));
    }

    Err(io::Error::new(io::ErrorKind::InvalidData, "rpc response missing result and error"))
}

pub(crate) fn normalize_line_text(text: Option<String>) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    text.to_owned()
}

pub(crate) fn checked_advance(
    current: usize,
    amount: usize,
    len: usize,
    op: &str,
) -> io::Result<usize> {
    let next = current.checked_add(amount).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{op} op overflowed source index"))
    })?;
    if next > len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{op} op exceeded cached line count"),
        ));
    }
    Ok(next)
}

pub(crate) fn invalid_line_ranges(line_cache: &[LineSlot]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;

    for (index, slot) in line_cache.iter().enumerate() {
        match (slot, start) {
            (LineSlot::Invalid, None) => start = Some(index),
            (LineSlot::Known(_), Some(range_start)) => {
                ranges.push((range_start, index));
                start = None;
            }
            _ => {}
        }
    }

    if let Some(range_start) = start {
        ranges.push((range_start, line_cache.len()));
    }

    ranges
}

pub(crate) fn xi_reader_thread(
    rx: std_mpsc::Receiver<String>,
    tx: mpsc::Sender<String>,
    backend_tx: std_mpsc::Sender<BackendEvent>,
    pending: PendingRequests,
    shutdown: Option<Arc<AtomicBool>>,
) {
    loop {
        if shutdown.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            break;
        }

        let raw = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(raw) => raw,
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let msg: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, &tx);
            } else if let Some(event) = parse_notification(method, params) {
                let _ = backend_tx.send(event);
            }
        } else if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            // Response to an outstanding RPC request.
            let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(resp_tx) = map.remove(&id) {
                let _ = resp_tx.send(msg);
            }
        }
    }
}

pub(crate) fn respond_to_frontend_request(
    method: &str,
    params: Value,
    id: Value,
    tx: &mpsc::Sender<String>,
) {
    let response = match method {
        "measure_width" => {
            let widths = params
                .as_array()
                .into_iter()
                .flatten()
                .map(|req| {
                    req.get("strings")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|text| {
                            Value::from(
                                UnicodeWidthStr::width(text.as_str().unwrap_or_default()) as f64
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            json!({ "jsonrpc": "2.0", "id": id, "result": widths })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unsupported frontend request: {method}") }
        }),
    };
    if let Ok(raw) = serde_json::to_string(&response) {
        let _ = tx.blocking_send(raw);
    }
}

pub(crate) fn parse_notification(method: &str, params: Value) -> Option<BackendEvent> {
    match method {
        "update" => {
            let p = serde_json::from_value::<CoreNotificationParams>(params).ok()?;
            Some(BackendEvent::Update { view_id: p.view_id, update: p.update })
        }
        "scroll_to" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let line = params.get("line").and_then(Value::as_u64)? as usize;
            let col = params.get("col").and_then(Value::as_u64)? as usize;
            Some(BackendEvent::ScrollTo { view_id, line, col })
        }
        "alert" => {
            let msg = params.get("msg").and_then(Value::as_str)?.to_owned();
            Some(BackendEvent::Alert(msg))
        }
        "plugin_started" => {
            Some(BackendEvent::Alert(format_plugin_state_notification("started", &params)?))
        }
        "plugin_stopped" => {
            Some(BackendEvent::Alert(format_plugin_state_notification("stopped", &params)?))
        }
        "plugin_terminated" => {
            Some(BackendEvent::Alert(format_plugin_terminated_notification(&params)?))
        }
        "hover" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let content = params.get("content").and_then(Value::as_str)?.to_owned();
            Some(BackendEvent::Hover { view_id, content })
        }
        "completions" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let items =
                serde_json::from_value::<Vec<CompletionSuggestion>>(params.get("items")?.clone())
                    .ok()?;
            Some(BackendEvent::Completions { view_id, items })
        }
        "locations" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let title = params.get("title").and_then(Value::as_str)?.to_owned();
            let locations =
                serde_json::from_value::<Vec<NavigationTarget>>(params.get("locations")?.clone())
                    .ok()?;
            Some(BackendEvent::Locations { view_id, title, locations })
        }
        "symbols" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let title = params.get("title").and_then(Value::as_str)?.to_owned();
            let symbols =
                serde_json::from_value::<Vec<SymbolItem>>(params.get("symbols")?.clone()).ok()?;
            Some(BackendEvent::Symbols { view_id, title, symbols })
        }
        "diagnostics" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let diagnostics =
                serde_json::from_value::<Vec<Diagnostic>>(params.get("diagnostics")?.clone())
                    .ok()?;
            Some(BackendEvent::Diagnostics { view_id, diagnostics })
        }
        "code_actions" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let actions =
                serde_json::from_value::<Vec<CodeActionDescriptor>>(params.get("actions")?.clone())
                    .ok()?;
            Some(BackendEvent::CodeActions { view_id, actions })
        }
        "document_mode" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let is_vlf = params.get("is_vlf").and_then(Value::as_bool).unwrap_or(false);
            Some(BackendEvent::DocumentMode { view_id, is_vlf })
        }
        "vlf_chunks" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let generation = params.get("generation").and_then(Value::as_u64)?;
            let line_start = params.get("line_start").and_then(Value::as_u64)?;
            let lines = params
                .get("lines")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_owned())
                .collect();
            let approximate_line_count =
                params.get("approximate_line_count").and_then(Value::as_u64).unwrap_or(0);
            let line_count_exact =
                params.get("line_count_exact").and_then(Value::as_bool).unwrap_or(false);
            let index_progress =
                params.get("index_progress").and_then(Value::as_f64).unwrap_or(0.0);
            Some(BackendEvent::VlfChunks {
                view_id,
                generation,
                line_start,
                lines,
                approximate_line_count,
                line_count_exact,
                index_progress,
            })
        }
        "vlf_search_status" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let query = params.get("query").and_then(Value::as_str)?.to_owned();
            let scanned_bytes = params.get("scanned_bytes").and_then(Value::as_u64).unwrap_or(0);
            let total_bytes = params.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
            let complete = params.get("complete").and_then(Value::as_bool).unwrap_or(false);
            let stored_match_count =
                params.get("stored_match_count").and_then(Value::as_u64).unwrap_or(0) as usize;
            let ranges = serde_json::from_value::<Vec<VlfSearchRange>>(
                params.get("ranges").cloned().unwrap_or_else(|| json!([])),
            )
            .ok()?;
            Some(BackendEvent::VlfSearchStatus {
                view_id,
                query,
                scanned_bytes,
                total_bytes,
                complete,
                stored_match_count,
                ranges,
            })
        }
        _ => None,
    }
}

fn format_plugin_state_notification(state: &str, params: &Value) -> Option<String> {
    let plugin = params.get("plugin").and_then(Value::as_str)?;
    Some(format!("plugin {plugin} {state}"))
}

fn format_plugin_terminated_notification(params: &Value) -> Option<String> {
    let plugin = params.get("plugin").and_then(Value::as_str)?;
    let reason =
        serde_json::from_value::<PluginTerminationReason>(params.get("reason")?.clone()).ok()?;
    Some(match reason {
        PluginTerminationReason::MaxRssBytes { limit_bytes, observed_bytes } => {
            format!("plugin {plugin} terminated: rss {} > {} bytes", observed_bytes, limit_bytes)
        }
        PluginTerminationReason::MaxCpuSeconds { limit_seconds, observed_seconds } => format!(
            "plugin {plugin} terminated: cpu {} > {} seconds",
            observed_seconds, limit_seconds
        ),
        PluginTerminationReason::RpcTimedOut { limit_ms, method } => {
            format!("plugin {plugin} terminated: rpc {method} timed out after {limit_ms} ms")
        }
    })
}

pub(crate) fn format_location_message(title: &str, locations: &[NavigationTarget]) -> String {
    match locations {
        [] => format!("{title}: no locations"),
        [location] => {
            format!("{title}: {}:{}:{}", location.path, location.line + 1, location.column + 1)
        }
        many => format!("{title}: {} locations", many.len()),
    }
}

pub(crate) fn send_rpc_notification(
    tx: &mpsc::Sender<String>,
    method: &str,
    params: Value,
) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    tx.blocking_send(raw).map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

pub(crate) fn send_rpc_request(
    tx: &mpsc::Sender<String>,
    id: u64,
    method: &str,
    params: Value,
) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    tx.blocking_send(raw).map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

pub(crate) fn block_for_response(
    rx: &mut std_mpsc::Receiver<String>,
    tx: &mpsc::Sender<String>,
    expected_id: u64,
) -> io::Result<Value> {
    loop {
        let raw = rx.recv().ok().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "rpc response channel closed")
        })?;
        let msg: Value = serde_json::from_str(&raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            }
            continue;
        }

        if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return parse_response(msg);
        }
    }
}

pub(crate) fn drain_sync_notifications(
    rx: &mut std_mpsc::Receiver<String>,
    tx: &mpsc::Sender<String>,
) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    let mut timeout = Duration::from_millis(20);
    while let Some(raw) = recv_with_timeout(rx, timeout) {
        timeout = Duration::from_millis(1);
        let msg: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            } else if let Some(event) = parse_notification(method, params) {
                events.push(event);
            }
        }
    }
    events
}

pub(crate) fn recv_with_timeout<T>(rx: &mut std_mpsc::Receiver<T>, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(value) => return Some(value),
            Err(std_mpsc::TryRecvError::Disconnected) => return None,
            Err(std_mpsc::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::yield_now();
            }
        }
    }
}
