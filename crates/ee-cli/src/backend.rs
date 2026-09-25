use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;
use xi_core_lib::plugin_rpc::{CodeActionDescriptor, Diagnostic, SymbolItem};
use xi_core_lib::plugins::PluginTerminationReason;
use xi_core_lib::plugins::rpc::ClientPluginInfo;
use xi_rpc::{ReadTransport, WriteTransport};

pub(crate) mod update;
pub(crate) use update::{
    CoreAnnotation, CoreNotificationParams, CoreSyntaxSpan, CoreUpdate, CoreUpdateKind,
    CoreUpdateOp,
};

pub(crate) struct ChannelReader {
    pub(crate) rx: mpsc::Receiver<String>,
}

/// One frame on the core -> frontend channel.
///
/// The in-process pair carries two frame kinds so a payload can ship raw bytes
/// (the update text blob) without newline framing or base64. The frontend -> core
/// direction stays text-only: nothing sends binary frames to the core, so
/// [`ChannelReader`] keeps the trait's unsupported default for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    Text(String),
    Binary(Vec<u8>),
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
    pub(crate) tx: std_mpsc::Sender<Frame>,
}

impl WriteTransport for ChannelWriter {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        let message = String::from_utf8(data.to_vec())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.tx
            .send(Frame::Text(message))
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }

    fn write_binary_message(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx
            .send(Frame::Binary(data.to_vec()))
            .map_err(|err| io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
    }

    fn supports_binary_frames(&self) -> bool {
        true
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
    AvailablePlugins {
        view_id: String,
        plugins: Vec<ClientPluginInfo>,
    },
    Diagnostics {
        view_id: String,
        diagnostics: Vec<Diagnostic>,
    },
    CodeActions {
        view_id: String,
        actions: Vec<CodeActionDescriptor>,
    },
    AgentToolResult {
        view_id: String,
        kind: String,
        payload: Value,
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
    VlfSearchStatus {
        view_id: String,
        query: String,
        scanned_bytes: u64,
        total_bytes: u64,
        complete: bool,
        stored_match_count: usize,
        ranges: Vec<VlfSearchRange>,
    },
    SaveProgress {
        view_id: String,
        complete: bool,
        generation: u64,
    },
    SaveResult {
        view_id: String,
        generation: u64,
        success: bool,
        permission_denied: bool,
        message: Option<String>,
    },
}

impl BackendEvent {
    pub(crate) fn is_startup_critical(&self) -> bool {
        matches!(
            self,
            Self::Update { .. }
                | Self::ScrollTo { .. }
                | Self::DocumentMode { .. }
                | Self::VlfSearchStatus { .. }
        )
    }

    fn coalesce_key(&self) -> Option<BackendEventCoalesceKey> {
        match self {
            Self::Hover { view_id, .. } => Some(BackendEventCoalesceKey::Hover(view_id.clone())),
            Self::Completions { view_id, .. } => {
                Some(BackendEventCoalesceKey::Completions(view_id.clone()))
            }
            Self::Diagnostics { view_id, .. } => {
                Some(BackendEventCoalesceKey::Diagnostics(view_id.clone()))
            }
            Self::AvailablePlugins { view_id, .. } => {
                Some(BackendEventCoalesceKey::AvailablePlugins(view_id.clone()))
            }
            Self::CodeActions { view_id, .. } => {
                Some(BackendEventCoalesceKey::CodeActions(view_id.clone()))
            }
            Self::AgentToolResult { view_id, kind, .. } => {
                Some(BackendEventCoalesceKey::AgentToolResult(view_id.clone(), kind.clone()))
            }
            Self::DocumentMode { view_id, .. } => {
                Some(BackendEventCoalesceKey::DocumentMode(view_id.clone()))
            }
            Self::VlfSearchStatus { view_id, query, .. } => {
                Some(BackendEventCoalesceKey::VlfSearchStatus(view_id.clone(), query.clone()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BackendEventCoalesceKey {
    Hover(String),
    Completions(String),
    Diagnostics(String),
    AvailablePlugins(String),
    CodeActions(String),
    AgentToolResult(String, String),
    DocumentMode(String),
    VlfSearchStatus(String, String),
}

pub(crate) fn coalesce_backend_events(events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(events.len());
    for event in events.into_iter().rev() {
        if let Some(key) = event.coalesce_key()
            && !seen.insert(key)
        {
            continue;
        }
        out.push(event);
    }
    out.reverse();
    out
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
    /// Logical line number of this visual row (0-based); `None` on wrapped
    /// continuation rows so the gutter can leave them blank.
    pub(crate) logical_line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LineSlot {
    Known(CachedLine),
    Invalid,
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

/// Borrowed form of [`normalize_line_text`], for callers that need the served line
/// length without an allocation.
pub(crate) fn strip_line_ending(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

pub(crate) fn normalize_line_text(text: Option<String>) -> String {
    match text {
        Some(text) => strip_line_ending(&text).to_owned(),
        None => String::new(),
    }
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

pub(crate) fn invalid_line_ranges_bounded(
    line_cache: &[LineSlot],
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    let end = end.min(line_cache.len());
    if start >= end {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut range_start = None;

    for (index, slot) in line_cache[start..end].iter().enumerate() {
        let index = start + index;
        match (slot, range_start) {
            (LineSlot::Invalid, None) => range_start = Some(index),
            (LineSlot::Known(_), Some(start)) => {
                ranges.push((start, index));
                range_start = None;
            }
            _ => {}
        }
    }

    if let Some(start) = range_start {
        ranges.push((start, end));
    }

    ranges
}

pub(crate) fn xi_reader_thread(
    rx: std_mpsc::Receiver<Frame>,
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
            Ok(Frame::Text(raw)) => raw,
            // A binary frame outside the update it belongs to: the blob is read
            // synchronously with its notification, so this is a protocol error.
            Ok(Frame::Binary(_)) => continue,
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
                let _ = backend_tx.send(attach_blob_frame(event, &rx));
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

/// Reads the binary text frame of an update notification that references one.
///
/// The blob is written back to back with its notification, so the frame is read
/// synchronously here and cannot be mistaken for a later message. A missing frame
/// or one that is not binary is an error: the caller rejects the payload instead
/// of slicing from whatever arrives next. The frame length is not declared by the
/// payload; its slices are validated against the frame that actually arrived,
/// which is the stronger check.
fn read_blob_frame(rx: &std_mpsc::Receiver<Frame>) -> Result<Arc<[u8]>, String> {
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Frame::Binary(bytes)) => Ok(Arc::from(bytes)),
        Ok(Frame::Text(_)) => Err(String::from("blob declared, but a text frame followed")),
        Err(_) => Err(String::from("blob declared, but the channel closed")),
    }
}

/// Attaches the binary text frame an `update` event declared.
///
/// A malformed or missing frame turns the event into an operator-visible alert
/// instead of an update, so the buffer keeps its previous cache and the operator
/// sees why.
fn attach_blob_frame(mut event: BackendEvent, rx: &std_mpsc::Receiver<Frame>) -> BackendEvent {
    let BackendEvent::Update { update, .. } = &mut event else {
        return event;
    };
    if !update.declares_blob() {
        return event;
    }
    match read_blob_frame(rx) {
        Ok(blob) => {
            update.blob = Some(blob);
            event
        }
        Err(err) => BackendEvent::Alert(format!("dropped malformed update text frame: {err}")),
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
            let _ = format_plugin_state_notification("started", &params)?;
            None
        }
        "plugin_stopped" => {
            let _ = format_plugin_state_notification("stopped", &params)?;
            None
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
        "available_plugins" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let plugins =
                serde_json::from_value::<Vec<ClientPluginInfo>>(params.get("plugins")?.clone())
                    .ok()?;
            Some(BackendEvent::AvailablePlugins { view_id, plugins })
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
        "agent_tool_result" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let kind = params.get("kind").and_then(Value::as_str)?.to_owned();
            let payload = params.get("payload")?.clone();
            Some(BackendEvent::AgentToolResult { view_id, kind, payload })
        }
        "document_mode" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let is_vlf = params.get("is_vlf").and_then(Value::as_bool).unwrap_or(false);
            Some(BackendEvent::DocumentMode { view_id, is_vlf })
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
        "save_progress" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let complete = params.get("complete").and_then(Value::as_bool).unwrap_or(false);
            let generation = params.get("generation").and_then(Value::as_u64).unwrap_or(0);
            Some(BackendEvent::SaveProgress { view_id, complete, generation })
        }
        "save_result" => {
            let view_id = params.get("view_id").and_then(Value::as_str)?.to_owned();
            let generation = params.get("generation").and_then(Value::as_u64).unwrap_or(0);
            let success = params.get("success").and_then(Value::as_bool).unwrap_or(false);
            let permission_denied =
                params.get("permission_denied").and_then(Value::as_bool).unwrap_or(false);
            let message = params.get("message").and_then(Value::as_str).map(str::to_owned);
            Some(BackendEvent::SaveResult {
                view_id,
                generation,
                success,
                permission_denied,
                message,
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
    rx: &mut std_mpsc::Receiver<Frame>,
    tx: &mpsc::Sender<String>,
    expected_id: u64,
) -> io::Result<Value> {
    loop {
        let raw = match rx.recv() {
            Ok(Frame::Text(raw)) => raw,
            Ok(Frame::Binary(_)) => continue,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "rpc response channel closed",
                ));
            }
        };
        let msg: Value = serde_json::from_str(&raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            } else if let Some(event) = parse_notification(method, params) {
                // Startup notifications can carry an update blob too; consume it
                // from the stream even though this path drops the event, or the
                // binary frame would be read as a malformed message later.
                let _ = attach_blob_frame(event, rx);
            }
            continue;
        }

        if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return parse_response(msg);
        }
    }
}

pub(crate) fn drain_sync_notifications(
    rx: &mut std_mpsc::Receiver<Frame>,
    tx: &mpsc::Sender<String>,
) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    let mut timeout = Duration::from_millis(20);
    while let Some(frame) = recv_with_timeout(rx, timeout) {
        timeout = Duration::from_millis(1);
        let Frame::Text(raw) = frame else { continue };
        let msg: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id").cloned() {
                respond_to_frontend_request(method, params, id, tx);
            } else if let Some(event) = parse_notification(method, params) {
                events.push(attach_blob_frame(event, rx));
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
