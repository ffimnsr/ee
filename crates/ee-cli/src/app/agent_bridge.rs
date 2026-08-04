//! Phase 4 bridge: ACP `fs/*` and `terminal/*` client methods against editor
//! buffers, the existing save pipeline, and tracked terminal processes.
//!
//! The host's [`BridgeUiHandler`] forwards file and terminal-create requests
//! to the agents pane (approval UI, buffer reads/writes, terminal spawning
//! all live in `ee-cli`); terminal output/wait/kill/release operate on a
//! shared [`AgentTerminals`] registry from the host worker thread.  Every
//! buffer read/write is recorded in the action log for future
//! checkpoint/restore.
//!
//! Policy invariants (fail closed):
//! - ACP paths must be absolute; reads resolve against open buffers first
//!   (unsaved text wins over disk) and fall back to workspace-scoped disk
//!   reads.  Paths outside the workspace are rejected.
//! - Writes always route through buffer open → edit → save semantics; the
//!   diff is recomputed against the latest snapshot and a conflict error is
//!   returned when the buffer cannot converge.
//! - VLF buffers reject unbounded reads and all writes.
//! - Terminals run with an explicit command/args (never a shell string),
//!   inherit no secret-like environment variables, and their output is
//!   bounded by both the ACP request limit and an editor-side hard cap.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ee_agent_host::{
    AgentError, ClientRequest, ClientRequestHandler, ClientRequestResponse, ClientRequestResult,
    HandlerCapabilities,
};
use ee_agent_protocol::{
    CreateElicitationRequest, CreateTerminalRequest, CreateTerminalResponse, ElicitationScope,
    EnvVariable, KillTerminalRequest, KillTerminalResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, SessionId,
    TerminalExitStatus, TerminalId, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use globset::Glob;
use ignore::WalkBuilder;
use similar::TextDiff;
use tokio::sync::oneshot;

use super::*;

// ── Policy constants ─────────────────────────────────────────────────────────

/// Hard cap on lines served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_LINES: usize = 100_000;
/// Hard cap on bytes served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_BYTES: usize = 1024 * 1024;
/// Editor-side hard cap on retained terminal output bytes.
pub(crate) const BRIDGE_TERMINAL_OUTPUT_CAP: usize = 1024 * 1024;
/// How many bytes each terminal output reader flushes per read.
const TERMINAL_READER_CHUNK: usize = 4096;
/// Cap on entries returned by one `ee.list_directory` call.
const PROXY_LIST_DIRECTORY_LIMIT: usize = 500;
/// Cap on matches returned by one `ee.search_files` call.
const PROXY_SEARCH_FILES_LIMIT: usize = 500;
/// Cap on matches returned by one `ee.search_text` call.
const PROXY_SEARCH_TEXT_LIMIT: usize = 200;
/// Max visible context bytes returned for one `ee.search_text` match.
const PROXY_SEARCH_TEXT_CONTEXT_BYTES: usize = 200;
/// Cap on diagnostics returned by one Phase 3 diagnostics tool.
const PROXY_DIAGNOSTICS_LIMIT: usize = 500;
/// Cap on document symbols returned by one `ee.document_symbols` call.
const PROXY_DOCUMENT_SYMBOLS_LIMIT: usize = 500;
/// Cap on references returned by one `ee.references` call.
const PROXY_REFERENCES_LIMIT: usize = 500;
/// Cap on code actions returned by one `ee.list_code_actions` call.
const PROXY_CODE_ACTIONS_LIMIT: usize = 100;
/// Cap on files returned by one rename preview.
const PROXY_RENAME_FILES_LIMIT: usize = 100;
/// Cap on edits returned by one rename preview.
const PROXY_RENAME_EDITS_LIMIT: usize = 1000;
/// Max regex pattern length accepted by `ee.search_text_regex`.
const PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES: usize = 4096;
/// Max wall time spent in one regex search before fail-closed timeout.
const PROXY_SEARCH_REGEX_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_TERMINAL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// ── Secret handling ──────────────────────────────────────────────────────────

/// Whether an environment variable name looks secret-like.
///
/// Delegates to the shared host policy (single source of truth).
pub(crate) fn is_secret_env_key(name: &str) -> bool {
    ee_agent_host::redact::is_secret_key(name)
}

/// Environment display values with secret values redacted (approval UI).
pub(crate) fn redact_env_display(env: &[EnvVariable]) -> Vec<(String, String)> {
    env.iter()
        .map(|variable| {
            (
                variable.name.clone(),
                ee_agent_host::redact::redact_pair(&variable.name, &variable.value),
            )
        })
        .collect()
}

/// Child environment: the parent environment minus secret-like keys, overlaid
/// with the explicitly configured request values.
fn sanitized_child_env(request_env: &[EnvVariable]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        std::env::vars().filter(|(name, _)| !is_secret_env_key(name)).collect();
    for variable in request_env {
        env.push((variable.name.clone(), variable.value.clone()));
    }
    env
}

// ── Bounded output ring buffer ───────────────────────────────────────────────

/// Byte-bounded output buffer: truncates from the front on overflow so the
/// final visible output is always preserved, at a char boundary.
#[derive(Debug, Default)]
pub(crate) struct BoundedOutput {
    bytes: Vec<u8>,
    total: u64,
    truncated: bool,
    cap: usize,
}

impl BoundedOutput {
    /// Creates an empty buffer with `cap` max retained bytes.
    #[must_use]
    pub(crate) fn new(cap: usize) -> Self {
        Self { bytes: Vec::new(), total: 0, truncated: false, cap }
    }

    /// Appends one chunk, enforcing the cap.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        self.bytes.extend_from_slice(chunk);
        while self.bytes.len() > self.cap {
            let mut cut = self.bytes.len() - self.cap;
            // Advance to a UTF-8 char boundary so the retained output stays
            // valid string content.
            while cut < self.bytes.len() && self.bytes[cut] & 0xC0 == 0x80 {
                cut += 1;
            }
            self.bytes.drain(..cut);
            self.truncated = true;
        }
    }

    /// Retained output as lossy string.
    #[must_use]
    pub(crate) fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Whether any bytes were dropped by the cap.
    #[must_use]
    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// Total bytes ever pushed.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn total(&self) -> u64 {
        self.total
    }
}

// ── Tracked terminals ────────────────────────────────────────────────────────

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

fn exit_status_of(status: &std::process::ExitStatus) -> TerminalExitStatus {
    TerminalExitStatus::new()
        .exit_code(status.code().map(|code| code as u32))
        .signal(signal_of(status))
}

/// One tracked agent terminal: child process, bounded output, exit status.
#[derive(Debug)]
pub(crate) struct AgentTerminalTrack {
    #[allow(dead_code)]
    pub(crate) terminal_id: String,
    #[allow(dead_code)]
    pub(crate) owner_session_id: String,
    #[allow(dead_code)]
    pub(crate) command: String,
    #[allow(dead_code)]
    pub(crate) args: Vec<String>,
    #[allow(dead_code)]
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) output: Arc<Mutex<BoundedOutput>>,
    child: Option<Child>,
    pub(crate) exit_status: Option<TerminalExitStatus>,
    pub(crate) released: bool,
}

#[derive(Debug, Default)]
struct AgentTerminalRegistry {
    active: HashMap<String, AgentTerminalTrack>,
    released: HashMap<String, AgentTerminalTrack>,
}

/// Shared registry of agent terminals (UI spawns, worker queries/kills).
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentTerminals {
    inner: Arc<Mutex<AgentTerminalRegistry>>,
}

impl AgentTerminals {
    /// Spawns and registers a terminal from an approved request.
    ///
    /// # Errors
    ///
    /// Fails when the command is empty, the cwd is relative, spawning fails,
    /// or the generated id already exists.
    pub(crate) fn spawn(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, AgentError> {
        if request.command.trim().is_empty() {
            return Err(AgentError::invalid_params("terminal command must not be empty"));
        }
        if let Some(cwd) = &request.cwd
            && !cwd.is_absolute()
        {
            return Err(AgentError::invalid_params(format!(
                "terminal cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        let cap = request
            .output_byte_limit
            .map(|limit| (limit as usize).min(BRIDGE_TERMINAL_OUTPUT_CAP))
            .unwrap_or(BRIDGE_TERMINAL_OUTPUT_CAP);
        let terminal_id = self.allocate_terminal_id();

        let mut command = Command::new(&request.command);
        command.args(&request.args).envs(sanitized_child_env(&request.env));
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| AgentError::Io(format!("terminal spawn failed: {error}")))?;

        let output = Arc::new(Mutex::new(BoundedOutput::new(cap)));
        spawn_output_reader(child.stdout.take(), Arc::clone(&output));
        spawn_output_reader(child.stderr.take(), Arc::clone(&output));

        let track = AgentTerminalTrack {
            terminal_id: terminal_id.clone(),
            owner_session_id: request.session_id.0.to_string(),
            command: request.command.clone(),
            args: request.args.clone(),
            cwd: request.cwd.clone(),
            output,
            child: Some(child),
            exit_status: None,
            released: false,
        };
        let mut registry = self.inner.lock().expect("terminals poisoned");
        if registry.active.contains_key(&terminal_id)
            || registry.released.contains_key(&terminal_id)
        {
            return Err(AgentError::HandlerError("terminal id collision".into()));
        }
        registry.active.insert(terminal_id.clone(), track);
        Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
    }

    /// Snapshot of the retained output and current exit status.
    ///
    /// # Errors
    ///
    /// Fails when the terminal id is unknown.
    pub(crate) fn output(
        &self,
        request: &TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(track, &request.session_id)?;
        self.refresh_exit(track);
        Ok(Self::output_response(track))
    }

    /// Waits for the terminal to exit (async polling; cancellable by dropping
    /// the awaiting handler future).
    ///
    /// # Errors
    ///
    /// Fails with `InvalidParams` for unknown terminals.
    pub(crate) async fn wait_for_exit(
        &self,
        request: &WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, AgentError> {
        loop {
            {
                let mut registry = self.inner.lock().expect("terminals poisoned");
                let Some(track) = registry.active.get_mut(request.terminal_id.0.as_ref()) else {
                    return Err(AgentError::invalid_params("unknown terminal"));
                };
                self.validate_owner(track, &request.session_id)?;
                self.refresh_exit(track);
                if let Some(exit_status) = track.exit_status.clone() {
                    return Ok(WaitForTerminalExitResponse::new(exit_status));
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Kills the terminal process and reaps it.
    ///
    /// # Errors
    ///
    /// Fails for unknown terminals or when the kill fails.
    pub(crate) fn kill(
        &self,
        request: &KillTerminalRequest,
    ) -> Result<KillTerminalResponse, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(track, &request.session_id)?;
        self.kill_track(track)?;
        Ok(KillTerminalResponse::new())
    }

    /// Releases host tracking; kills the process when still running.
    ///
    /// # Errors
    ///
    /// Fails for unknown terminals.
    pub(crate) fn release(
        &self,
        request: &ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(mut track) = registry.active.remove(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(&track, &request.session_id)?;
        self.kill_track(&mut track)?;
        track.released = true;
        registry.released.insert(track.terminal_id.clone(), track);
        Ok(ReleaseTerminalResponse::new())
    }

    fn allocate_terminal_id(&self) -> String {
        loop {
            let tick = NEXT_TERMINAL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let pid = u128::from(std::process::id());
            let candidate = format!("term-{:032x}", now ^ (pid << 32) ^ tick);
            let registry = self.inner.lock().expect("terminals poisoned");
            let taken = registry.active.contains_key(&candidate)
                || registry.released.contains_key(&candidate);
            drop(registry);
            if !taken {
                return candidate;
            }
        }
    }

    fn validate_owner(
        &self,
        track: &AgentTerminalTrack,
        session_id: &SessionId,
    ) -> Result<(), AgentError> {
        if track.owner_session_id == session_id.0.as_ref() {
            Ok(())
        } else {
            Err(AgentError::invalid_params("terminal does not belong to this session"))
        }
    }

    fn kill_track(&self, track: &mut AgentTerminalTrack) -> Result<(), AgentError> {
        if let Some(child) = track.child.as_mut() {
            child
                .kill()
                .map_err(|error| AgentError::Io(format!("terminal kill failed: {error}")))?;
            let status = child
                .wait()
                .map_err(|error| AgentError::Io(format!("terminal wait failed: {error}")))?;
            track.exit_status = Some(exit_status_of(&status));
            track.child = None;
        }
        Ok(())
    }

    fn output_response(track: &AgentTerminalTrack) -> TerminalOutputResponse {
        let output = track.output.lock().expect("output poisoned");
        let mut response = TerminalOutputResponse::new(output.as_string(), output.truncated());
        response.exit_status = track.exit_status.clone();
        response
    }

    /// Reaps the child when it exited and caches the exit status.
    fn refresh_exit(&self, track: &mut AgentTerminalTrack) {
        if track.exit_status.is_none()
            && let Some(child) = track.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            track.exit_status = Some(exit_status_of(&status));
            track.child = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn display_output(&self, terminal_id: &str) -> Option<TerminalOutputResponse> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        if let Some(track) = registry.active.get_mut(terminal_id) {
            self.refresh_exit(track);
            return Some(Self::output_response(track));
        }
        registry.released.get(terminal_id).map(Self::output_response)
    }

    /// Kills every tracked terminal and clears the registry (app shutdown).
    pub(crate) fn kill_all(&self) {
        let registry = std::mem::take(&mut *self.inner.lock().expect("terminals poisoned"));
        for (_, mut track) in registry.active.into_iter().chain(registry.released) {
            if let Some(mut child) = track.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Number of tracked terminals (tests and status lines).
    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.inner.lock().expect("terminals poisoned").active.len()
    }
}

fn spawn_output_reader(
    stream: Option<impl std::io::Read + Send + 'static>,
    output: Arc<Mutex<BoundedOutput>>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    std::thread::Builder::new()
        .name(String::from("ee-agent-terminal-output"))
        .spawn(move || {
            let mut buffer = [0u8; TERMINAL_READER_CHUNK];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => output.lock().expect("output poisoned").push(&buffer[..n]),
                }
            }
        })
        .expect("spawn terminal output reader");
}

// ── Handler → pane messages ──────────────────────────────────────────────────

/// One agent-to-client request forwarded to the pane.
pub(crate) enum BridgeUiMessage {
    ReadFile {
        request: ReadTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    WriteFile {
        request: WriteTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    TerminalCreate {
        request: CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    Elicitation {
        session_id: Option<SessionId>,
        request: CreateElicitationRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    /// A tool call from the ee MCP proxy (Phase 6).  Writes and terminal
    /// creates queue the same approval prompts as direct ACP client methods;
    /// reads and diagnostics are served immediately.
    ProxyTool {
        call: super::agents_mcp::ProxyToolCall,
        reply: oneshot::Sender<ClientRequestResult>,
    },
}

async fn forward_and_await(
    tx: std_mpsc::Sender<BridgeUiMessage>,
    make: impl FnOnce(oneshot::Sender<ClientRequestResult>) -> BridgeUiMessage,
) -> ClientRequestResult {
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send(make(reply_tx)).is_err() {
        return Err(AgentError::Cancelled);
    }
    match reply_rx.await {
        Ok(result) => result,
        Err(_) => Err(AgentError::Cancelled),
    }
}

/// Host handler: file requests and terminal creation are approved and
/// executed by the pane; terminal output/wait/kill/release run against the
/// shared registry on this (worker) thread.
pub(crate) struct BridgeUiHandler {
    tx: std_mpsc::Sender<BridgeUiMessage>,
    terminals: AgentTerminals,
}

impl BridgeUiHandler {
    #[must_use]
    pub(crate) fn new(tx: std_mpsc::Sender<BridgeUiMessage>, terminals: AgentTerminals) -> Self {
        Self { tx, terminals }
    }

    /// Exact editor-backed capability set wired by agents mode.
    ///
    /// Keep this explicit instead of using `HandlerCapabilities::all()` so new
    /// capability bits never become advertised in production before the editor
    /// bridge actually implements them.
    #[must_use]
    pub(crate) const fn editor_capabilities() -> HandlerCapabilities {
        HandlerCapabilities {
            fs_read: true,
            fs_write: true,
            terminal: true,
            elicitation_form: true,
            elicitation_url: true,
            session_config_boolean: true,
            proxy_discovery: true,
        }
    }
}

impl ClientRequestHandler for BridgeUiHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        Self::editor_capabilities()
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            match request {
                ClientRequest::ProxyWorkspaceRoots => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::WorkspaceRoots,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectory { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ListDirectory { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectoryAll { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ListDirectoryAll { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFiles { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::SearchFiles { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFilesAll { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::SearchFilesAll { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchText { query } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::SearchText { query },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextRegex { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::SearchTextRegex { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextInFiles { query, file_glob } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::SearchTextInFiles {
                            query,
                            file_glob,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReplaceText { path, old_text, new_text } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ReplaceText {
                            path,
                            old_text,
                            new_text,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyApplyPatch { path, edits } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ApplyPatch { path, edits },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCreateTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::CreateTextFile { path, content },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyOverwriteTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::OverwriteTextFile { path, content },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBuffer { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ReadBuffer { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBufferLines { path, line, limit } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ReadBufferLines {
                            path,
                            line,
                            limit,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyOpenBuffers => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::OpenBuffers,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetDiagnostics => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::GetDiagnostics,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetFileDiagnostics { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::GetFileDiagnostics { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyDocumentSymbols { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::DocumentSymbols { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReferences { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::References {
                            path,
                            line,
                            character,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListCodeActions { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ListCodeActions {
                            path,
                            line,
                            character,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyApplyCodeAction { path, action_id } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::ApplyCodeAction { path, action_id },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFormatFile { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::FormatFile { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyPreviewRenameSymbol { path, line, character, new_name } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::PreviewRenameSymbol {
                            path,
                            line,
                            character,
                            new_name,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyRenameSymbol { path, line, character, new_name } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        call: super::agents_mcp::ProxyToolCall::RenameSymbol {
                            path,
                            line,
                            character,
                            new_name,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ReadTextFile(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ReadFile {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::WriteTextFile(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::WriteFile {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::CreateTerminal(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::TerminalCreate {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::TerminalOutput(request) => {
                    self.terminals.output(&request).map(ClientRequestResponse::TerminalOutput)
                }
                ClientRequest::WaitForTerminalExit(request) => self
                    .terminals
                    .wait_for_exit(&request)
                    .await
                    .map(ClientRequestResponse::WaitForTerminalExit),
                ClientRequest::KillTerminal(request) => {
                    self.terminals.kill(&request).map(ClientRequestResponse::KillTerminal)
                }
                ClientRequest::ReleaseTerminal(request) => {
                    self.terminals.release(&request).map(ClientRequestResponse::ReleaseTerminal)
                }
                ClientRequest::CreateElicitation(request) => {
                    let session_id = match request.scope() {
                        ElicitationScope::Session(scope) => Some(scope.session_id.clone()),
                        _ => None,
                    };
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::Elicitation {
                        session_id,
                        request,
                        reply,
                    })
                    .await
                }
            }
        })
    }
}

// ── Approval prompt ──────────────────────────────────────────────────────────

/// The operation awaiting an explicit user decision.
#[derive(Debug, Clone)]
enum WriteExpectation {
    Blind,
    MustNotExist,
    ExpectRevision(String),
}

#[derive(Debug, Clone, Copy)]
enum WriteReplyKind {
    FsWrite,
    ProxyStructured,
}

#[derive(Debug, Clone)]
struct PreparedWrite {
    path: PathBuf,
    content: String,
    tool_call_id: Option<String>,
    expectation: WriteExpectation,
    reply_kind: WriteReplyKind,
    proxy_edit_count: u32,
}

#[derive(Debug)]
struct ProxyWriteSpec {
    title: String,
    detail: String,
    prepared: PreparedWrite,
}

#[derive(Debug)]
enum ApprovalKind {
    Write {
        path: PathBuf,
        content: String,
        tool_call_id: Option<String>,
        expectation: WriteExpectation,
        reply_kind: WriteReplyKind,
        proxy_edit_count: u32,
    },
    WriteBatch {
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
    },
    TerminalCreate {
        request: CreateTerminalRequest,
    },
}

/// One approval decision the user can pick (Phase 7 policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    /// Allow this operation only; identical future operations ask again.
    AllowOnce,
    /// Allow this and every identical operation for the rest of the session.
    AllowSession,
    /// Deny this operation only.
    DenyOnce,
    /// Deny this and every identical operation for the rest of the session.
    DenySession,
}

impl ApprovalChoice {
    fn label(self) -> &'static str {
        match self {
            ApprovalChoice::AllowOnce => "Allow once",
            ApprovalChoice::AllowSession => "Allow session",
            ApprovalChoice::DenyOnce => "Deny",
            ApprovalChoice::DenySession => "Deny session",
        }
    }

    fn allows(self) -> bool {
        matches!(self, ApprovalChoice::AllowOnce | ApprovalChoice::AllowSession)
    }
}

/// In-memory approval policy (Phase 7).
///
/// `allow_once` / `deny_once` decisions are not recorded; `allow_session` /
/// `deny_session` decisions are remembered per session, keyed by action kind
/// and fingerprint (path for writes, command+args fingerprint for
/// terminals), and invalidated when the session closes.  Allow-always
/// persistence is deliberately not implemented: the config writer has no
/// safe update path for it, so the option does not exist at the schema
/// level.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApprovalPolicy {
    /// `(session_id, fingerprint)` entries auto-allowed for the session.
    allowed: std::collections::BTreeSet<(String, String)>,
    /// `(session_id, fingerprint)` entries auto-denied for the session.
    denied: std::collections::BTreeSet<(String, String)>,
}

impl ApprovalPolicy {
    /// Auto-decision for `(session_id, fingerprint)`, if recorded.
    fn lookup(&self, session_id: &str, fingerprint: &str) -> Option<ApprovalChoice> {
        if self.denied.contains(&(session_id.to_string(), fingerprint.to_string())) {
            return Some(ApprovalChoice::DenySession);
        }
        if self.allowed.contains(&(session_id.to_string(), fingerprint.to_string())) {
            return Some(ApprovalChoice::AllowSession);
        }
        None
    }

    /// Records a session-scoped decision.
    fn record(&mut self, session_id: &str, fingerprint: &str, choice: ApprovalChoice) {
        let key = (session_id.to_string(), fingerprint.to_string());
        match choice {
            ApprovalChoice::AllowSession => {
                self.allowed.insert(key);
            }
            ApprovalChoice::DenySession => {
                self.denied.insert(key);
            }
            ApprovalChoice::AllowOnce | ApprovalChoice::DenyOnce => {}
        }
    }

    /// Drops every recorded decision for `session_id` (session close).
    pub(crate) fn invalidate_session(&mut self, session_id: &str) {
        self.allowed.retain(|(session, _)| session != session_id);
        self.denied.retain(|(session, _)| session != session_id);
    }
}

/// Fingerprint for one approval operation: action kind + stable identity.
fn approval_fingerprint(kind: &ApprovalKind) -> String {
    match kind {
        ApprovalKind::Write { path, .. } => format!("write:{}", path.display()),
        ApprovalKind::WriteBatch { writes, .. } => format!(
            "write-batch:{}",
            writes
                .iter()
                .map(|write| write.path.display().to_string())
                .collect::<Vec<_>>()
                .join("|")
        ),
        ApprovalKind::TerminalCreate { request } => {
            let command = [request.command.clone()]
                .into_iter()
                .chain(request.args.clone())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            format!("terminal:{command}")
        }
    }
}

/// A pending file-write or terminal-create approval.
#[derive(Debug)]
pub(crate) struct ApprovalPrompt {
    pub(crate) thread_index: Option<usize>,
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// `(label, choice)` option list; the user picks one with Enter.
    pub(crate) options: Vec<(String, ApprovalChoice)>,
    pub(crate) selected: usize,
    kind: ApprovalKind,
    pub(crate) reply: oneshot::Sender<ClientRequestResult>,
}

impl ApprovalPrompt {
    fn write(
        thread_index: Option<usize>,
        session_id: &SessionId,
        request: &WriteTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self::write_with(
            thread_index,
            session_id,
            String::from("fs/write_text_file"),
            format!("{} ({} bytes)", request.path.display(), request.content.len()),
            PreparedWrite {
                path: request.path.clone(),
                content: request.content.clone(),
                tool_call_id: None,
                expectation: WriteExpectation::Blind,
                reply_kind: WriteReplyKind::FsWrite,
                proxy_edit_count: 0,
            },
            reply,
        )
    }

    fn proxy_write(spec: ProxyWriteSpec, reply: oneshot::Sender<ClientRequestResult>) -> Self {
        Self::write_with(
            None,
            &SessionId::new("proxy"),
            spec.title,
            spec.detail,
            spec.prepared,
            reply,
        )
    }

    fn write_with(
        thread_index: Option<usize>,
        session_id: &SessionId,
        title: String,
        detail: String,
        prepared: PreparedWrite,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            title,
            detail,
            options: approval_options(),
            selected: 0,
            kind: ApprovalKind::Write {
                path: prepared.path,
                content: prepared.content,
                tool_call_id: prepared.tool_call_id,
                expectation: prepared.expectation,
                reply_kind: prepared.reply_kind,
                proxy_edit_count: prepared.proxy_edit_count,
            },
            reply,
        }
    }

    fn proxy_write_batch(
        title: String,
        detail: String,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index: None,
            session_id: SessionId::new("proxy").0.to_string(),
            title,
            detail,
            options: approval_options(),
            selected: 0,
            kind: ApprovalKind::WriteBatch { writes, total_edit_count },
            reply,
        }
    }

    fn terminal(
        thread_index: Option<usize>,
        session_id: &SessionId,
        request: &CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let command = if request.args.is_empty() {
            request.command.clone()
        } else {
            format!("{} {}", request.command, request.args.join(" "))
        };
        let cwd = request
            .cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| String::from("(default)"));
        let env = redact_env_display(&request.env);
        let env_text = if env.is_empty() {
            String::from("(inherited, secrets filtered)")
        } else {
            env.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join(" ")
        };
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            title: String::from("terminal/create"),
            detail: format!("{command} · cwd: {cwd} · env: {env_text}"),
            options: approval_options(),
            selected: 0,
            kind: ApprovalKind::TerminalCreate { request: request.clone() },
            reply,
        }
    }
}

/// The fixed approval option list.  Allow-always is intentionally absent:
/// persisting it has no safe config-write path (Phase 7 policy).
fn approval_options() -> Vec<(String, ApprovalChoice)> {
    [
        ApprovalChoice::AllowOnce,
        ApprovalChoice::AllowSession,
        ApprovalChoice::DenyOnce,
        ApprovalChoice::DenySession,
    ]
    .into_iter()
    .map(|choice| (choice.label().to_string(), choice))
    .collect()
}

// ── Action log ───────────────────────────────────────────────────────────────

/// One recorded agent file operation (future checkpoint/restore source).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActionLogEntry {
    Read {
        path: PathBuf,
        bytes: usize,
        session_id: String,
    },
    Write {
        path: PathBuf,
        old_fingerprint: u64,
        new_fingerprint: u64,
        tool_call_id: Option<String>,
        session_id: String,
    },
}

/// FNV-1a content fingerprint (deterministic, non-cryptographic).
fn fingerprint(content: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug)]
struct BridgeWriteOutcome {
    old_content: String,
    byte_count: u64,
    new_revision: String,
    saved: bool,
    dirty: bool,
}

fn text_revision_id(content: &str) -> String {
    format!("{:016x}", fingerprint(content))
}

fn buffer_revision_id(buf: &crate::buffer::BufState) -> String {
    if buf.is_vlf {
        return format!(
            "vlf:{}:{}:{}",
            buf.vlf_generation, buf.vlf_cache_start_line, buf.vlf_approx_line_count
        );
    }
    text_revision_id(&buf.whole_text().unwrap_or_default())
}

fn buffer_saved_state(buf: &crate::buffer::BufState) -> bool {
    buf.save_complete && buf.last_save_succeeded && !buf.last_save_permission_denied
}

// ── Text helpers ─────────────────────────────────────────────────────────────

/// Splits file content into editor line model entries.
///
/// A trailing newline does not produce a phantom empty line (the backend
/// model stores lines without newline terminators).
fn split_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<String> = content.split('\n').map(str::to_owned).collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Line-diff hunks: `(old_start, old_end_exclusive, new_lines)`.
///
/// Adjacent changed regions merge into one hunk; a pure insertion reports
/// `old_start == old_end` (no old lines consumed).
fn diff_hunks(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize, Vec<String>)> {
    let old_text = old_lines.join("\n");
    let new_text = new_lines.join("\n");
    let diff = TextDiff::from_lines(&old_text, &new_text);

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(0) {
        let old_start = group.first().map(|op| op.old_range().start).unwrap_or(0);
        let old_end = group.last().map(|op| op.old_range().end).unwrap_or(old_start);
        let mut inserted = Vec::new();
        for op in &group {
            if matches!(op.tag(), similar::DiffTag::Insert | similar::DiffTag::Replace) {
                inserted.extend_from_slice(&new_lines[op.new_range()]);
            }
        }
        hunks.push((old_start, old_end, inserted));
    }
    hunks
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentDocumentSymbolsPayload {
    symbols: Vec<ee_mcp::DocumentSymbolEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentReferencesPayload {
    references: Vec<ee_mcp::ReferenceEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentCodeActionPayload {
    actions: Vec<AgentCodeActionPayloadEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentCodeActionPayloadEntry {
    title: String,
    kind: Option<String>,
    has_command: bool,
    edits: Vec<ee_mcp::PlannedTextEdit>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentTextEditsPayload {
    edits: Vec<ee_mcp::PlannedTextEdit>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRenamePayload {
    files: Vec<ee_mcp::PlannedFileEdit>,
}

fn utf16_column_to_byte_offset(text: &str, utf16_column: usize) -> usize {
    let mut utf16_seen = 0usize;
    for (byte, ch) in text.char_indices() {
        if utf16_seen >= utf16_column {
            return byte;
        }
        utf16_seen = utf16_seen.saturating_add(ch.len_utf16());
        if utf16_seen > utf16_column {
            return byte;
        }
    }
    text.len()
}

fn text_offset_for_range_position(
    text: &str,
    line: usize,
    character_utf16: usize,
) -> Result<usize, AgentError> {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(line_text) = lines.get(line) else {
        return Err(AgentError::invalid_params(format!(
            "line {} is beyond the end of the document",
            line + 1
        )));
    };
    let prefix =
        lines.iter().take(line).fold(0usize, |acc, value| acc.saturating_add(value.len() + 1));
    Ok(prefix.saturating_add(utf16_column_to_byte_offset(line_text, character_utf16)))
}

fn apply_planned_text_edits_to_content(
    content: &str,
    edits: &[ee_mcp::PlannedTextEdit],
) -> Result<String, AgentError> {
    let mut with_offsets = Vec::with_capacity(edits.len());
    for edit in edits {
        let start_line = edit.range.start_line.saturating_sub(1) as usize;
        let start_character = edit.range.start_character.saturating_sub(1) as usize;
        let end_line = edit.range.end_line.saturating_sub(1) as usize;
        let end_character = edit.range.end_character.saturating_sub(1) as usize;
        let start = text_offset_for_range_position(content, start_line, start_character)?;
        let end = text_offset_for_range_position(content, end_line, end_character)?;
        if start > end {
            return Err(AgentError::invalid_params("planned edit range is inverted"));
        }
        with_offsets.push((start, end, edit.new_text.as_str()));
    }
    with_offsets.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let mut next = content.to_string();
    for (start, end, replacement) in with_offsets {
        if start > next.len()
            || end > next.len()
            || !next.is_char_boundary(start)
            || !next.is_char_boundary(end)
        {
            return Err(AgentError::invalid_params(
                "planned edit range does not align with document text",
            ));
        }
        next.replace_range(start..end, replacement);
    }
    Ok(next)
}

// ── App integration ──────────────────────────────────────────────────────────

impl App {
    /// Drains bridge requests forwarded by the host handler.
    pub(super) fn pump_bridge_requests(&mut self) {
        while let Ok(message) = self.agents.bridge_rx.try_recv() {
            match message {
                BridgeUiMessage::ReadFile { request, reply } => {
                    self.bridge_read_file(&request, reply);
                }
                BridgeUiMessage::WriteFile { request, reply } => {
                    if let Err(error) = self.validate_workspace_write_path(&request.path) {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    let thread = self.session_thread(&request.session_id);
                    self.request_bridge_approval(ApprovalPrompt::write(
                        thread,
                        &request.session_id,
                        &request,
                        reply,
                    ));
                }
                BridgeUiMessage::TerminalCreate { request, reply } => {
                    let thread = self.session_thread(&request.session_id);
                    self.request_bridge_approval(ApprovalPrompt::terminal(
                        thread,
                        &request.session_id,
                        &request,
                        reply,
                    ));
                }
                BridgeUiMessage::Elicitation { session_id, request, reply } => {
                    self.present_elicitation(session_id, request, reply);
                }
                BridgeUiMessage::ProxyTool { call, reply } => {
                    self.handle_proxy_tool(call, reply);
                }
            }
        }
    }

    /// Answers one proxy tool call through the same approval/bridge paths as
    /// direct ACP client methods (Phase 6).  `fs/read_text_file` is served
    /// directly; writes and terminal creates queue an approval prompt;
    /// diagnostics return the last stderr text.
    fn handle_proxy_tool(
        &mut self,
        call: super::agents_mcp::ProxyToolCall,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let session_id = SessionId::new("proxy");
        match call {
            super::agents_mcp::ProxyToolCall::WorkspaceRoots => {
                let _ =
                    reply.send(self.proxy_workspace_roots().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::ListDirectory { path } => {
                let _ = reply.send(
                    self.proxy_list_directory(Path::new(&path), false)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ListDirectoryAll { path } => {
                let _ = reply.send(
                    self.proxy_list_directory(Path::new(&path), true)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::SearchFiles { pattern } => {
                let _ = reply.send(
                    self.proxy_search_files(&pattern, false).map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::SearchFilesAll { pattern } => {
                let _ = reply.send(
                    self.proxy_search_files(&pattern, true).map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::SearchText { query } => {
                let _ = reply
                    .send(self.proxy_search_text(&query).map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::SearchTextRegex { pattern } => {
                let _ = reply.send(
                    self.proxy_search_text_regex(&pattern).map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::SearchTextInFiles { query, file_glob } => {
                let _ = reply.send(
                    self.proxy_search_text_in_files(&query, &file_glob)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ReplaceText { path, old_text, new_text } => {
                self.queue_proxy_replace_text(&path, &old_text, &new_text, reply);
            }
            super::agents_mcp::ProxyToolCall::ApplyPatch { path, edits } => {
                self.queue_proxy_apply_patch(&path, &edits, reply);
            }
            super::agents_mcp::ProxyToolCall::CreateTextFile { path, content } => {
                self.queue_proxy_create_text_file(&path, &content, reply);
            }
            super::agents_mcp::ProxyToolCall::OverwriteTextFile { path, content } => {
                self.queue_proxy_overwrite_text_file(&path, &content, reply);
            }
            super::agents_mcp::ProxyToolCall::ReadBuffer { path } => {
                let _ = reply.send(
                    self.proxy_read_buffer(Path::new(&path), None, None)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ReadBufferLines { path, line, limit } => {
                let _ = reply.send(
                    self.proxy_read_buffer(Path::new(&path), Some(line), Some(limit))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::OpenBuffers => {
                let _ =
                    reply.send(self.proxy_open_buffers().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::GetDiagnostics => {
                let _ = reply
                    .send(self.proxy_get_diagnostics(None).map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::GetFileDiagnostics { path } => {
                let _ = reply.send(
                    self.proxy_get_diagnostics(Some(Path::new(&path)))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::DocumentSymbols { path } => {
                let _ = reply.send(
                    self.proxy_document_symbols(Path::new(&path))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::References { path, line, character } => {
                let _ = reply.send(
                    self.proxy_references(Path::new(&path), line, character)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ListCodeActions { path, line, character } => {
                let _ = reply.send(
                    self.proxy_list_code_actions(Path::new(&path), line, character)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ApplyCodeAction { path, action_id } => {
                self.queue_proxy_apply_code_action(&path, &action_id, reply);
            }
            super::agents_mcp::ProxyToolCall::FormatFile { path } => {
                self.queue_proxy_format_file(&path, reply);
            }
            super::agents_mcp::ProxyToolCall::PreviewRenameSymbol {
                path,
                line,
                character,
                new_name,
            } => {
                let _ = reply.send(
                    self.proxy_preview_rename_symbol(Path::new(&path), line, character, &new_name)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::RenameSymbol { path, line, character, new_name } => {
                self.queue_proxy_rename_symbol(&path, line, character, &new_name, reply);
            }
            super::agents_mcp::ProxyToolCall::Read(request) => {
                self.bridge_read_file(&request, reply);
            }
            super::agents_mcp::ProxyToolCall::Write(request) => {
                if let Err(error) = self.validate_workspace_write_path(&request.path) {
                    let _ = reply.send(Err(error));
                    return;
                }
                self.request_bridge_approval(ApprovalPrompt::write(
                    None,
                    &session_id,
                    &request,
                    reply,
                ));
            }
            super::agents_mcp::ProxyToolCall::Terminal(request) => {
                self.request_bridge_approval(ApprovalPrompt::terminal(
                    None,
                    &session_id,
                    &request,
                    reply,
                ));
            }
            super::agents_mcp::ProxyToolCall::Diagnostics => {
                // Transport-only mapping: diagnostics travel as terminal
                // output text internally and are re-mapped by the proxy
                // listener (never crosses the ACP wire).
                let text = self
                    .agents
                    .mcp
                    .servers
                    .values()
                    .filter_map(|server| server.error.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = reply.send(Ok(ClientRequestResponse::TerminalOutput(
                    TerminalOutputResponse::new(text, false),
                )));
            }
        }
    }

    fn session_thread(&self, session_id: &SessionId) -> Option<usize> {
        self.agents.thread_index(session_id.0.as_ref())
    }

    /// Validates and answers an `fs/read_text_file` request.
    ///
    /// Open buffers win over disk (unsaved in-memory text is returned);
    /// unopened files are read from disk only when inside the workspace.
    fn bridge_read_file(
        &mut self,
        request: &ReadTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        if !request.path.is_absolute() {
            let _ = reply.send(Err(AgentError::invalid_params("path must be absolute")));
            return;
        }
        if let Err(error) = validate_read_window(request.line, request.limit, None) {
            let _ = reply.send(Err(error));
            return;
        }
        if !self.path_in_effective_workspace(&request.path) {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                request.path.display()
            ))));
            return;
        }

        // Open buffer first: the in-memory snapshot is authoritative.
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, &request.path)))
        {
            let session_id = request.session_id.0.to_string();
            match self.read_from_buffer(buf, request) {
                Ok((content, bytes)) => {
                    self.agents.action_log.push(ActionLogEntry::Read {
                        path: request.path.clone(),
                        bytes,
                        session_id: session_id.clone(),
                    });
                    if let Some(thread) = self.session_thread(&request.session_id) {
                        self.agents.threads[thread]
                            .push_system(format!("agent read: {}", request.path.display()));
                    }
                    let _ = reply.send(Ok(ClientRequestResponse::ReadTextFile(
                        ReadTextFileResponse::new(content),
                    )));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
            return;
        }

        // Disk fallback after containment check above.
        match std::fs::read_to_string(&request.path) {
            Ok(content) => match read_text_window(&content, request.line, request.limit) {
                Ok(content) => {
                    let bytes = content.len();
                    self.agents.action_log.push(ActionLogEntry::Read {
                        path: request.path.clone(),
                        bytes,
                        session_id: request.session_id.0.to_string(),
                    });
                    let _ = reply.send(Ok(ClientRequestResponse::ReadTextFile(
                        ReadTextFileResponse::new(content),
                    )));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(AgentError::Io(format!(
                    "cannot read {}: {error}",
                    request.path.display()
                ))));
            }
        }
    }

    /// Serves a read from an open buffer, applying ACP line/limit semantics
    /// (1-based `line`, optional `limit`, both enforced against caps).
    fn read_from_buffer(
        &self,
        buf: &crate::buffer::BufState,
        request: &ReadTextFileRequest,
    ) -> Result<(String, usize), AgentError> {
        let line_count = buf.line_count();
        let start = validate_read_window(request.line, request.limit, Some(line_count))?;
        if buf.is_vlf {
            if request.line.is_none() && request.limit.is_none() {
                return Err(AgentError::invalid_params(
                    "unbounded reads are not supported for very large files",
                ));
            }
            let count = request.limit.map(|limit| limit as usize).unwrap_or(BRIDGE_READ_MAX_LINES);
            let end = start.saturating_add(count);
            let cache_start = buf.vlf_cache_start_line;
            let cache_end = cache_start.saturating_add(buf.line_cache.len());
            if start < cache_start || end > cache_end {
                return Err(AgentError::invalid_params(
                    "requested range is not loaded in the very-large-file viewport",
                ));
            }
            let lines: Vec<String> = buf
                .line_cache
                .iter()
                .skip(start - cache_start)
                .take(count)
                .map(|slot| match slot {
                    crate::backend::LineSlot::Known(cached) => cached.text.clone(),
                    crate::backend::LineSlot::Invalid => String::new(),
                })
                .collect();
            let content = lines.join("\n");
            let bytes = content.len();
            return Ok((content, bytes));
        }
        let content =
            read_text_window(&buf.whole_text().unwrap_or_default(), request.line, request.limit)?;
        let bytes = content.len();
        Ok((content, bytes))
    }

    /// Queues an approval prompt (front of the queue wins) and notifies,
    /// unless a recorded session policy already auto-resolves it.
    fn request_bridge_approval(&mut self, prompt: ApprovalPrompt) {
        let thread_index = prompt.thread_index;
        let session_id = prompt.session_id.clone();
        let fingerprint = approval_fingerprint(&prompt.kind);
        if let Some(choice) = self.agents.approval_policy.lookup(&session_id, &fingerprint) {
            // Session-scoped decision: resolve silently, no UI.
            self.resolve_approval(prompt, choice);
            return;
        }
        if let Some(thread) = thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread)
        {
            thread.transcript.push(TranscriptItem::Approval {
                title: prompt.title.clone(),
                detail: prompt.detail.clone(),
                options: prompt.options.iter().map(|(label, _)| label.clone()).collect(),
                at: SystemTime::now(),
            });
        }
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(if self.agents.layout == AgentPaneLayout::Closed {
            String::from("agent approval required (open :agents)")
        } else {
            String::from("agent approval required")
        });
    }

    /// Resolves one approval with the chosen policy decision.
    fn resolve_approval(&mut self, prompt: ApprovalPrompt, choice: ApprovalChoice) {
        let fingerprint = approval_fingerprint(&prompt.kind);
        self.agents.approval_policy.record(&prompt.session_id, &fingerprint, choice);
        let allow = choice.allows();
        if !allow {
            let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                reason: String::from("user denied the operation"),
            }));
            if let Some(thread) = prompt.thread_index
                && let Some(thread) = self.agents.threads.get_mut(thread)
            {
                thread.push_system("approval denied");
            }
            return;
        }
        match prompt.kind {
            ApprovalKind::Write {
                path,
                content,
                tool_call_id,
                expectation,
                reply_kind,
                proxy_edit_count,
            } => {
                self.apply_bridge_write(
                    PreparedWrite {
                        path,
                        content,
                        tool_call_id,
                        expectation,
                        reply_kind,
                        proxy_edit_count,
                    },
                    &prompt.session_id,
                    prompt.reply,
                );
            }
            ApprovalKind::WriteBatch { writes, total_edit_count } => {
                self.apply_bridge_write_batch(
                    writes,
                    total_edit_count,
                    &prompt.session_id,
                    prompt.reply,
                );
            }
            ApprovalKind::TerminalCreate { request } => {
                self.spawn_bridge_terminal(&request, prompt.reply);
            }
        }
    }

    /// Confirms the front approval with the selected option.
    pub(super) fn confirm_bridge_approval(&mut self, choice: ApprovalChoice) {
        let Some(prompt) = self.agents.approvals.pop_front() else {
            return;
        };
        self.resolve_approval(prompt, choice);
    }

    /// Performs an approved buffer write: open/reuse buffer, diff, edit,
    /// verify, save — all through existing buffer/save semantics.
    fn apply_bridge_write(
        &mut self,
        prepared: PreparedWrite,
        session_id: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = prepared.path.as_path();
        let content = prepared.content.as_str();
        if let Err(error) = self.validate_workspace_write_path(path) {
            let _ = reply.send(Err(error));
            return;
        }
        if let Err(error) = self.validate_write_expectation(path, &prepared.expectation) {
            let _ = reply.send(Err(error));
            return;
        }
        match self.write_through_buffer(path, content) {
            Ok(outcome) => {
                self.agents.action_log.push(ActionLogEntry::Write {
                    path: path.to_path_buf(),
                    old_fingerprint: fingerprint(&outcome.old_content),
                    new_fingerprint: fingerprint(content),
                    tool_call_id: prepared.tool_call_id,
                    session_id: session_id.to_string(),
                });
                if let Some(thread) = self.session_thread_by_id(session_id) {
                    self.agents.threads[thread]
                        .push_system(format!("agent wrote: {}", path.display()));
                }
                let response = match prepared.reply_kind {
                    WriteReplyKind::FsWrite => {
                        ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new())
                    }
                    WriteReplyKind::ProxyStructured => {
                        ClientRequestResponse::ProxyValue(json!(ee_mcp::EditTextResult {
                            changed_file: path.display().to_string(),
                            byte_count: outcome.byte_count,
                            edit_count: prepared.proxy_edit_count,
                            new_revision: outcome.new_revision,
                            saved: outcome.saved,
                            dirty: outcome.dirty,
                        }))
                    }
                };
                let _ = reply.send(Ok(response));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn apply_bridge_write_batch(
        &mut self,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        session_id: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        for prepared in &writes {
            if let Err(error) = self.validate_workspace_write_path(prepared.path.as_path()) {
                let _ = reply.send(Err(error));
                return;
            }
            if let Err(error) =
                self.validate_write_expectation(prepared.path.as_path(), &prepared.expectation)
            {
                let _ = reply.send(Err(error));
                return;
            }
        }
        let mut files = Vec::new();
        for prepared in writes {
            let path = prepared.path.clone();
            match self.write_through_buffer(path.as_path(), prepared.content.as_str()) {
                Ok(outcome) => {
                    self.agents.action_log.push(ActionLogEntry::Write {
                        path: path.clone(),
                        old_fingerprint: fingerprint(&outcome.old_content),
                        new_fingerprint: fingerprint(prepared.content.as_str()),
                        tool_call_id: prepared.tool_call_id.clone(),
                        session_id: session_id.to_string(),
                    });
                    files.push(ee_mcp::EditTextResult {
                        changed_file: path.display().to_string(),
                        byte_count: outcome.byte_count,
                        edit_count: prepared.proxy_edit_count,
                        new_revision: outcome.new_revision,
                        saved: outcome.saved,
                        dirty: outcome.dirty,
                    });
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
        }
        let _ =
            reply.send(Ok(ClientRequestResponse::ProxyValue(json!(ee_mcp::WorkspaceEditResult {
                file_count: u32::try_from(files.len()).unwrap_or(u32::MAX),
                edit_count: total_edit_count,
                files,
            }))));
    }

    /// Opens/reuses the buffer, applies the minimal diff, verifies, saves.
    fn write_through_buffer(
        &mut self,
        path: &Path,
        content: &str,
    ) -> Result<BridgeWriteOutcome, AgentError> {
        let target_lines = split_lines(content);
        let buf_id = match self.buffer_id_for_path(path) {
            Some(id) => id,
            None => self.backend.open_buffer(Some(path.to_path_buf())).map_err(|error| {
                AgentError::Io(format!("cannot open {}: {error}", path.display()))
            })?,
        };
        let snapshot = |backend: &crate::buffer::BufferManager| -> Option<String> {
            backend.all_bufs().iter().find(|buf| buf.id == buf_id).and_then(|buf| buf.whole_text())
        };
        if let Some(buf) = self.backend.all_bufs().iter().find(|buf| buf.id == buf_id)
            && buf.is_vlf
        {
            return Err(AgentError::invalid_params(
                "writes are not supported for very large file buffers",
            ));
        }
        self.backend
            .flush_all_pending_edits()
            .map_err(|error| AgentError::Io(format!("flush failed: {error}")))?;
        let old_content = snapshot(&self.backend).unwrap_or_default();

        // Line edits target the active view only, so transiently switch to
        // the target buffer and restore the previous one afterwards.  The
        // switch is invisible to the renderer (no pump happens in between).
        let previous_idx = self.backend.current_idx();
        if self.backend.active().id != buf_id {
            self.backend.switch_to_id(buf_id).map_err(|error| {
                AgentError::Io(format!("cannot switch to {}: {error}", path.display()))
            })?;
        }
        let result =
            self.apply_bridge_write_edits(&target_lines, buf_id, path, &snapshot, &old_content);
        if self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        result?;
        let Some(buf) = self.backend.all_bufs().iter().find(|buf| buf.id == buf_id) else {
            return Err(AgentError::HandlerError(String::from("buffer disappeared after save")));
        };
        Ok(BridgeWriteOutcome {
            old_content,
            byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
            new_revision: buffer_revision_id(buf),
            saved: buffer_saved_state(buf),
            dirty: !buf.pristine,
        })
    }

    /// Diff-applies `target_lines` to the active buffer, polls for the edit
    /// updates, verifies convergence, and saves.
    fn apply_bridge_write_edits(
        &mut self,
        target_lines: &[String],
        buf_id: crate::buffer::BufferId,
        path: &Path,
        snapshot: &impl Fn(&crate::buffer::BufferManager) -> Option<String>,
        old_content: &str,
    ) -> Result<(), AgentError> {
        let mut current_content = old_content.to_string();
        for _ in 0..2 {
            let current_lines = split_lines(&current_content);
            if current_lines == target_lines {
                break;
            }
            let hunks = diff_hunks(&current_lines, target_lines);
            if hunks.is_empty() {
                break;
            }
            for (start, end, new_lines) in hunks.into_iter().rev() {
                self.apply_diff_hunk(&current_lines, start, end, &new_lines)?;
            }
            self.backend
                .flush_all_pending_edits()
                .map_err(|error| AgentError::Io(format!("flush failed: {error}")))?;
            // xi-core applies edits asynchronously; poll until the update
            // lands (bounded, so a hung backend fails closed).
            let deadline = Instant::now() + Duration::from_millis(2000);
            current_content = loop {
                self.backend
                    .drain_events()
                    .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
                let next = snapshot(&self.backend).unwrap_or_default();
                if split_lines(&next) == target_lines || Instant::now() >= deadline {
                    break next;
                }
                thread::sleep(Duration::from_millis(10));
            };
        }
        if split_lines(&current_content) != target_lines {
            return Err(AgentError::invalid_params(
                "buffer changed concurrently; agent write conflicts with user edits",
            ));
        }

        self.backend.save_buffer(buf_id).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                AgentError::PermissionDenied {
                    reason: format!("save permission denied for {}", path.display()),
                }
            } else {
                AgentError::Io(format!("save failed: {error}"))
            }
        })?;
        Ok(())
    }

    /// Applies one diff hunk against the backend's inclusive line-range edit.
    ///
    /// `end` is exclusive; a pure insertion (`start == end`) anchors on the
    /// current line so nothing is overwritten.
    fn apply_diff_hunk(
        &mut self,
        current_lines: &[String],
        start: usize,
        end: usize,
        new_lines: &[String],
    ) -> Result<(), AgentError> {
        if start == end {
            let anchor = current_lines
                .get(start)
                .or_else(|| current_lines.last())
                .cloned()
                .unwrap_or_default();
            let mut replacement = new_lines.to_vec();
            replacement.push(anchor);
            let last = current_lines.len().saturating_sub(1);
            self.backend
                .replace_line_range(start.min(last), start.min(last), &replacement)
                .map_err(|error| AgentError::Io(format!("edit failed: {error}")))?;
            return Ok(());
        }
        self.backend
            .replace_line_range(start, end.saturating_sub(1), new_lines)
            .map_err(|error| AgentError::Io(format!("edit failed: {error}")))
    }

    /// Spawns an approved terminal and replies with its id.
    fn spawn_bridge_terminal(
        &mut self,
        request: &CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let result = self.agents.terminals.spawn(request);
        let _ = reply.send(result.map(ClientRequestResponse::CreateTerminal));
    }

    fn buffer_id_for_path(&self, path: &Path) -> Option<crate::buffer::BufferId> {
        self.backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
            .map(|buf| buf.id)
    }

    fn session_thread_by_id(&self, session_id: &str) -> Option<usize> {
        self.agents.thread_index(session_id)
    }

    fn path_in_workspace(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.canonical_workspace_roots().iter().any(|root| canonical.starts_with(root))
    }

    fn path_in_effective_workspace(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.allowed_fs_roots().iter().any(|root| canonical.starts_with(root))
    }

    fn canonical_workspace_roots(&self) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for root in self.agents_workspace_roots() {
            if !root.is_absolute() {
                continue;
            }
            if let Ok(canonical) = std::fs::canonicalize(&root) {
                roots.insert(canonical);
            }
        }
        roots.into_iter().collect()
    }

    fn active_file_path(&self) -> Option<PathBuf> {
        self.backend.active().path.clone()
    }

    fn active_root_path(&self) -> Option<PathBuf> {
        let roots = self.canonical_workspace_roots();
        let active_file = self
            .active_file_path()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .or_else(|| std::fs::canonicalize(&self.working_dir).ok());
        active_file.and_then(|path| roots.into_iter().find(|root| path.starts_with(root)))
    }

    fn allowed_fs_roots(&self) -> Vec<PathBuf> {
        self.active_root_path().map_or_else(|| self.canonical_workspace_roots(), |root| vec![root])
    }

    fn validate_workspace_write_path(&self, path: &Path) -> Result<(), AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let candidate = if path.exists() {
            std::fs::canonicalize(path).map_err(|error| {
                AgentError::Io(format!("cannot access {}: {error}", path.display()))
            })?
        } else {
            let Some(parent) = path.parent() else {
                return Err(AgentError::invalid_params(format!(
                    "path has no parent directory: {}",
                    path.display()
                )));
            };
            let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
                AgentError::Io(format!("cannot access parent {}: {error}", parent.display()))
            })?;
            let Some(name) = path.file_name() else {
                return Err(AgentError::invalid_params(format!(
                    "path has no file name: {}",
                    path.display()
                )));
            };
            canonical_parent.join(name)
        };
        if self.allowed_fs_roots().iter().any(|root| candidate.starts_with(root)) {
            Ok(())
        } else {
            Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )))
        }
    }

    fn current_text_revision(&self, path: &Path) -> Result<Option<String>, AgentError> {
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            return Ok(Some(buffer_revision_id(buf)));
        }
        if !path.exists() {
            return Ok(None);
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        Ok(Some(text_revision_id(&content)))
    }

    fn validate_write_expectation(
        &self,
        path: &Path,
        expectation: &WriteExpectation,
    ) -> Result<(), AgentError> {
        match expectation {
            WriteExpectation::Blind => Ok(()),
            WriteExpectation::MustNotExist => {
                if self.current_text_revision(path)?.is_some() {
                    Err(AgentError::invalid_params(format!(
                        "target already exists or was created before approval: {}",
                        path.display()
                    )))
                } else {
                    Ok(())
                }
            }
            WriteExpectation::ExpectRevision(expected) => {
                let actual = self.current_text_revision(path)?;
                if actual.as_deref() == Some(expected.as_str()) {
                    Ok(())
                } else {
                    Err(AgentError::invalid_params(format!(
                        "buffer changed after tool prepared edit for {}; re-read and retry",
                        path.display()
                    )))
                }
            }
        }
    }

    fn read_current_text(&mut self, path: &Path) -> Result<(String, String), AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            if buf.is_vlf {
                return Err(AgentError::invalid_params(
                    "full-buffer edits are not supported for very large file buffers",
                ));
            }
            let content = buf.whole_text().unwrap_or_default();
            let revision = text_revision_id(&content);
            return Ok((content, revision));
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        let revision = text_revision_id(&content);
        Ok((content, revision))
    }

    fn prepare_replace_text(
        &mut self,
        path: &Path,
        old_text: &str,
        new_text: &str,
    ) -> Result<(String, WriteExpectation), AgentError> {
        if old_text.is_empty() {
            return Err(AgentError::invalid_params("old_text must not be empty"));
        }
        let (content, revision) = self.read_current_text(path)?;
        let matches = content.match_indices(old_text).count();
        match matches {
            1 => Ok((
                content.replacen(old_text, new_text, 1),
                WriteExpectation::ExpectRevision(revision),
            )),
            0 => Err(AgentError::invalid_params(format!(
                "old_text was not found in {}",
                path.display()
            ))),
            count => Err(AgentError::invalid_params(format!(
                "old_text matched {count} times in {}; expected exactly one match",
                path.display()
            ))),
        }
    }

    fn prepare_apply_patch(
        &mut self,
        path: &Path,
        edits: &[ee_agent_host::ProxyTextEdit],
    ) -> Result<(String, WriteExpectation), AgentError> {
        if edits.is_empty() {
            return Err(AgentError::invalid_params("edits must not be empty"));
        }
        let (mut content, revision) = self.read_current_text(path)?;
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_text.is_empty() {
                return Err(AgentError::invalid_params(format!(
                    "edit {} old_text must not be empty",
                    index + 1
                )));
            }
            let matches = content.match_indices(edit.old_text.as_str()).count();
            match matches {
                1 => {
                    content = content.replacen(edit.old_text.as_str(), edit.new_text.as_str(), 1);
                }
                0 => {
                    return Err(AgentError::invalid_params(format!(
                        "edit {} old_text was not found in {}",
                        index + 1,
                        path.display()
                    )));
                }
                count => {
                    return Err(AgentError::invalid_params(format!(
                        "edit {} old_text matched {count} times in {}; expected exactly one match",
                        index + 1,
                        path.display()
                    )));
                }
            }
        }
        Ok((content, WriteExpectation::ExpectRevision(revision)))
    }

    fn queue_proxy_replace_text(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        match self.prepare_replace_text(&path, old_text, new_text) {
            Ok((content, expectation)) => {
                let spec = ProxyWriteSpec {
                    title: String::from("ee.replace_text"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), content.len()),
                    prepared: PreparedWrite {
                        path,
                        content,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_apply_patch(
        &mut self,
        path: &str,
        edits: &[ee_agent_host::ProxyTextEdit],
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        match self.prepare_apply_patch(&path, edits) {
            Ok((content, expectation)) => {
                let edit_count = u32::try_from(edits.len()).unwrap_or(u32::MAX);
                let detail = format!(
                    "{} ({} bytes, {} edit{})",
                    path.display(),
                    content.len(),
                    edit_count,
                    if edit_count == 1 { "" } else { "s" }
                );
                let spec = ProxyWriteSpec {
                    title: String::from("ee.apply_patch"),
                    detail,
                    prepared: PreparedWrite {
                        path,
                        content,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: edit_count,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_create_text_file(
        &mut self,
        path: &str,
        content: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        if let Err(error) = self.validate_workspace_write_path(&path) {
            let _ = reply.send(Err(error));
            return;
        }
        match self.current_text_revision(&path) {
            Ok(Some(_)) => {
                let _ = reply.send(Err(AgentError::invalid_params(format!(
                    "target already exists: {}",
                    path.display()
                ))));
            }
            Ok(None) => {
                let created = content.to_string();
                let spec = ProxyWriteSpec {
                    title: String::from("ee.create_text_file"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), created.len()),
                    prepared: PreparedWrite {
                        path,
                        content: created,
                        tool_call_id: None,
                        expectation: WriteExpectation::MustNotExist,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_overwrite_text_file(
        &mut self,
        path: &str,
        content: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        if let Err(error) = self.validate_workspace_write_path(&path) {
            let _ = reply.send(Err(error));
            return;
        }
        match self.current_text_revision(&path) {
            Ok(Some(revision)) => {
                let updated = content.to_string();
                let spec = ProxyWriteSpec {
                    title: String::from("ee.overwrite_text_file"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), updated.len()),
                    prepared: PreparedWrite {
                        path,
                        content: updated,
                        tool_call_id: None,
                        expectation: WriteExpectation::ExpectRevision(revision),
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply))
            }
            Ok(None) => {
                let _ = reply.send(Err(AgentError::invalid_params(format!(
                    "target does not exist: {}",
                    path.display()
                ))));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn proxy_read_buffer(
        &mut self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<serde_json::Value, AgentError> {
        let mut request = ReadTextFileRequest::new(SessionId::new("proxy"), path.to_path_buf());
        request.line = line;
        request.limit = limit;
        let text = if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            self.read_from_buffer(buf, &request)?.0
        } else {
            if !path.is_absolute() {
                return Err(AgentError::invalid_params("path must be absolute"));
            }
            if !self.path_in_workspace(path) {
                return Err(AgentError::invalid_params(format!(
                    "path outside allowed workspace: {}",
                    path.display()
                )));
            }
            let content = std::fs::read_to_string(path).map_err(|error| {
                AgentError::Io(format!("cannot read {}: {error}", path.display()))
            })?;
            if let Some(line) = line {
                read_text_window(&content, Some(line), limit)?
            } else {
                read_text_window(&content, None, limit)?
            }
        };
        Ok(serde_json::Value::String(text))
    }

    fn buffer_language_id(&self, buf: &crate::buffer::BufState) -> Option<String> {
        self.syntax_overrides
            .get(&buf.id)
            .cloned()
            .or_else(|| {
                buf.path
                    .as_deref()
                    .and_then(xi_core_lib::tree_sitter_support::language_name_for_path)
            })
            .or_else(|| self.highlighter.syntax_name_for_path(buf.path.as_deref()))
    }

    fn buffer_selection_summary(&mut self, buf_id: crate::buffer::BufferId) -> String {
        let previous_idx = self.backend.current_idx();
        let switched = self.backend.active().id != buf_id;
        if switched && self.backend.switch_to_id(buf_id).is_err() {
            return String::from("selection unavailable");
        }
        let summary = match self.primary_selection_preview() {
            Ok(Some(selection)) => {
                let start = selection.start.min(selection.end);
                let end = selection.start.max(selection.end);
                if start == end {
                    format!("cursor at offset {start}")
                } else {
                    format!("offsets {start}..{end}")
                }
            }
            Ok(None) => String::from("cursor only"),
            Err(_) => String::from("selection unavailable"),
        };
        if switched && self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        summary
    }

    fn proxy_open_buffers(&mut self) -> Result<serde_json::Value, AgentError> {
        let active_id = self.backend.active().id;
        let snapshot = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buf| {
                let path = buf.path.as_ref()?;
                Some((
                    buf.id,
                    path.display().to_string(),
                    !buf.pristine,
                    buffer_revision_id(buf),
                    format!("line {}, column {}", buf.cursor_line + 1, buf.cursor_col + 1),
                    self.buffer_language_id(buf),
                    buf.id == active_id,
                ))
            })
            .collect::<Vec<_>>();
        let buffers = snapshot
            .into_iter()
            .map(|(id, path, dirty, revision_id, cursor_summary, language_id, active)| {
                ee_mcp::OpenBufferEntry {
                    path,
                    dirty,
                    revision_id,
                    cursor_summary,
                    selection_summary: self.buffer_selection_summary(id),
                    language_id,
                    active,
                }
            })
            .collect();
        serde_json::to_value(ee_mcp::OpenBuffersResult { buffers })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn ensure_proxy_buffer(&mut self, path: &Path) -> Result<crate::buffer::BufferId, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if let Some(id) = self.buffer_id_for_path(path) {
            return Ok(id);
        }
        if !self.path_in_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        self.backend
            .open_buffer(Some(path.to_path_buf()))
            .map_err(|error| AgentError::Io(format!("cannot open {}: {error}", path.display())))
    }

    fn proxy_agent_tool_payload(
        &mut self,
        path: &Path,
        line: Option<u32>,
        character: Option<u32>,
        method: &str,
        kind: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        let buf_id = self.ensure_proxy_buffer(path)?;
        let previous_idx = self.backend.current_idx();
        let switched = self.backend.active().id != buf_id;
        if switched {
            self.backend.switch_to_id(buf_id).map_err(|error| {
                AgentError::Io(format!("cannot switch to {}: {error}", path.display()))
            })?;
        }
        let saved_selections =
            if !switched { self.backend.selections_preview().ok() } else { None };
        if let Some(line) = line {
            let character = character.unwrap_or(1);
            self.move_cursor_to(
                (line.saturating_sub(1)) as usize,
                character.saturating_sub(1) as usize,
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                self.backend
                    .drain_events()
                    .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
                if self.backend.cursor_line == (line.saturating_sub(1)) as usize {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.backend
            .send_edit(method, params)
            .map_err(|error| AgentError::Io(format!("{method} failed: {error}")))?;
        let active_view_id = self.backend.active().view_id.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            self.backend
                .drain_events()
                .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
            let pending = self.backend.drain_pending_agent_tool_results();
            let mut remainder = Vec::new();
            let mut matched = None;
            for entry in pending {
                if matched.is_none() && entry.0 == active_view_id && entry.1 == kind {
                    matched = Some(entry.2);
                } else {
                    remainder.push(entry);
                }
            }
            self.backend.pending_agent_tool_results.extend(remainder);
            if let Some(payload) = matched {
                break Ok(payload);
            }
            if Instant::now() >= deadline {
                break Err(AgentError::Io(format!("{method} timed out")));
            }
            thread::sleep(Duration::from_millis(10));
        };
        if !switched && let Some(selections) = saved_selections {
            let _ = self.backend.set_selections(&selections);
        }
        if switched && self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        result
    }

    fn proxy_diagnostic_entries(&self, path: Option<&Path>) -> Vec<ee_mcp::DiagnosticEntry> {
        self.backend
            .all_bufs()
            .iter()
            .filter_map(|buf| {
                let buf_path = buf.path.as_ref()?;
                if let Some(target) = path
                    && !paths_equivalent(buf_path, target)
                {
                    return None;
                }
                Some((buf_path, &buf.lines, &buf.diagnostics))
            })
            .flat_map(|(buf_path, lines, diagnostics)| {
                diagnostics.iter().map(move |diagnostic| {
                    let (start_line, start_col) =
                        line_col_for_offset(lines, diagnostic.range.start);
                    let (end_line, end_col) = line_col_for_offset(lines, diagnostic.range.end);
                    ee_mcp::DiagnosticEntry {
                        path: buf_path.display().to_string(),
                        range: ee_mcp::TextRange {
                            start_line: u32::try_from(start_line + 1).unwrap_or(u32::MAX),
                            start_character: u32::try_from(start_col + 1).unwrap_or(u32::MAX),
                            end_line: u32::try_from(end_line + 1).unwrap_or(u32::MAX),
                            end_character: u32::try_from(end_col + 1).unwrap_or(u32::MAX),
                        },
                        severity: match diagnostic.severity {
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Error => {
                                String::from("error")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Warning => {
                                String::from("warning")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Information => {
                                String::from("information")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Hint => {
                                String::from("hint")
                            }
                        },
                        source: diagnostic.source.clone(),
                        code: diagnostic.code.clone(),
                        message: diagnostic.message.clone(),
                    }
                })
            })
            .collect()
    }

    fn proxy_get_diagnostics(
        &mut self,
        path: Option<&Path>,
    ) -> Result<serde_json::Value, AgentError> {
        if let Some(path) = path
            && self.buffer_id_for_path(path).is_none()
            && path.exists()
        {
            let _ = self.ensure_proxy_buffer(path)?;
            let _ = self.backend.drain_events();
        }
        let mut diagnostics = self.proxy_diagnostic_entries(path);
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.range.start_line.cmp(&right.range.start_line))
                .then(left.range.start_character.cmp(&right.range.start_character))
        });
        let total = u32::try_from(diagnostics.len()).unwrap_or(u32::MAX);
        let truncated = diagnostics.len() > PROXY_DIAGNOSTICS_LIMIT;
        diagnostics.truncate(PROXY_DIAGNOSTICS_LIMIT);
        serde_json::to_value(ee_mcp::DiagnosticsResult { diagnostics, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_document_symbols(&mut self, path: &Path) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            None,
            None,
            "ee.agent.document_symbols",
            "document_symbols",
            json!({}),
        )?;
        let mut symbols = serde_json::from_value::<AgentDocumentSymbolsPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .symbols;
        let total = u32::try_from(symbols.len()).unwrap_or(u32::MAX);
        let truncated = symbols.len() > PROXY_DOCUMENT_SYMBOLS_LIMIT;
        symbols.truncate(PROXY_DOCUMENT_SYMBOLS_LIMIT);
        serde_json::to_value(ee_mcp::DocumentSymbolsResult { symbols, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_references(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.references",
            "references",
            json!({}),
        )?;
        let mut references = serde_json::from_value::<AgentReferencesPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .references;
        let total = u32::try_from(references.len()).unwrap_or(u32::MAX);
        let truncated = references.len() > PROXY_REFERENCES_LIMIT;
        references.truncate(PROXY_REFERENCES_LIMIT);
        serde_json::to_value(ee_mcp::ReferencesResult { references, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_list_code_actions(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.list_code_actions",
            "list_code_actions",
            json!({}),
        )?;
        let actions = serde_json::from_value::<AgentCodeActionPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .actions;
        let total = u32::try_from(actions.len()).unwrap_or(u32::MAX);
        let truncated = actions.len() > PROXY_CODE_ACTIONS_LIMIT;
        let path_text = path.display().to_string();
        let mut listed = Vec::new();
        for action in actions.into_iter().take(PROXY_CODE_ACTIONS_LIMIT) {
            let action_id = format!("proxy-action-{}", self.agents.mcp.next_proxy_action_id);
            self.agents.mcp.next_proxy_action_id =
                self.agents.mcp.next_proxy_action_id.saturating_add(1);
            self.agents.mcp.proxy_code_actions.insert(
                action_id.clone(),
                super::agents_mcp::CachedProxyCodeAction {
                    path: path_text.clone(),
                    has_command: action.has_command,
                    edits: action.edits.clone(),
                },
            );
            listed.push(ee_mcp::CodeActionEntry {
                action_id,
                title: action.title,
                kind: action.kind,
            });
        }
        serde_json::to_value(ee_mcp::CodeActionsResult { actions: listed, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn current_proxy_edit_result(
        &self,
        path: &Path,
        edit_count: u32,
    ) -> Result<ee_mcp::EditTextResult, AgentError> {
        if let Some(buf) = self.backend.all_bufs().iter().find(|buf| {
            buf.path.as_deref().is_some_and(|candidate| paths_equivalent(candidate, path))
        }) {
            let content = buf.whole_text().unwrap_or_default();
            return Ok(ee_mcp::EditTextResult {
                changed_file: path.display().to_string(),
                byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
                edit_count,
                new_revision: buffer_revision_id(buf),
                saved: buffer_saved_state(buf),
                dirty: !buf.pristine,
            });
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        Ok(ee_mcp::EditTextResult {
            changed_file: path.display().to_string(),
            byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
            edit_count,
            new_revision: text_revision_id(&content),
            saved: true,
            dirty: false,
        })
    }

    fn prepare_planned_file_write(
        &mut self,
        path: &Path,
        edits: &[ee_mcp::PlannedTextEdit],
    ) -> Result<PreparedWrite, AgentError> {
        let (content, revision) = self.read_current_text(path)?;
        let next = apply_planned_text_edits_to_content(&content, edits)?;
        Ok(PreparedWrite {
            path: path.to_path_buf(),
            content: next,
            tool_call_id: None,
            expectation: WriteExpectation::ExpectRevision(revision),
            reply_kind: WriteReplyKind::ProxyStructured,
            proxy_edit_count: u32::try_from(edits.len()).unwrap_or(u32::MAX),
        })
    }

    fn proxy_preview_rename_symbol(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.preview_rename",
            "preview_rename",
            json!({ "new_name": new_name }),
        )?;
        let mut files = serde_json::from_value::<AgentRenamePayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .files;
        for file in &files {
            self.validate_workspace_write_path(Path::new(&file.path))?;
        }
        let total_files = u32::try_from(files.len()).unwrap_or(u32::MAX);
        let total_edits = u32::try_from(files.iter().map(|file| file.edits.len()).sum::<usize>())
            .unwrap_or(u32::MAX);
        let mut seen_edits = 0usize;
        let mut truncated = files.len() > PROXY_RENAME_FILES_LIMIT;
        files.truncate(PROXY_RENAME_FILES_LIMIT);
        for file in &mut files {
            if seen_edits >= PROXY_RENAME_EDITS_LIMIT {
                file.edits.clear();
                truncated = true;
                continue;
            }
            let remaining = PROXY_RENAME_EDITS_LIMIT.saturating_sub(seen_edits);
            if file.edits.len() > remaining {
                file.edits.truncate(remaining);
                truncated = true;
            }
            seen_edits = seen_edits.saturating_add(file.edits.len());
        }
        serde_json::to_value(ee_mcp::RenamePreviewResult {
            files,
            truncated,
            total_files,
            total_edits,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn queue_proxy_apply_code_action(
        &mut self,
        path: &str,
        action_id: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let Some(cached) = self.agents.mcp.proxy_code_actions.get(action_id).cloned() else {
            let _ = reply
                .send(Err(AgentError::invalid_params(format!("unknown action_id: {action_id}"))));
            return;
        };
        if cached.path != path {
            let _ = reply
                .send(Err(AgentError::invalid_params("action_id was listed for a different path")));
            return;
        }
        if cached.has_command {
            let _ = reply.send(Err(AgentError::invalid_params(
                "code actions that require executeCommand are not supported yet",
            )));
            return;
        }
        let path = PathBuf::from(path);
        match self.prepare_planned_file_write(&path, &cached.edits) {
            Ok(prepared) => {
                if prepared.content
                    == self.read_current_text(&path).map(|(text, _)| text).unwrap_or_default()
                {
                    let result = self.current_proxy_edit_result(&path, prepared.proxy_edit_count);
                    let _ = reply
                        .send(result.map(|value| ClientRequestResponse::ProxyValue(json!(value))));
                    return;
                }
                let detail = format!(
                    "{} ({} bytes, {} edit{})",
                    path.display(),
                    prepared.content.len(),
                    prepared.proxy_edit_count,
                    if prepared.proxy_edit_count == 1 { "" } else { "s" }
                );
                let spec = ProxyWriteSpec {
                    title: String::from("ee.apply_code_action"),
                    detail,
                    prepared,
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_format_file(&mut self, path: &str, reply: oneshot::Sender<ClientRequestResult>) {
        let path_buf = PathBuf::from(path);
        match self.proxy_agent_tool_payload(
            &path_buf,
            None,
            None,
            "ee.agent.format_preview",
            "format_preview",
            json!({}),
        ) {
            Ok(payload) => match serde_json::from_value::<AgentTextEditsPayload>(payload) {
                Ok(payload) => match self.prepare_planned_file_write(&path_buf, &payload.edits) {
                    Ok(prepared) => {
                        if payload.edits.is_empty() {
                            let result = self.current_proxy_edit_result(&path_buf, 0);
                            let _ = reply.send(
                                result.map(|value| ClientRequestResponse::ProxyValue(json!(value))),
                            );
                            return;
                        }
                        let detail = format!(
                            "{} ({} bytes, {} edit{})",
                            path_buf.display(),
                            prepared.content.len(),
                            prepared.proxy_edit_count,
                            if prepared.proxy_edit_count == 1 { "" } else { "s" }
                        );
                        let spec = ProxyWriteSpec {
                            title: String::from("ee.format_file"),
                            detail,
                            prepared,
                        };
                        self.request_bridge_approval(ApprovalPrompt::proxy_write(spec, reply));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                },
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(error.to_string())));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_rename_symbol(
        &mut self,
        path: &str,
        line: u32,
        character: u32,
        new_name: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path_buf = PathBuf::from(path);
        match self.proxy_agent_tool_payload(
            &path_buf,
            Some(line),
            Some(character),
            "ee.agent.preview_rename",
            "preview_rename",
            json!({ "new_name": new_name }),
        ) {
            Ok(payload) => match serde_json::from_value::<AgentRenamePayload>(payload) {
                Ok(payload) => {
                    let mut writes = Vec::new();
                    let mut total_edits = 0u32;
                    for file in payload.files {
                        let file_path = PathBuf::from(&file.path);
                        if let Err(error) = self.validate_workspace_write_path(&file_path) {
                            let _ = reply.send(Err(error));
                            return;
                        }
                        match self.prepare_planned_file_write(&file_path, &file.edits) {
                            Ok(prepared) => {
                                total_edits = total_edits.saturating_add(prepared.proxy_edit_count);
                                writes.push(prepared);
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                return;
                            }
                        }
                    }
                    if writes.is_empty() {
                        let _ = reply.send(Ok(ClientRequestResponse::ProxyValue(json!(
                            ee_mcp::WorkspaceEditResult {
                                files: Vec::new(),
                                file_count: 0,
                                edit_count: 0
                            }
                        ))));
                        return;
                    }
                    let detail = format!(
                        "{} file{}, {} edit{}",
                        writes.len(),
                        if writes.len() == 1 { "" } else { "s" },
                        total_edits,
                        if total_edits == 1 { "" } else { "s" }
                    );
                    self.request_bridge_approval(ApprovalPrompt::proxy_write_batch(
                        String::from("ee.rename_symbol"),
                        detail,
                        writes,
                        total_edits,
                        reply,
                    ));
                }
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(error.to_string())));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn proxy_workspace_roots(&self) -> Result<serde_json::Value, AgentError> {
        let roots = self
            .canonical_workspace_roots()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let active_root = self.active_root_path().map(|path| path.display().to_string());
        let active_file = self.active_file_path().and_then(|path| {
            std::fs::canonicalize(&path)
                .ok()
                .or_else(|| path.is_absolute().then_some(path))
                .map(|path| path.display().to_string())
        });
        let additional_directories = roots.iter().skip(1).cloned().collect::<Vec<_>>();
        serde_json::to_value(ee_mcp::WorkspaceRootsResult {
            roots,
            active_root,
            active_file,
            additional_directories,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_list_directory(
        &self,
        path: &Path,
        include_hidden_ignored: bool,
    ) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| AgentError::Io(format!("cannot list {}: {error}", path.display())))?;
        if !canonical.is_dir() {
            return Err(AgentError::invalid_params(format!(
                "path is not a directory: {}",
                canonical.display()
            )));
        }
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                canonical.display()
            )));
        }

        let mut truncated = false;
        let visible = visible_child_paths(&canonical);
        let walker = if include_hidden_ignored {
            WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .parents(false)
                .build()
        } else {
            WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .hidden(true)
                .ignore(true)
                .git_ignore(true)
                .git_exclude(true)
                .parents(true)
                .build()
        };
        let mut entries = BTreeMap::new();
        for entry in walker.flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let entry_path = entry.into_path();
            if entry_path.parent() != Some(canonical.as_path()) {
                continue;
            }
            if entries.len() >= PROXY_LIST_DIRECTORY_LIMIT {
                truncated = true;
                break;
            }
            let hidden = is_hidden_path(&entry_path);
            let ignored = !visible.contains(&entry_path);
            let value = if include_hidden_ignored {
                serde_json::to_value(ee_mcp::DirectoryEntryAll {
                    path: entry_path.display().to_string(),
                    kind: file_kind(&entry_path),
                    size: entry_size(&entry_path),
                    hidden,
                    ignored,
                })
            } else {
                serde_json::to_value(ee_mcp::DirectoryEntry {
                    path: entry_path.display().to_string(),
                    kind: file_kind(&entry_path),
                    size: entry_size(&entry_path),
                })
            }
            .map_err(|error| AgentError::HandlerError(error.to_string()))?;
            entries.insert(entry_path.clone(), value);
        }
        if !truncated {
            for buf in self.backend.all_bufs() {
                let Some(buf_path) = &buf.path else {
                    continue;
                };
                let Some(parent) = buf_path.parent() else {
                    continue;
                };
                if !paths_equivalent(parent, &canonical)
                    || entries.len() >= PROXY_LIST_DIRECTORY_LIMIT
                {
                    if entries.len() >= PROXY_LIST_DIRECTORY_LIMIT {
                        truncated = true;
                    }
                    continue;
                }
                entries.entry(buf_path.clone()).or_insert_with(|| {
                    if include_hidden_ignored {
                        serde_json::to_value(ee_mcp::DirectoryEntryAll {
                            path: buf_path.display().to_string(),
                            kind: String::from("file"),
                            size: buffer_visible_size(buf),
                            hidden: is_hidden_path(buf_path),
                            ignored: !visible.contains(buf_path),
                        })
                        .expect("directory entry serializes")
                    } else {
                        serde_json::to_value(ee_mcp::DirectoryEntry {
                            path: buf_path.display().to_string(),
                            kind: String::from("file"),
                            size: buffer_visible_size(buf),
                        })
                        .expect("directory entry serializes")
                    }
                });
            }
        }
        if include_hidden_ignored {
            serde_json::to_value(ee_mcp::ListDirectoryAllResult {
                entries: entries
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("directory entry parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        } else {
            serde_json::to_value(ee_mcp::ListDirectoryResult {
                entries: entries
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("directory entry parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        }
    }

    fn proxy_search_files(
        &self,
        pattern: &str,
        include_hidden_ignored: bool,
    ) -> Result<serde_json::Value, AgentError> {
        if pattern.is_empty() {
            return Err(AgentError::invalid_params("pattern must not be empty"));
        }
        let matcher = build_path_matcher(pattern)?;
        let roots = self.canonical_workspace_roots();
        let visible_by_root: Vec<(PathBuf, BTreeSet<PathBuf>)> = roots
            .iter()
            .cloned()
            .map(|root| {
                let visible = visible_descendant_paths(&root);
                (root, visible)
            })
            .collect();
        let mut matches = BTreeMap::new();
        let mut truncated = false;
        for (root, visible) in &visible_by_root {
            let walker = if include_hidden_ignored {
                WalkBuilder::new(root)
                    .hidden(false)
                    .ignore(false)
                    .git_ignore(false)
                    .git_exclude(false)
                    .parents(false)
                    .build()
            } else {
                WalkBuilder::new(root)
                    .hidden(true)
                    .ignore(true)
                    .git_ignore(true)
                    .git_exclude(true)
                    .parents(true)
                    .build()
            };
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.into_path();
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if !matcher(rel, &path) {
                    continue;
                }
                let path_text = path.display().to_string();
                if include_hidden_ignored {
                    matches.entry(path_text.clone()).or_insert_with(|| {
                        serde_json::to_value(ee_mcp::FileMatch {
                            path: path_text,
                            hidden: is_hidden_path(&path),
                            ignored: !visible.contains(&path),
                        })
                        .expect("file match serializes")
                    });
                } else {
                    matches.entry(path_text).or_insert(serde_json::Value::Null);
                }
                if matches.len() >= PROXY_SEARCH_FILES_LIMIT {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        if !truncated {
            for buf in self.backend.all_bufs() {
                let Some(path) = &buf.path else {
                    continue;
                };
                if !path.is_absolute() || !self.path_in_workspace(path) {
                    continue;
                }
                let rel = roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .unwrap_or(path.as_path());
                if !matcher(rel, path) {
                    continue;
                }
                let path_text = path.display().to_string();
                if include_hidden_ignored {
                    let ignored = visible_by_root
                        .iter()
                        .find(|(root, _)| path.starts_with(root))
                        .is_some_and(|(_, visible)| !visible.contains(path));
                    matches.entry(path_text.clone()).or_insert_with(|| {
                        serde_json::to_value(ee_mcp::FileMatch {
                            path: path_text,
                            hidden: is_hidden_path(path),
                            ignored,
                        })
                        .expect("file match serializes")
                    });
                } else {
                    matches.entry(path_text).or_insert(serde_json::Value::Null);
                }
                if matches.len() >= PROXY_SEARCH_FILES_LIMIT {
                    truncated = true;
                    break;
                }
            }
        }
        if include_hidden_ignored {
            serde_json::to_value(ee_mcp::SearchFilesAllResult {
                matches: matches
                    .into_values()
                    .map(|value| serde_json::from_value(value).expect("file match parses"))
                    .collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        } else {
            serde_json::to_value(ee_mcp::SearchFilesResult {
                matches: matches.into_keys().collect(),
                truncated,
            })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
        }
    }

    fn proxy_search_text(&self, query: &str) -> Result<serde_json::Value, AgentError> {
        if query.is_empty() {
            return Err(AgentError::invalid_params("query must not be empty"));
        }
        let matches = self.collect_text_matches(|path, line_number, line| {
            Ok(line.contains(query).then(|| ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_search_context(line, query),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_search_text_regex(&self, pattern: &str) -> Result<serde_json::Value, AgentError> {
        let regex = compile_search_regex(pattern)?;
        let deadline = Instant::now() + PROXY_SEARCH_REGEX_TIMEOUT;
        let matches = self.collect_text_matches(|path, line_number, line| {
            if Instant::now() >= deadline {
                return Err(AgentError::Io(format!(
                    "regex search timed out after {:?}",
                    PROXY_SEARCH_REGEX_TIMEOUT
                )));
            }
            Ok(regex.is_match(line).then(|| ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_regex_context(line, &regex),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_search_text_in_files(
        &self,
        query: &str,
        file_glob: &str,
    ) -> Result<serde_json::Value, AgentError> {
        if query.is_empty() {
            return Err(AgentError::invalid_params("query must not be empty"));
        }
        let matcher = build_path_matcher(file_glob)?;
        let roots = self.canonical_workspace_roots();
        let matches = self.collect_text_matches(|path, line_number, line| {
            let rel = roots.iter().find_map(|root| path.strip_prefix(root).ok()).unwrap_or(path);
            if !matcher(rel, path) || !line.contains(query) {
                return Ok(None);
            }
            Ok(Some(ee_mcp::TextMatch {
                path: path.display().to_string(),
                line: line_number,
                context: trim_search_context(line, query),
            }))
        })?;
        serde_json::to_value(ee_mcp::SearchTextResult {
            truncated: matches.len() >= PROXY_SEARCH_TEXT_LIMIT,
            matches,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn collect_text_matches(
        &self,
        mut match_line: impl FnMut(&Path, u32, &str) -> Result<Option<ee_mcp::TextMatch>, AgentError>,
    ) -> Result<Vec<ee_mcp::TextMatch>, AgentError> {
        let mut matches = Vec::new();
        let mut seen_open_paths = BTreeSet::new();
        for buf in self.backend.all_bufs() {
            let Some(path) = &buf.path else {
                continue;
            };
            if buf.is_vlf || !self.path_in_workspace(path) {
                continue;
            }
            seen_open_paths.insert(path.clone());
            for (index, line) in buf.lines.iter().enumerate() {
                if let Some(text_match) =
                    match_line(path, u32::try_from(index + 1).unwrap_or(u32::MAX), line)?
                {
                    matches.push(text_match);
                    if matches.len() >= PROXY_SEARCH_TEXT_LIMIT {
                        return Ok(matches);
                    }
                }
            }
        }
        'roots: for root in self.canonical_workspace_roots() {
            let walker = WalkBuilder::new(&root)
                .hidden(true)
                .ignore(true)
                .git_ignore(true)
                .git_exclude(true)
                .parents(true)
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.into_path();
                if seen_open_paths.iter().any(|open| paths_equivalent(open, &path)) {
                    continue;
                }
                let content = match std::fs::read_to_string(&path) {
                    Ok(content) => content,
                    Err(_) => continue,
                };
                for (index, line) in content.lines().enumerate() {
                    if let Some(text_match) =
                        match_line(&path, u32::try_from(index + 1).unwrap_or(u32::MAX), line)?
                    {
                        matches.push(text_match);
                        if matches.len() >= PROXY_SEARCH_TEXT_LIMIT {
                            break 'roots;
                        }
                    }
                }
            }
        }
        Ok(matches)
    }

    /// The recorded agent file operations (tests, future checkpointing).
    #[allow(dead_code)]
    pub(crate) fn agents_action_log(&self) -> &[ActionLogEntry] {
        &self.agents.action_log
    }
}

fn validate_read_window(
    line: Option<u32>,
    limit: Option<u32>,
    line_count: Option<usize>,
) -> Result<usize, AgentError> {
    if matches!(line, Some(0)) {
        return Err(AgentError::invalid_params("line must be 1-based"));
    }
    if let Some(limit) = limit {
        let count = limit as usize;
        if count > BRIDGE_READ_MAX_LINES {
            return Err(AgentError::invalid_params(format!(
                "line limit {count} exceeds the {BRIDGE_READ_MAX_LINES} cap"
            )));
        }
    }
    let start = line.map(|line| (line - 1) as usize).unwrap_or(0);
    if let Some(line_count) = line_count
        && start > line_count
    {
        return Err(AgentError::invalid_params(format!(
            "start line {} is beyond the end of the file ({line_count} lines)",
            line.unwrap_or(1)
        )));
    }
    Ok(start)
}

/// Applies ACP read-window semantics and unbounded-read caps to `content`.
fn read_text_window(
    content: &str,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, AgentError> {
    let lines = split_lines(content);
    let start = validate_read_window(line, limit, Some(lines.len()))?;
    let selected = if let Some(limit) = limit {
        let count = limit as usize;
        lines.into_iter().skip(start).take(count).collect::<Vec<_>>()
    } else {
        let mut tail = lines.into_iter().skip(start).collect::<Vec<_>>();
        if line.is_some() || start == 0 {
            tail.truncate(BRIDGE_READ_MAX_LINES);
        }
        tail
    };
    let mut text = selected.join("\n");
    if limit.is_none() && text.len() > BRIDGE_READ_MAX_BYTES {
        let mut cut = BRIDGE_READ_MAX_BYTES;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Ok(text)
}

fn visible_child_paths(dir: &Path) -> BTreeSet<PathBuf> {
    let mut visible = BTreeSet::new();
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.into_path();
        if path.parent() == Some(dir) {
            visible.insert(path);
        }
    }
    visible
}

fn visible_descendant_paths(root: &Path) -> BTreeSet<PathBuf> {
    let mut visible = BTreeSet::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            visible.insert(entry.into_path());
        }
    }
    visible
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with('.'))
}

fn file_kind(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => String::from("symlink"),
        Ok(metadata) if metadata.is_dir() => String::from("directory"),
        Ok(metadata) if metadata.is_file() => String::from("file"),
        Ok(_) => String::from("other"),
        Err(_) => String::from("other"),
    }
}

fn entry_size(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

fn buffer_visible_size(buf: &crate::buffer::BufState) -> u64 {
    if buf.is_vlf {
        return 0;
    }
    buf.lines.iter().map(|line| line.len() + 1).sum::<usize>() as u64
}

type PathMatcher = Box<dyn Fn(&Path, &Path) -> bool + Send + Sync>;

fn build_path_matcher(pattern: &str) -> Result<PathMatcher, AgentError> {
    if pattern.chars().any(|ch| matches!(ch, '*' | '?' | '[' | '{')) {
        let glob = Glob::new(pattern)
            .map_err(|error| AgentError::invalid_params(format!("invalid glob pattern: {error}")))?
            .compile_matcher();
        Ok(Box::new(move |rel, path| glob.is_match(rel) || glob.is_match(path)))
    } else {
        let literal = pattern.to_string();
        Ok(Box::new(move |rel, path| {
            let rel = rel.to_string_lossy();
            let path_text = path.to_string_lossy();
            let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            rel.contains(&literal) || path_text.contains(&literal) || file_name.contains(&literal)
        }))
    }
}

fn compile_search_regex(pattern: &str) -> Result<regex::Regex, AgentError> {
    if pattern.is_empty() {
        return Err(AgentError::invalid_params("pattern must not be empty"));
    }
    if pattern.len() > PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES {
        return Err(AgentError::invalid_params(format!(
            "regex pattern exceeds {} bytes",
            PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES
        )));
    }
    regex::RegexBuilder::new(pattern)
        .unicode(true)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 22)
        .build()
        .map_err(|error| AgentError::invalid_params(format!("invalid regex pattern: {error}")))
}

fn trim_search_context(line: &str, query: &str) -> String {
    let Some(start) = line.find(query) else {
        return truncate_chars(line, PROXY_SEARCH_TEXT_CONTEXT_BYTES);
    };
    let end = start.saturating_add(query.len());
    let left_budget = PROXY_SEARCH_TEXT_CONTEXT_BYTES / 2;
    let right_budget = PROXY_SEARCH_TEXT_CONTEXT_BYTES.saturating_sub(left_budget);
    let left_start = previous_char_boundary(line, start.saturating_sub(left_budget));
    let right_end = next_char_boundary(line, (end + right_budget).min(line.len()));
    let mut context = line[left_start..right_end].to_string();
    if left_start > 0 {
        context = format!("…{context}");
    }
    if right_end < line.len() {
        context.push('…');
    }
    context
}

fn trim_regex_context(line: &str, regex: &regex::Regex) -> String {
    regex.find(line).map_or_else(
        || truncate_chars(line, PROXY_SEARCH_TEXT_CONTEXT_BYTES),
        |found| trim_search_context(line, found.as_str()),
    )
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = text[..cut].to_string();
    if cut < text.len() {
        truncated.push('…');
    }
    truncated
}

/// Equivalent-path check: canonical equality when both resolve, lexical
/// equality otherwise.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return ca == cb;
    }
    a == b
}

// ── Unit tests (pure helpers) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_matches_editor_line_model() {
        assert_eq!(split_lines("a\nb\n"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_lines("a\nb"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_lines(""), vec![String::new()]);
        assert_eq!(split_lines("\n"), vec![String::new()]);
    }

    /// Simulates the backend application of hunks (inclusive line ranges,
    /// insertion anchoring) and returns the resulting lines.
    fn apply_hunks(old_lines: &[String], hunks: &[(usize, usize, Vec<String>)]) -> Vec<String> {
        let mut lines = old_lines.to_vec();
        for (start, end, new_lines) in hunks.iter().rev() {
            let start = *start;
            if start == *end {
                let anchor = lines.get(start).or_else(|| lines.last()).cloned().unwrap_or_default();
                let mut replacement = new_lines.clone();
                replacement.push(anchor);
                let last = lines.len().saturating_sub(1);
                let target = start.min(last);
                lines.splice(target..=target, replacement);
            } else {
                lines.splice(start..=end.saturating_sub(1), new_lines.iter().cloned());
            }
        }
        lines
    }

    #[test]
    fn diff_hunks_reconstruct_target_when_applied() {
        let old = split_lines("alpha\nbeta\ngamma");
        let new = split_lines("alpha\nBETA\ngamma\ndelta");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_merge_adjacent_changes() {
        let old = split_lines("one\ntwo\nthree");
        let new = split_lines("one\n2\n3\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_handle_deletions_and_insertions() {
        let old = split_lines("one\ntwo\nthree");
        let new = split_lines("one\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);

        let new = split_lines("one\ntwo\n2.5\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_are_empty_for_equal_content() {
        let lines = split_lines("same\ncontent");
        assert!(diff_hunks(&lines, &lines).is_empty());
    }

    #[test]
    fn bounded_output_truncates_from_front_at_char_boundary() {
        let mut output = BoundedOutput::new(10);
        output.push("hello world".as_bytes());
        assert!(output.truncated());
        assert_eq!(output.as_string(), "ello world");
        assert_eq!(output.total(), 11);
    }

    #[test]
    fn bounded_output_keeps_final_visible_output() {
        let mut output = BoundedOutput::new(5);
        output.push("aaaa".as_bytes());
        output.push("bbb".as_bytes());
        assert_eq!(output.as_string(), "aabbb");
        assert!(output.truncated());
    }

    #[test]
    fn secret_env_keys_are_detected_case_insensitively() {
        for name in ["TOKEN", "API_KEY", "secret", "Password", "AWS_ACCESS_KEY_ID", "CREDENTIALS"] {
            assert!(is_secret_env_key(name), "{name} must be secret-like");
        }
        for name in ["PATH", "HOME", "EDITOR", "SHELL"] {
            assert!(!is_secret_env_key(name), "{name} must not be secret-like");
        }
    }

    #[test]
    fn env_display_redacts_secret_values() {
        let env = vec![EnvVariable::new("PATH", "/bin"), EnvVariable::new("API_TOKEN", "hunter2")];
        let redacted = redact_env_display(&env);
        assert_eq!(redacted[0], ("PATH".to_string(), "/bin".to_string()));
        assert_eq!(redacted[1], ("API_TOKEN".to_string(), "***".to_string()));
    }

    #[test]
    fn read_caps_truncate_unbounded_reads() {
        let content = "x\n".repeat(200_000);
        let capped = read_text_window(&content, None, None).expect("unbounded read caps");
        assert!(capped.len() <= BRIDGE_READ_MAX_BYTES);
        let bounded = read_text_window(&content, None, Some(3)).expect("bounded read caps");
        assert_eq!(bounded, "x\nx\nx");
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        assert_eq!(fingerprint("same"), fingerprint("same"));
        assert_ne!(fingerprint("same"), fingerprint("other"));
    }

    // ── Approval policy (Phase 7) ───────────────────────────────────────────

    fn write_kind(path: &str) -> ApprovalKind {
        ApprovalKind::Write {
            path: PathBuf::from(path),
            content: String::new(),
            tool_call_id: None,
            expectation: WriteExpectation::Blind,
            reply_kind: WriteReplyKind::FsWrite,
            proxy_edit_count: 0,
        }
    }

    #[test]
    fn policy_allow_once_is_not_recorded() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, ApprovalChoice::AllowOnce);
        assert!(policy.lookup("s1", &fp).is_none(), "allow-once must not persist");
    }

    #[test]
    fn policy_deny_once_is_not_recorded() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, ApprovalChoice::DenyOnce);
        assert!(policy.lookup("s1", &fp).is_none(), "deny-once must not persist");
    }

    #[test]
    fn policy_session_allow_and_deny_are_scoped_and_invalidated() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, ApprovalChoice::AllowSession);
        assert_eq!(policy.lookup("s1", &fp), Some(ApprovalChoice::AllowSession));
        // A different session is unaffected.
        assert!(policy.lookup("s2", &fp).is_none());

        policy.record("s1", &fp, ApprovalChoice::DenySession);
        // Deny wins over allow for the same key.
        assert_eq!(policy.lookup("s1", &fp), Some(ApprovalChoice::DenySession));

        policy.invalidate_session("s1");
        assert!(policy.lookup("s1", &fp).is_none(), "policy dies with the session");
    }

    #[test]
    fn policy_deny_wins_over_allow_for_same_fingerprint() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, ApprovalChoice::AllowSession);
        policy.record("s1", &fp, ApprovalChoice::DenySession);
        assert_eq!(policy.lookup("s1", &fp), Some(ApprovalChoice::DenySession));
    }

    #[test]
    fn approval_fingerprints_differ_by_kind_and_identity() {
        let write = approval_fingerprint(&write_kind("/work/a.txt"));
        let other = approval_fingerprint(&write_kind("/work/b.txt"));
        assert_ne!(write, other);
        let mut request = CreateTerminalRequest::new(SessionId::new("s1"), "cargo");
        request.args = vec![String::from("test")];
        let terminal = approval_fingerprint(&ApprovalKind::TerminalCreate { request });
        assert_ne!(write, terminal);
    }

    #[test]
    fn approval_options_have_no_always_allow() {
        // Allow-always persistence has no safe config-write path; the option
        // must not exist at the schema/option level.
        for (label, _) in approval_options() {
            assert!(
                !label.contains("Always"),
                "allow-always must stay disabled at the option level: {label}"
            );
        }
    }
}
