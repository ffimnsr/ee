//! Bounded terminal registry, output capture, environment and command rules.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ee_agent_host::{AgentError, EvidenceRevision};
use ee_agent_protocol::{
    CreateTerminalRequest, CreateTerminalResponse, EnvVariable, KillTerminalRequest,
    KillTerminalResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, SessionId,
    TerminalExitStatus, TerminalId, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};

use crate::policy::match_profile_entry;

/// Editor-side hard cap on retained terminal output bytes.
pub(crate) const BRIDGE_TERMINAL_OUTPUT_CAP: usize = 1024 * 1024;
/// How many bytes each terminal output reader flushes per read.
const TERMINAL_READER_CHUNK: usize = 4096;

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

/// Git variable that prevents read commands from taking optional index locks.
const GIT_OPTIONAL_LOCKS_ENV: &str = "GIT_OPTIONAL_LOCKS";

/// Whether a request is one of the fixed, application-owned Git read commands.
fn is_git_readonly_profile_request(request: &CreateTerminalRequest) -> bool {
    matches!(match_profile_entry(&request.command, &request.args), Some(("git_readonly", _)))
}

/// Child environment: the parent environment minus secret-like keys, overlaid
/// with explicitly configured request values. Curated Git reads cannot take an
/// optional index lock, even when a caller supplied a conflicting environment.
fn terminal_child_env(request: &CreateTerminalRequest) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        std::env::vars().filter(|(name, _)| !is_secret_env_key(name)).collect();
    for variable in &request.env {
        env.push((variable.name.clone(), variable.value.clone()));
    }
    if is_git_readonly_profile_request(request) {
        env.retain(|(name, _)| name != GIT_OPTIONAL_LOCKS_ENV);
        env.push((String::from(GIT_OPTIONAL_LOCKS_ENV), String::from("0")));
    }
    env
}

/// Shell command text for an approved terminal request.
///
/// The ACP `command` field is agent-authored shell text. Explicit `args` are
/// appended as shell-quoted literals so an argument cannot alter that command.
pub(super) fn terminal_command_line(request: &CreateTerminalRequest) -> String {
    let mut command = request.command.clone();
    for arg in &request.args {
        command.push(' ');
        command.push_str(&crate::terminal::shell_quote(arg));
    }
    command
}

fn terminal_command_line_for_track(track: &AgentTerminalTrack) -> String {
    let mut command = track.command.clone();
    for arg in &track.args {
        command.push(' ');
        command.push_str(&crate::terminal::shell_quote(arg));
    }
    command
}

// ── Bounded output ring buffer ───────────────────────────────────────────────

/// Origin of one retained terminal-output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalOutputStream {
    Stdout,
    Stderr,
}

/// One retained terminal-output chunk. Sequence numbers are per terminal and
/// strictly increase in observed reader-write order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalOutputChunk {
    pub(crate) sequence: u64,
    pub(crate) stream: TerminalOutputStream,
    pub(crate) text: String,
}

/// Structured terminal-output view for MCP callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalOutputSnapshot {
    /// ACP-compatible combined output; structured callers should use `chunks`.
    pub(crate) combined_output: String,
    pub(crate) chunks: Vec<TerminalOutputChunk>,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
    pub(crate) exit_status: Option<TerminalExitStatus>,
    pub(crate) running: bool,
    pub(crate) elapsed_ms: u64,
}

#[derive(Debug)]
struct RetainedOutputChunk {
    sequence: u64,
    stream: TerminalOutputStream,
    bytes: Vec<u8>,
}

/// Byte-bounded terminal output: preserves stdout/stderr chunk boundaries and
/// truncates oldest output from the front at UTF-8 character boundaries.
#[derive(Debug, Default)]
pub(crate) struct BoundedOutput {
    chunks: VecDeque<RetainedOutputChunk>,
    retained_bytes: usize,
    total: u64,
    truncated: bool,
    cap: usize,
    next_sequence: u64,
}

impl BoundedOutput {
    /// Creates an empty buffer with `cap` max retained bytes.
    #[must_use]
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            retained_bytes: 0,
            total: 0,
            truncated: false,
            cap,
            next_sequence: 1,
        }
    }

    /// Appends one stream-tagged chunk, enforcing the shared byte cap.
    pub(crate) fn push(&mut self, stream: TerminalOutputStream, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }

        self.total += chunk.len() as u64;
        self.chunks.push_back(RetainedOutputChunk {
            sequence: self.next_sequence,
            stream,
            bytes: chunk.to_vec(),
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.retained_bytes += chunk.len();
        self.truncate_to_cap();
    }

    fn truncate_to_cap(&mut self) {
        while self.retained_bytes > self.cap {
            let required = self.retained_bytes - self.cap;
            let front = self.chunks.front_mut().expect("retained output must have a chunk");
            let mut cut = required.min(front.bytes.len());
            // Advance to a UTF-8 char boundary so retained combined output
            // remains valid when the source stream is UTF-8.
            while cut < front.bytes.len() && front.bytes[cut] & 0xC0 == 0x80 {
                cut += 1;
            }
            front.bytes.drain(..cut);
            self.retained_bytes -= cut;
            self.truncated = true;
            if front.bytes.is_empty() {
                self.chunks.pop_front();
            }
        }
    }

    /// Retained output as ACP-compatible combined lossy text.
    #[must_use]
    pub(crate) fn as_string(&self) -> String {
        let mut bytes = Vec::with_capacity(self.retained_bytes);
        for chunk in &self.chunks {
            bytes.extend_from_slice(&chunk.bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[must_use]
    fn chunks(&self) -> Vec<TerminalOutputChunk> {
        self.chunks
            .iter()
            .map(|chunk| TerminalOutputChunk {
                sequence: chunk.sequence,
                stream: chunk.stream,
                text: String::from_utf8_lossy(&chunk.bytes).into_owned(),
            })
            .collect()
    }

    /// Whether any bytes were dropped by the cap.
    #[must_use]
    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// Total bytes ever pushed.
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

/// Agent/session identity used for local terminal ownership checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalOwner {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
}

/// Bounded local display record for one currently tracked agent terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedTerminalSummary {
    pub(crate) terminal_id: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) running: bool,
    pub(crate) exit_status: Option<TerminalExitStatus>,
    pub(crate) elapsed_ms: u64,
    pub(crate) output_tail: String,
    pub(crate) output_total_bytes: u64,
    pub(crate) output_truncated: bool,
}

/// Result of nonblocking local stop request for one owned terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedTerminalStop {
    StopRequested,
    AlreadyExited,
}

/// Bounded terminal completion sent from worker bridge back to pane. Never
/// contains command output; evidence records retain only structured outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalCompletion {
    pub(crate) session_id: String,
    pub(crate) terminal_id: String,
    pub(crate) command: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed_ms: u64,
    pub(crate) output_truncated: bool,
    pub(super) validation: Option<TerminalValidationRun>,
}

/// Host-owned association between an approved terminal and one current
/// verification revision. It never contains terminal output or model claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalValidationRun {
    pub(super) revision: EvidenceRevision,
    pub(super) selector: String,
    pub(super) diagnostics_before: Option<u32>,
}

/// One tracked agent terminal: child process, bounded output, exit status.
#[derive(Debug)]
pub(crate) struct AgentTerminalTrack {
    #[allow(dead_code)]
    pub(crate) terminal_id: String,
    #[allow(dead_code)]
    pub(crate) owner: TerminalOwner,
    #[allow(dead_code)]
    pub(crate) command: String,
    #[allow(dead_code)]
    pub(crate) args: Vec<String>,
    #[allow(dead_code)]
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) output: Arc<Mutex<BoundedOutput>>,
    output_readers: Vec<thread::JoinHandle<()>>,
    child: Option<Child>,
    pub(crate) exit_status: Option<TerminalExitStatus>,
    started_at: Instant,
    pub(crate) released: bool,
    validation: Option<TerminalValidationRun>,
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
        owner_agent_id: Option<&str>,
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

        let cwd = request.cwd.as_deref().unwrap_or_else(|| Path::new("."));
        let command_line = terminal_command_line(request);
        let mut command = crate::terminal::shell_command(&command_line, cwd);
        command.envs(terminal_child_env(request));
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| AgentError::Io(format!("terminal spawn failed: {error}")))?;

        let output = Arc::new(Mutex::new(BoundedOutput::new(cap)));
        let output_readers = [
            spawn_output_reader(
                child.stdout.take(),
                Arc::clone(&output),
                TerminalOutputStream::Stdout,
            ),
            spawn_output_reader(
                child.stderr.take(),
                Arc::clone(&output),
                TerminalOutputStream::Stderr,
            ),
        ]
        .into_iter()
        .flatten()
        .collect();

        let track = AgentTerminalTrack {
            terminal_id: terminal_id.clone(),
            owner: TerminalOwner {
                agent_id: owner_agent_id.unwrap_or("proxy").to_string(),
                session_id: request.session_id.0.to_string(),
            },
            command: request.command.clone(),
            args: request.args.clone(),
            cwd: request.cwd.clone(),
            output,
            output_readers,
            child: Some(child),
            exit_status: None,
            started_at: Instant::now(),
            released: false,
            validation: None,
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
        Ok(Self::output_response(self.output_snapshot(request)?))
    }

    /// Snapshot retained output chunks for a terminal owned by this session.
    ///
    /// This is the structured counterpart to ACP's combined-text terminal
    /// output response. Released terminals intentionally remain unavailable
    /// through this ownership-checked API.
    ///
    /// # Errors
    ///
    /// Fails when the terminal id is unknown or belongs to another session.
    pub(crate) fn output_snapshot(
        &self,
        request: &TerminalOutputRequest,
    ) -> Result<TerminalOutputSnapshot, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(track, &request.session_id)?;
        self.refresh_exit(track);
        Ok(Self::output_snapshot_for_track(track))
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

    /// Returns bounded post-exit metadata for pane-owned evidence collection.
    pub(super) fn completion(
        &self,
        request: &WaitForTerminalExitRequest,
    ) -> Result<TerminalCompletion, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(request.terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(track, &request.session_id)?;
        self.refresh_exit(track);
        let snapshot = Self::output_snapshot_for_track(track);
        let exit_code = snapshot.exit_status.and_then(|status| {
            serde_json::to_value(status)
                .ok()
                .and_then(|value| {
                    value
                        .get("exitCode")
                        .or_else(|| value.get("exit_code"))
                        .and_then(serde_json::Value::as_u64)
                })
                .and_then(|code| i32::try_from(code).ok())
        });
        Ok(TerminalCompletion {
            session_id: track.owner.session_id.clone(),
            terminal_id: track.terminal_id.clone(),
            command: terminal_command_line_for_track(track),
            exit_code,
            elapsed_ms: snapshot.elapsed_ms,
            output_truncated: snapshot.truncated,
            validation: track.validation.clone(),
        })
    }

    /// Associates an approved terminal with current write verification.
    pub(super) fn register_validation_run(
        &self,
        terminal_id: &TerminalId,
        session_id: &SessionId,
        validation: TerminalValidationRun,
    ) -> Result<(), AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(terminal_id.0.as_ref()) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        self.validate_owner(track, session_id)?;
        track.validation = Some(validation);
        Ok(())
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
        if track.owner.session_id == session_id.0.as_ref() {
            Ok(())
        } else {
            Err(AgentError::invalid_params("terminal does not belong to this session"))
        }
    }

    /// Lists only active terminals owned by exact agent/session identity.
    /// Output remains byte-bounded and terminal environment is never exposed.
    pub(crate) fn list_owned(
        &self,
        owner: &TerminalOwner,
        tail_bytes: usize,
    ) -> Vec<OwnedTerminalSummary> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let mut terminals = registry
            .active
            .values_mut()
            .filter(|track| &track.owner == owner)
            .map(|track| {
                self.refresh_exit(track);
                let snapshot = Self::output_snapshot_for_track(track);
                let output_tail = tail_at_char_boundary(&snapshot.combined_output, tail_bytes);
                OwnedTerminalSummary {
                    terminal_id: track.terminal_id.clone(),
                    command: track.command.clone(),
                    args: track.args.clone(),
                    cwd: track.cwd.clone(),
                    running: snapshot.running,
                    exit_status: snapshot.exit_status,
                    elapsed_ms: snapshot.elapsed_ms,
                    output_tail,
                    output_total_bytes: snapshot.total_bytes,
                    output_truncated: snapshot.truncated,
                }
            })
            .collect::<Vec<_>>();
        terminals.sort_by(|left, right| left.terminal_id.cmp(&right.terminal_id));
        terminals
    }

    /// Requests stop for one exact-owned direct child without waiting or joining readers.
    /// Descendant process trees are outside this bounded local operation.
    pub(crate) fn stop_owned(
        &self,
        owner: &TerminalOwner,
        terminal_id: &str,
    ) -> Result<OwnedTerminalStop, AgentError> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        let Some(track) = registry.active.get_mut(terminal_id) else {
            return Err(AgentError::invalid_params("unknown terminal"));
        };
        if &track.owner != owner {
            return Err(AgentError::invalid_params(
                "terminal does not belong to active agent session",
            ));
        }
        self.refresh_exit(track);
        let Some(child) = track.child.as_mut() else {
            return Ok(OwnedTerminalStop::AlreadyExited);
        };
        child.kill().map_err(|error| AgentError::Io(format!("terminal stop failed: {error}")))?;
        Ok(OwnedTerminalStop::StopRequested)
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
        Self::join_output_readers(track);
        Ok(())
    }

    fn output_response(snapshot: TerminalOutputSnapshot) -> TerminalOutputResponse {
        let mut response =
            TerminalOutputResponse::new(snapshot.combined_output, snapshot.truncated);
        response.exit_status = snapshot.exit_status;
        response
    }

    fn output_snapshot_for_track(track: &AgentTerminalTrack) -> TerminalOutputSnapshot {
        let output = track.output.lock().expect("output poisoned");
        TerminalOutputSnapshot {
            combined_output: output.as_string(),
            chunks: output.chunks(),
            total_bytes: output.total(),
            truncated: output.truncated(),
            exit_status: track.exit_status.clone(),
            running: track.child.is_some(),
            elapsed_ms: track.started_at.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }
    }

    fn join_output_readers(track: &mut AgentTerminalTrack) {
        for reader in track.output_readers.drain(..) {
            let _ = reader.join();
        }
    }

    /// Reaps the child when it exited, waits for output readers to drain, and
    /// caches the exit status.
    fn refresh_exit(&self, track: &mut AgentTerminalTrack) {
        if track.exit_status.is_none()
            && let Some(child) = track.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            track.exit_status = Some(exit_status_of(&status));
            track.child = None;
            Self::join_output_readers(track);
        }
    }

    #[cfg(test)]
    pub(crate) fn display_output(&self, terminal_id: &str) -> Option<TerminalOutputResponse> {
        let mut registry = self.inner.lock().expect("terminals poisoned");
        if let Some(track) = registry.active.get_mut(terminal_id) {
            self.refresh_exit(track);
            return Some(Self::output_response(Self::output_snapshot_for_track(track)));
        }
        registry
            .released
            .get(terminal_id)
            .map(Self::output_snapshot_for_track)
            .map(Self::output_response)
    }

    /// Kills every tracked terminal and clears the registry (app shutdown).
    pub(crate) fn kill_all(&self) {
        let registry = std::mem::take(&mut *self.inner.lock().expect("terminals poisoned"));
        for (_, mut track) in registry.active.into_iter().chain(registry.released) {
            if let Some(mut child) = track.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Self::join_output_readers(&mut track);
        }
    }

    /// Number of tracked terminals (tests and status lines).
    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.inner.lock().expect("terminals poisoned").active.len()
    }
}

fn tail_at_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[tail]\n{}", &text[start..])
}

fn spawn_output_reader(
    stream: Option<impl std::io::Read + Send + 'static>,
    output: Arc<Mutex<BoundedOutput>>,
    stream_kind: TerminalOutputStream,
) -> Option<thread::JoinHandle<()>> {
    let mut stream = stream?;
    Some(
        std::thread::Builder::new()
            .name(String::from("ee-agent-terminal-output"))
            .spawn(move || {
                let mut buffer = [0u8; TERMINAL_READER_CHUNK];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            output.lock().expect("output poisoned").push(stream_kind, &buffer[..n])
                        }
                    }
                }
            })
            .expect("spawn terminal output reader"),
    )
}

#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_truncates_from_front_at_char_boundary() {
        let mut output = BoundedOutput::new(10);
        output.push(TerminalOutputStream::Stdout, "hello world".as_bytes());
        assert!(output.truncated());
        assert_eq!(output.as_string(), "ello world");
        assert_eq!(output.total(), 11);
    }

    #[test]
    fn terminal_command_line_preserves_shell_command_and_quotes_explicit_args() {
        let mut request = CreateTerminalRequest::new(SessionId::new("s1"), "ls -la");
        request.args = vec![String::from("path with spaces"), String::from("$(not-expanded)")];

        assert_eq!(terminal_command_line(&request), "ls -la 'path with spaces' '$(not-expanded)'");
    }

    #[test]
    fn git_readonly_profile_commands_disable_optional_index_locks() {
        let mut request = CreateTerminalRequest::new(SessionId::new("s1"), "git");
        request.args = vec![String::from("status")];
        request.env = vec![EnvVariable::new(GIT_OPTIONAL_LOCKS_ENV, "1")];

        let env = terminal_child_env(&request);
        let optional_locks =
            env.iter().filter(|(name, _)| name == GIT_OPTIONAL_LOCKS_ENV).collect::<Vec<_>>();
        assert_eq!(
            optional_locks,
            vec![&(String::from(GIT_OPTIONAL_LOCKS_ENV), String::from("0"))]
        );
    }

    #[test]
    fn other_terminal_commands_preserve_optional_lock_environment() {
        let mut request = CreateTerminalRequest::new(SessionId::new("s1"), "git");
        request.args = vec![String::from("commit")];
        request.env = vec![EnvVariable::new(GIT_OPTIONAL_LOCKS_ENV, "1")];

        let env = terminal_child_env(&request);
        assert!(env.iter().any(|(name, value)| name == GIT_OPTIONAL_LOCKS_ENV && value == "1"));
    }

    #[test]
    fn bounded_output_keeps_final_visible_output() {
        let mut output = BoundedOutput::new(5);
        output.push(TerminalOutputStream::Stdout, "aaaa".as_bytes());
        output.push(TerminalOutputStream::Stdout, "bbb".as_bytes());
        assert_eq!(output.as_string(), "aabbb");
        assert!(output.truncated());
    }

    #[test]
    fn bounded_output_retains_stream_chunks_with_monotonic_sequence_ids() {
        let mut output = BoundedOutput::new(5);
        output.push(TerminalOutputStream::Stdout, b"abcd");
        output.push(TerminalOutputStream::Stderr, "é!".as_bytes());

        assert_eq!(output.total(), 7);
        assert!(output.truncated());
        assert_eq!(output.as_string(), "cdé!");
        assert_eq!(
            output.chunks(),
            vec![
                TerminalOutputChunk {
                    sequence: 1,
                    stream: TerminalOutputStream::Stdout,
                    text: String::from("cd"),
                },
                TerminalOutputChunk {
                    sequence: 2,
                    stream: TerminalOutputStream::Stderr,
                    text: String::from("é!"),
                },
            ]
        );
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
}
