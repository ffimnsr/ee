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

use std::collections::HashMap;
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
/// Hard cap on how long `terminal/wait_for_exit` may poll.
pub(crate) const BRIDGE_TERMINAL_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How many bytes each terminal output reader flushes per read.
const TERMINAL_READER_CHUNK: usize = 4096;

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

/// Shared registry of agent terminals (UI spawns, worker queries/kills).
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentTerminals {
    inner: Arc<Mutex<HashMap<String, AgentTerminalTrack>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
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
        let terminal_id =
            format!("term-{}", self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

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
            command: request.command.clone(),
            args: request.args.clone(),
            cwd: request.cwd.clone(),
            output,
            child: Some(child),
            exit_status: None,
            released: false,
        };
        let mut registry = self.inner.lock().expect("terminals poisoned");
        if registry.contains_key(&terminal_id) {
            return Err(AgentError::HandlerError("terminal id collision".into()));
        }
        registry.insert(terminal_id.clone(), track);
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
        let Some(track) = registry.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.refresh_exit(track);
        let output = track.output.lock().expect("output poisoned");
        let mut response = TerminalOutputResponse::new(output.as_string(), output.truncated());
        response.exit_status = track.exit_status.clone();
        Ok(response)
    }

    /// Waits for the terminal to exit (async polling; cancellable by dropping
    /// the awaiting handler future).
    ///
    /// # Errors
    ///
    /// Fails with `InvalidParams` for unknown terminals and `Io` on timeout.
    pub(crate) async fn wait_for_exit(
        &self,
        request: &WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, AgentError> {
        let deadline = Instant::now() + BRIDGE_TERMINAL_WAIT_TIMEOUT;
        loop {
            {
                let mut registry = self.inner.lock().expect("terminals poisoned");
                let Some(track) = registry.get_mut(request.terminal_id.0.as_ref()) else {
                    return Err(AgentError::invalid_params("unknown terminal"));
                };
                self.refresh_exit(track);
                if let Some(exit_status) = track.exit_status.clone() {
                    return Ok(WaitForTerminalExitResponse::new(exit_status));
                }
            }
            if Instant::now() >= deadline {
                return Err(AgentError::Io("terminal/wait_for_exit timed out".into()));
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
        let Some(track) = registry.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        if let Some(child) = track.child.as_mut() {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                track.exit_status = Some(exit_status_of(&status));
            }
            track.child = None;
        }
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
        let Some(mut track) = registry.remove(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        track.released = true;
        if let Some(mut child) = track.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(ReleaseTerminalResponse::new())
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

    /// Kills every tracked terminal and clears the registry (app shutdown).
    pub(crate) fn kill_all(&self) {
        let registry = std::mem::take(&mut *self.inner.lock().expect("terminals poisoned"));
        for (_, mut track) in registry {
            if let Some(mut child) = track.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Number of tracked terminals (tests and status lines).
    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.inner.lock().expect("terminals poisoned").len()
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
}

impl ClientRequestHandler for BridgeUiHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities::all()
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            match request {
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
#[derive(Debug)]
pub(crate) enum ApprovalKind {
    Write { path: PathBuf, content: String, tool_call_id: Option<String> },
    TerminalCreate { request: CreateTerminalRequest },
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
    pub(crate) kind: ApprovalKind,
    pub(crate) reply: oneshot::Sender<ClientRequestResult>,
}

impl ApprovalPrompt {
    fn write(
        thread_index: Option<usize>,
        session_id: &SessionId,
        request: &WriteTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let path = request.path.display().to_string();
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            title: String::from("fs/write_text_file"),
            detail: format!("{path} ({} bytes)", request.content.len()),
            options: approval_options(),
            selected: 0,
            kind: ApprovalKind::Write {
                path: request.path.clone(),
                content: request.content.clone(),
                tool_call_id: None,
            },
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
            super::agents_mcp::ProxyToolCall::Read(request) => {
                self.bridge_read_file(&request, reply);
            }
            super::agents_mcp::ProxyToolCall::Write(request) => {
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

        // Disk fallback: workspace scope only.
        if !self.path_in_workspace(&request.path) {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                request.path.display()
            ))));
            return;
        }
        match std::fs::read_to_string(&request.path) {
            Ok(content) => {
                let content = apply_read_caps(&content, request.line, request.limit);
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
        let start =
            request.line.map(|line| (line.saturating_sub(1)) as usize).unwrap_or(0).min(line_count);
        if start > line_count {
            return Err(AgentError::invalid_params(format!(
                "start line {} is beyond the end of the file ({line_count} lines)",
                request.line.unwrap_or(1)
            )));
        }
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
        if let Some(limit) = request.limit {
            let count = limit as usize;
            if count > BRIDGE_READ_MAX_LINES {
                return Err(AgentError::invalid_params(format!(
                    "line limit {count} exceeds the {BRIDGE_READ_MAX_LINES} cap"
                )));
            }
            let end = start.saturating_add(count).min(line_count);
            let lines = buf.line_range_owned(start, end.saturating_sub(1)).unwrap_or_default();
            let content = lines.join("\n");
            let bytes = content.len();
            Ok((content, bytes))
        } else {
            let content = apply_read_caps(&buf.whole_text().unwrap_or_default(), None, None);
            let bytes = content.len();
            Ok((content, bytes))
        }
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
            ApprovalKind::Write { path, content, tool_call_id } => {
                self.apply_bridge_write(
                    &path,
                    &content,
                    tool_call_id,
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
        path: &Path,
        content: &str,
        tool_call_id: Option<String>,
        session_id: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        if !path.is_absolute() {
            let _ = reply.send(Err(AgentError::invalid_params("path must be absolute")));
            return;
        }
        if !self.path_in_workspace(path) {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            ))));
            return;
        }
        match self.write_through_buffer(path, content) {
            Ok((old_content, new_fingerprint)) => {
                self.agents.action_log.push(ActionLogEntry::Write {
                    path: path.to_path_buf(),
                    old_fingerprint: fingerprint(&old_content),
                    new_fingerprint,
                    tool_call_id,
                    session_id: session_id.to_string(),
                });
                if let Some(thread) = self.session_thread_by_id(session_id) {
                    self.agents.threads[thread]
                        .push_system(format!("agent wrote: {}", path.display()));
                }
                let _ = reply
                    .send(Ok(ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new())));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Opens/reuses the buffer, applies the minimal diff, verifies, saves.
    fn write_through_buffer(
        &mut self,
        path: &Path,
        content: &str,
    ) -> Result<(String, u64), AgentError> {
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
        Ok((old_content, fingerprint(content)))
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
        let roots = self.agents_workspace_roots();
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        roots.iter().any(|root| {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            canonical.starts_with(&root)
        })
    }

    /// The recorded agent file operations (tests, future checkpointing).
    #[allow(dead_code)]
    pub(crate) fn agents_action_log(&self) -> &[ActionLogEntry] {
        &self.agents.action_log
    }
}

/// Applies the unbounded-read caps (line and byte) to `content`.
fn apply_read_caps(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_some() && limit.is_none() {
        return content.to_string();
    }
    let lines = split_lines(content);
    let mut effective = lines;
    if let Some(limit) = limit {
        let count = (limit as usize).min(BRIDGE_READ_MAX_LINES);
        effective.truncate(count);
    } else {
        effective.truncate(BRIDGE_READ_MAX_LINES);
    }
    let mut text = effective.join("\n");
    if text.len() > BRIDGE_READ_MAX_BYTES {
        let mut cut = BRIDGE_READ_MAX_BYTES;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
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
        let capped = apply_read_caps(&content, None, None);
        assert!(capped.len() <= BRIDGE_READ_MAX_BYTES);
        let bounded = apply_read_caps(&content, None, Some(3));
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
