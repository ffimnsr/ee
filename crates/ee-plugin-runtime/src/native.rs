//! Native process transport for the v2 wire protocol.
//!
//! Two framings are supported: newline-delimited JSON and LSP-style
//! `Content-Length` framing. The client is synchronous: a reader thread only
//! frames messages, while request/response correlation, deadlines, effects, and
//! contributions are handled on the caller thread so ordering is deterministic.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ee_plugin_api::context::ContributionSink;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{CommandId, PluginId, ServiceId};
use ee_plugin_api::manifest::{LaunchConfig, LaunchTransport, Limits, ManifestV2};
use ee_plugin_api::wire::{
    CommandParams, ContributionEnvelope, ContributionRequestParams, EffectEnvelope, EventParams,
    HealthReport, HostToPlugin, InitializeParams, PluginResponse, PluginToHost, ShutdownParams,
    WireError,
};
use serde_json::Value;

use crate::contributions::ContributionSession;

/// Maximum accepted frame size.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Maximum retained stderr bytes.
pub const MAX_STDERR_BYTES: usize = 16 * 1024;
/// Request id reserved for the initialize acknowledgement.
pub const INITIALIZE_REQUEST_ID: u64 = 0;

/// Everything needed to spawn one native plugin.
#[derive(Debug, Clone)]
pub struct NativeSpec {
    pub plugin_id: PluginId,
    pub entry: PathBuf,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    pub transport: LaunchTransport,
    pub limits: Limits,
    pub host: HostKind,
    pub services: Vec<ServiceId>,
    pub config: Value,
}

impl NativeSpec {
    pub fn new(plugin_id: PluginId, entry: impl Into<PathBuf>) -> Self {
        Self {
            plugin_id,
            entry: entry.into(),
            args: Vec::new(),
            working_dir: PathBuf::from("."),
            env: BTreeMap::new(),
            transport: LaunchTransport::StdioNewline,
            limits: Limits::default(),
            host: HostKind::Editor,
            services: Vec::new(),
            config: Value::Null,
        }
    }

    /// Build a spec from a validated manifest and its directory.
    pub fn from_manifest(
        manifest: &ManifestV2,
        manifest_dir: &Path,
        host: HostKind,
        services: Vec<ServiceId>,
        config: Value,
    ) -> Result<Self, PluginError> {
        if manifest.kind != ee_plugin_api::manifest::PluginKind::Native {
            return Err(PluginError::Manifest(
                ee_plugin_api::manifest::ManifestError::InvalidEntry {
                    entry: manifest.entry.clone(),
                    kind: manifest.kind.as_str(),
                },
            ));
        }
        let plugin_id = PluginId::new(manifest.name.clone()).map_err(PluginError::from)?;
        let LaunchConfig { transport, args, working_dir, env } = manifest.launch.clone();
        let working_dir = if Path::new(&working_dir).is_absolute() {
            PathBuf::from(&working_dir)
        } else if working_dir == "." {
            manifest_dir.to_path_buf()
        } else {
            manifest_dir.join(&working_dir)
        };
        Ok(Self {
            plugin_id,
            entry: manifest_dir.join(&manifest.entry),
            args,
            working_dir,
            env,
            transport,
            limits: manifest.limits,
            host,
            services,
            config,
        })
    }
}

/// Synchronous native plugin client.
pub struct NativeClient {
    child: Child,
    stdin: Option<ChildStdin>,
    receiver: Receiver<Result<PluginToHost, String>>,
    reader: Option<JoinHandle<()>>,
    stderr_text: Arc<Mutex<String>>,
    stderr_reader: Option<JoinHandle<()>>,
    transport: LaunchTransport,
    adapter: Arc<dyn HostAdapter>,
    sink: Arc<dyn ContributionSink>,
    contributions: ContributionSession,
    health: Option<HealthReport>,
    rpc_timeout: Duration,
    next_request_id: u64,
    last_contribution_revision: u64,
    plugin_id: PluginId,
    stopped: bool,
}

impl NativeClient {
    /// Spawn the plugin and complete the initialize handshake.
    pub fn spawn(
        spec: NativeSpec,
        adapter: Arc<dyn HostAdapter>,
        sink: Arc<dyn ContributionSink>,
    ) -> Result<Self, PluginError> {
        if !spec.entry.is_file() {
            return Err(PluginError::Io {
                message: format!("native entry `{}` does not exist", spec.entry.display()),
            });
        }
        if !spec.working_dir.is_dir() {
            return Err(PluginError::Io {
                message: format!(
                    "native working directory `{}` does not exist",
                    spec.working_dir.display()
                ),
            });
        }

        let mut command = Command::new(&spec.entry);
        command
            .args(&spec.args)
            .current_dir(&spec.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("EE_PLUGIN_ID", spec.plugin_id.as_str())
            .env("EE_PLUGIN_HOST", spec.host.as_str());
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|error| PluginError::Io {
            message: format!("cannot spawn `{}`: {error}", spec.entry.display()),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| PluginError::Transport {
            message: "child stdin is unavailable".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| PluginError::Transport {
            message: "child stdout is unavailable".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| PluginError::Transport {
            message: "child stderr is unavailable".to_owned(),
        })?;

        let (sender, receiver) = mpsc::channel();
        let transport = spec.transport;
        let reader = thread::Builder::new()
            .name(format!("ee-plugin-stdout-{}", spec.plugin_id))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_frame(&mut reader, transport) {
                        Ok(Some(message)) => {
                            if sender.send(Ok(message)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            break;
                        }
                    }
                }
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start plugin reader thread: {error}"),
            })?;

        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_handle = Arc::clone(&stderr_text);
        let stderr_reader = thread::Builder::new()
            .name(format!("ee-plugin-stderr-{}", spec.plugin_id))
            .spawn(move || read_stderr(stderr, stderr_handle))
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start plugin stderr thread: {error}"),
            })?;

        let rpc_timeout = Duration::from_millis(spec.limits.rpc_timeout_ms.max(1));
        let mut client = Self {
            child,
            stdin: Some(stdin),
            receiver,
            reader: Some(reader),
            stderr_text,
            stderr_reader: Some(stderr_reader),
            transport,
            adapter,
            sink,
            contributions: ContributionSession::new(),
            health: None,
            rpc_timeout,
            next_request_id: 1,
            last_contribution_revision: 0,
            plugin_id: spec.plugin_id.clone(),
            stopped: false,
        };

        let initialize = InitializeParams {
            plugin_id: spec.plugin_id,
            host: spec.host,
            api: ee_plugin_api::manifest::API_VERSION.to_owned(),
            services: spec.services,
            config: spec.config,
            limits: spec.limits,
        };
        client.initialize(initialize)?;
        Ok(client)
    }

    fn initialize(&mut self, params: InitializeParams) -> Result<(), PluginError> {
        self.send(&HostToPlugin::Initialize(params))?;
        let deadline = Instant::now() + self.rpc_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PluginError::Transport {
                    message: "initialize handshake timed out".to_owned(),
                });
            }
            let message = self.recv(remaining)?;
            match message {
                PluginToHost::Response(response)
                    if response.request_id == INITIALIZE_REQUEST_ID =>
                {
                    if let Some(error) = response.error {
                        return Err(PluginError::Transport {
                            message: format!("initialize failed: {}", error.message),
                        });
                    }
                    return Ok(());
                }
                other => self.handle_side_message(other)?,
            }
        }
    }

    /// Invoke a command and wait for its response.
    pub fn invoke(&mut self, id: &CommandId, args: Value) -> Result<Value, PluginError> {
        self.invoke_view(id, ee_plugin_api::model::ViewId(0), args)
    }

    /// Invoke a command for one view and wait for its response.
    pub fn invoke_view(
        &mut self,
        id: &CommandId,
        view_id: ee_plugin_api::model::ViewId,
        args: Value,
    ) -> Result<Value, PluginError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let deadline_ms = self.rpc_timeout.as_millis() as u64;
        self.send(&HostToPlugin::Command(CommandParams {
            id: id.clone(),
            view_id,
            request_id,
            args,
            deadline_ms,
        }))?;

        let deadline = Instant::now() + self.rpc_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PluginError::Transport {
                    message: format!("command `{id}` timed out"),
                });
            }
            let message = self.recv(remaining)?;
            match message {
                PluginToHost::Response(response) if response.request_id == request_id => {
                    if let Some(error) = response.error {
                        return Err(PluginError::Transport { message: error.message });
                    }
                    return Ok(response.result.unwrap_or(Value::Null));
                }
                PluginToHost::Response(_) => {}
                other => self.handle_side_message(other)?,
            }
        }
    }

    /// Deliver an event without waiting for a response.
    pub fn send_event(
        &mut self,
        name: impl Into<String>,
        revision: u64,
        payload: Value,
    ) -> Result<(), PluginError> {
        self.send(&HostToPlugin::Event(EventParams { name: name.into(), revision, payload }))
    }

    /// Request a fresh contribution snapshot.
    pub fn request_contributions(&mut self, revision: u64) -> Result<(), PluginError> {
        self.send(&HostToPlugin::ContributionRequest(ContributionRequestParams { revision }))
    }

    /// Read one raw message from the plugin. Returns `None` on timeout.
    ///
    /// Effects and contributions are **not** applied by this method; use it for
    /// tests that assert on wire data. Normal command dispatch uses
    /// [`NativeClient::invoke`], which applies side messages.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<PluginToHost>, PluginError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(Some(message)),
            Ok(Err(error)) => Err(PluginError::Transport { message: error }),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(PluginError::Transport {
                message: format!(
                    "plugin `{}` closed stdout{}",
                    self.plugin_id,
                    self.stderr_suffix()
                ),
            }),
        }
    }

    /// Drain pending messages for a bounded window.
    ///
    /// Native event handling may synchronously call a host service before the
    /// next idle cycle. Waiting a short, bounded time after an event write keeps
    /// those calls from deadlocking against the host's non-blocking pump.
    pub fn pump_for(&mut self, budget: Duration) -> Result<usize, PluginError> {
        let deadline = Instant::now() + budget;
        let mut processed = 0;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(Ok(message)) => {
                    processed += 1;
                    match message {
                        PluginToHost::Response(_) => {}
                        other => self.handle_side_message(other)?,
                    }
                }
                Ok(Err(error)) => return Err(PluginError::Transport { message: error }),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PluginError::Transport {
                        message: format!(
                            "plugin `{}` closed stdout{}",
                            self.plugin_id,
                            self.stderr_suffix()
                        ),
                    });
                }
            }
        }
        Ok(processed)
    }

    /// Drain pending messages without blocking.
    ///
    /// Side messages (effects, contributions, health, service requests) are
    /// applied on the caller thread. Command responses are ignored here; call
    /// [`NativeClient::invoke`] when a response is expected.
    pub fn pump(&mut self) -> Result<usize, PluginError> {
        let mut processed = 0;
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(message)) => {
                    processed += 1;
                    match message {
                        PluginToHost::Response(_) => {}
                        other => self.handle_side_message(other)?,
                    }
                }
                Ok(Err(error)) => return Err(PluginError::Transport { message: error }),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(PluginError::Transport {
                        message: format!(
                            "plugin `{}` closed stdout{}",
                            self.plugin_id,
                            self.stderr_suffix()
                        ),
                    });
                }
            }
        }
        Ok(processed)
    }

    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Process id of the running child.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn health(&self) -> Option<&HealthReport> {
        self.health.as_ref()
    }

    pub fn contribution_ids(&self) -> Vec<ee_plugin_api::ids::ContributionId> {
        self.contributions.ids()
    }

    /// Bounded stderr text captured from the child.
    pub fn stderr(&self) -> String {
        self.stderr_text.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Whether the child is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&mut self, message: &HostToPlugin) -> Result<(), PluginError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| PluginError::Transport {
            message: "plugin stdin is closed".to_owned(),
        })?;
        write_frame(stdin, self.transport, message)
    }

    fn recv(&mut self, timeout: Duration) -> Result<PluginToHost, PluginError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => Err(PluginError::Transport { message: error }),
            Err(RecvTimeoutError::Timeout) => {
                Err(PluginError::Transport { message: "plugin response timed out".to_owned() })
            }
            Err(RecvTimeoutError::Disconnected) => Err(PluginError::Transport {
                message: format!(
                    "plugin `{}` closed stdout{}",
                    self.plugin_id,
                    self.stderr_suffix()
                ),
            }),
        }
    }

    fn handle_side_message(&mut self, message: PluginToHost) -> Result<(), PluginError> {
        match message {
            PluginToHost::Effect(EffectEnvelope { effects, .. }) => {
                self.adapter.submit_effects(Some(&self.plugin_id), effects)
            }
            PluginToHost::Contribution(envelope) => self.apply_contributions(envelope),
            PluginToHost::Health(report) => {
                self.health = Some(report);
                Ok(())
            }
            PluginToHost::Request(request) => {
                let response = self.adapter.handle_service_request(&self.plugin_id, &request);
                match response {
                    Ok(result) => self.respond_ok(request.request_id, result),
                    Err(error) => self.respond_error(
                        request.request_id,
                        error.class().as_str(),
                        error.code(),
                        &error.to_string(),
                    ),
                }
            }
            PluginToHost::Response(_) => Ok(()),
        }
    }

    fn apply_contributions(&mut self, envelope: ContributionEnvelope) -> Result<(), PluginError> {
        if envelope.revision < self.last_contribution_revision {
            return Err(PluginError::Transport {
                message: format!(
                    "stale contribution revision {} (last {})",
                    envelope.revision, self.last_contribution_revision
                ),
            });
        }
        self.last_contribution_revision = envelope.revision;
        for id in envelope.remove {
            self.contributions.remove(&id);
        }
        for contribution in envelope.upsert {
            self.contributions.upsert(self.sink.as_ref(), contribution)?;
        }
        Ok(())
    }

    fn respond_error(
        &mut self,
        request_id: u64,
        class: &str,
        code: &str,
        message: &str,
    ) -> Result<(), PluginError> {
        self.send(&HostToPlugin::Response(PluginResponse {
            request_id,
            result: None,
            error: Some(WireError {
                class: class.to_owned(),
                code: code.to_owned(),
                message: message.to_owned(),
                retryable: false,
            }),
        }))
    }

    fn respond_ok(&mut self, request_id: u64, result: Value) -> Result<(), PluginError> {
        self.send(&HostToPlugin::Response(PluginResponse {
            request_id,
            result: Some(result),
            error: None,
        }))
    }

    fn stderr_suffix(&self) -> String {
        let stderr = self.stderr();
        if stderr.is_empty() { String::new() } else { format!(": {}", stderr.trim_end()) }
    }

    /// Send shutdown, wait briefly, then kill if necessary.
    pub fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self
            .send(&HostToPlugin::Shutdown(ShutdownParams { reason: "host shutdown".to_owned() }));
        self.stdin = None;

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shutdown();
        self.contributions.dispose_all();
    }
}

fn write_frame(
    writer: &mut impl Write,
    transport: LaunchTransport,
    message: &HostToPlugin,
) -> Result<(), PluginError> {
    let payload = serde_json::to_vec(message)
        .map_err(|error| PluginError::Transport { message: error.to_string() })?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(PluginError::Transport {
            message: format!("outgoing frame exceeds {MAX_FRAME_BYTES} bytes"),
        });
    }
    match transport {
        LaunchTransport::StdioNewline => {
            writer.write_all(&payload)?;
            writer.write_all(b"\n")?;
        }
        LaunchTransport::StdioContentLength => {
            writer.write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())?;
            writer.write_all(&payload)?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn read_frame(
    reader: &mut impl BufRead,
    transport: LaunchTransport,
) -> Result<Option<PluginToHost>, String> {
    let payload = match transport {
        LaunchTransport::StdioNewline => {
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                let read =
                    reader.read_until(b'\n', &mut buffer).map_err(|error| error.to_string())?;
                if read == 0 {
                    return Ok(None);
                }
                if buffer.len() > MAX_FRAME_BYTES {
                    return Err(format!("incoming frame exceeds {MAX_FRAME_BYTES} bytes"));
                }
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                if !buffer.is_empty() {
                    break buffer;
                }
            }
        }
        LaunchTransport::StdioContentLength => {
            let mut length = None;
            loop {
                let mut header = String::new();
                let read = reader.read_line(&mut header).map_err(|error| error.to_string())?;
                if read == 0 {
                    return Ok(None);
                }
                let header = header.trim_end_matches(['\r', '\n']);
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Content-Length:") {
                    length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| format!("invalid Content-Length: {value:?}"))?,
                    );
                }
            }
            let length = length.ok_or_else(|| "missing Content-Length header".to_owned())?;
            if length > MAX_FRAME_BYTES {
                return Err(format!("incoming frame exceeds {MAX_FRAME_BYTES} bytes"));
            }
            let mut payload = vec![0_u8; length];
            reader.read_exact(&mut payload).map_err(|error| format!("truncated frame: {error}"))?;
            payload
        }
    };

    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| format!("malformed plugin message: {error}"))
}

fn read_stderr(mut stderr: impl Read, target: Arc<Mutex<String>>) {
    let mut buffer = [0_u8; 2048];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut target) = target.lock() {
                    let remaining = MAX_STDERR_BYTES.saturating_sub(target.len());
                    if remaining == 0 {
                        continue;
                    }
                    let take = read.min(remaining);
                    let chunk = String::from_utf8_lossy(&buffer[..take]);
                    let mut end = chunk.len();
                    while end > 0 && !chunk.is_char_boundary(end) {
                        end -= 1;
                    }
                    target.push_str(&chunk[..end]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn plugin_message() -> PluginToHost {
        PluginToHost::Health(HealthReport { state: "loaded".to_owned(), reason: None })
    }

    #[test]
    fn newline_framing_round_trips() {
        let message = plugin_message();
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        let mut reader = Cursor::new(bytes);
        let decoded = read_frame(&mut reader, LaunchTransport::StdioNewline).unwrap().unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn content_length_framing_round_trips() {
        let message = plugin_message();
        let payload = serde_json::to_vec(&message).unwrap();
        let mut bytes = format!("Content-Length: {}\r\n\r\n", payload.len()).into_bytes();
        bytes.extend(payload);
        let mut reader = Cursor::new(bytes);
        let decoded =
            read_frame(&mut reader, LaunchTransport::StdioContentLength).unwrap().unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn empty_lines_are_skipped_and_eof_is_clean() {
        let message = plugin_message();
        let mut bytes = vec![b'\n', b'\r', b'\n'];
        bytes.extend(serde_json::to_vec(&message).unwrap());
        bytes.push(b'\n');
        let mut reader = Cursor::new(bytes);
        assert!(read_frame(&mut reader, LaunchTransport::StdioNewline).unwrap().is_some());
        assert!(read_frame(&mut reader, LaunchTransport::StdioNewline).unwrap().is_none());
    }

    #[test]
    fn malformed_message_is_reported() {
        let mut reader = Cursor::new(b"not json\n".to_vec());
        let error = read_frame(&mut reader, LaunchTransport::StdioNewline).unwrap_err();
        assert!(error.contains("malformed"));
    }
}
