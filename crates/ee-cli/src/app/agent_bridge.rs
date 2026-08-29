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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Stdio};
#[cfg(test)]
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ee_agent_host::{
    AgentError, ClientRequest, ClientRequestHandler, ClientRequestResponse, ClientRequestResult,
    EvidenceCheck, EvidenceRevision, HandlerCapabilities, HostValidationRecord, TurnObservation,
    WriteEvidenceOutcome, WriteTransactionStage,
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
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::*;

use super::agents_mcp::ProxyRoute;
use super::write_leases::{WriteLeaseId, WriteLeaseOwner};

#[cfg(test)]
type WriteVerificationTestHook = Box<dyn FnOnce(&mut App) + Send>;

#[cfg(test)]
static PRE_WRITE_VERIFICATION_TEST_HOOK: LazyLock<Mutex<Option<WriteVerificationTestHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
static POST_WRITE_TEST_HOOK: LazyLock<Mutex<Option<WriteVerificationTestHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
static WEB_DISPATCH_TEST_COUNT: AtomicUsize = AtomicUsize::new(0);

use crate::policy::{
    BoundedRuleCandidate, BoundedRulePreview, BrowserActionClass, CATASTROPHIC_DELETE_RULE_ID,
    CommandInvocation, CommandRule, DecisionReason, FilesystemOperationKind, FilesystemRule,
    HostMatchMode, MAX_WRITE_FILE_BYTES, MAX_WRITE_FILES, MAX_WRITE_TOTAL_BYTES, MatchMode,
    McpDenyRule, McpInvocation, NetworkMethodClass, NetworkRule, NetworkScheme, OperationIdentity,
    PathPrefix, PolicyInput, SafeguardCategory, SafeguardMatch, TERMINAL_READONLY_PROFILE,
    ToolRule, ToolRuleIdentity, TransportKind, TrustCategory, TrustDecision, TrustEffect,
    TrustOperation, TrustOutcome, TrustRule, TrustRuleScope, TrustStore, TrustStoreDocument,
    TrustStoreError, WorkspaceIdentity, WriteOperationKind, WriteRule, evaluate,
    generate_command_rule_id, generate_filesystem_rule_id, generate_mcp_rule_id,
    generate_network_rule_id, generate_tool_rule_id, generate_write_rule_id, inspect_path_escape,
    inspect_protected_state_path, inspect_special_file, inspect_terminal_command,
    is_protected_relative_path, match_profile_entry, resolve_command_cwd, validate_argv_tokens,
    validate_command_tokens,
};

// ── Policy constants ─────────────────────────────────────────────────────────

/// The persistent terminal approval option label.
pub(crate) const PERSISTENT_TERMINAL_OPTION_LABEL: &str = "Allow for 1 hour / 20 uses";

/// The persistent write approval option label.
pub(crate) const PERSISTENT_WRITE_OPTION_LABEL: &str = "Allow for 1 hour / 5 uses";

/// Session key and fingerprint used for normalized read evaluations
/// (Phase 4): reads are prompt-free today and never record session
/// decisions, so the session state stays empty for these keys.
const READ_SESSION: &str = "read";
const READ_FINGERPRINT: &str = "read";

/// Hard cap on lines served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_LINES: usize = 100_000;
/// Hard cap on bytes served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_BYTES: usize = 1024 * 1024;
/// Editor-side hard cap on retained terminal output bytes.
pub(crate) const BRIDGE_TERMINAL_OUTPUT_CAP: usize = 1024 * 1024;
/// How many bytes each terminal output reader flushes per read.
const TERMINAL_READER_CHUNK: usize = 4096;
/// Cap on entries returned by one `ee_list_directory` call.
const PROXY_LIST_DIRECTORY_LIMIT: usize = 500;
/// Cap on matches returned by one `ee_search_files` call.
const PROXY_SEARCH_FILES_LIMIT: usize = 500;
/// Cap on matches returned by one `ee_search_text` call.
const PROXY_SEARCH_TEXT_LIMIT: usize = 200;
/// Max visible context bytes returned for one `ee_search_text` match.
const PROXY_SEARCH_TEXT_CONTEXT_BYTES: usize = 200;
/// Cap on diagnostics returned by one Phase 3 diagnostics tool.
const PROXY_DIAGNOSTICS_LIMIT: usize = 500;
/// Cap on document symbols returned by one `ee_document_symbols` call.
const PROXY_DOCUMENT_SYMBOLS_LIMIT: usize = 500;
/// Cap on references returned by one `ee_references` call.
const PROXY_REFERENCES_LIMIT: usize = 500;
/// Cap on code actions returned by one `ee_list_code_actions` call.
const PROXY_CODE_ACTIONS_LIMIT: usize = 100;
/// Cap on files returned by one rename preview.
const PROXY_RENAME_FILES_LIMIT: usize = 100;
/// Cap on edits returned by one rename preview.
const PROXY_RENAME_EDITS_LIMIT: usize = 1000;
/// Cap on symbols returned by one review-context request.
const PROXY_REVIEW_SYMBOLS_LIMIT: usize = 500;
/// Cap on changed files queried for document symbols during review-context assembly.
const PROXY_REVIEW_SYMBOL_FILE_LIMIT: usize = 32;
/// Max regex pattern length accepted by `ee_search_text_regex`.
const PROXY_SEARCH_REGEX_MAX_PATTERN_BYTES: usize = 4096;
/// Max wall time spent in one regex search before fail-closed timeout.
const PROXY_SEARCH_REGEX_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_TERMINAL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static NEXT_WEB_LIFECYCLE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn web_context_agent_error(error: ee_agent_host::WebContextError) -> AgentError {
    AgentError::HandlerError(format!("{}: {}", error.code.as_str(), error.message))
}

fn web_context_config_agent_error(error: ee_agent_host::WebContextConfigError) -> AgentError {
    AgentError::HandlerError(format!("web_search_invalid_configuration: {error}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

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
fn terminal_command_line(request: &CreateTerminalRequest) -> String {
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
    validation: Option<TerminalValidationRun>,
}

/// Host-owned association between an approved terminal and one current
/// verification revision. It never contains terminal output or model claims.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalValidationRun {
    revision: EvidenceRevision,
    selector: String,
    diagnostics_before: Option<u32>,
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
    fn completion(
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
    fn register_validation_run(
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
    /// reads and diagnostics are served immediately.  `route` carries the
    /// transport that delivered the call (Phase 3 MCP trust).
    ProxyTool {
        call: super::agents_mcp::ProxyToolCall,
        route: super::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    /// Stdio proxy connection ended. Network grants and pending approvals are
    /// connection-scoped and must not survive a socket lifetime.
    ProxyConnectionClosed {
        scope: String,
    },
    /// Terminal lifecycle completion. Internal pane signal, not ACP or MCP.
    TerminalCompleted {
        completion: TerminalCompletion,
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
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::WorkspaceRoots,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectory { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ListDirectory { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectoryAll { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ListDirectoryAll { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFiles { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SearchFiles { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFilesAll { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SearchFilesAll { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchText { query } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SearchText { query },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextRegex { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SearchTextRegex { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyWebSearch { query, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::WebSearch {
                            query,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFetchUrl { url, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::FetchUrl {
                            url,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyBrowserRun { request, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::BrowserRun {
                            request,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextInFiles { query, file_glob } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ApplyPatch { path, edits },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCreateTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::CreateTextFile { path, content },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyOverwriteTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::OverwriteTextFile { path, content },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCreateDirectory { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::CreateDirectory { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyDeletePath { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::DeletePath { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCopyPath { source_path, destination_path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::CopyPath {
                            source_path,
                            destination_path,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyMovePath { source_path, destination_path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::MovePath {
                            source_path,
                            destination_path,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBuffer { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ReadBuffer { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBufferLines { path, line, limit } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::OpenBuffers,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetDiagnostics => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GetDiagnostics,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetFileDiagnostics { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GetFileDiagnostics { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyDocumentSymbols { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::DocumentSymbols { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReferences { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ApplyCodeAction { path, action_id },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFormatFile { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::FormatFile { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyPreviewRenameSymbol { path, line, character, new_name } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
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
                        route: ProxyRoute::AcpNative,
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
                ClientRequest::ProxyGitStatus => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GitStatus,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiff => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GitDiff,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiffStaged => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GitDiffStaged,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiffFile { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::GitDiffFile { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyChangedFiles => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ChangedFiles,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReviewContext => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ReviewContext,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyProjectInstructions => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ProjectInstructions,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySaveNote { scope, key, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SaveNote { scope, key, content },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadNotes { scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ReadNotes { scope },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadNote { scope, key } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::ReadNote { scope, key },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFileDependencyMap { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::FileDependencyMap { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySymbolDependencyMap { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: super::agents_mcp::ProxyToolCall::SymbolDependencyMap {
                            path,
                            line,
                            character,
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
                ClientRequest::ProxyTerminalOutput(request) => {
                    self.terminals.output_snapshot(&request).and_then(|snapshot| {
                        let chunks = snapshot
                            .chunks
                            .into_iter()
                            .map(|chunk| ee_mcp::TerminalOutputChunk {
                                sequence: chunk.sequence,
                                stream: match chunk.stream {
                                    TerminalOutputStream::Stdout => String::from("stdout"),
                                    TerminalOutputStream::Stderr => String::from("stderr"),
                                },
                                text: chunk.text,
                            })
                            .collect();
                        let exit_status =
                            snapshot.exit_status.map(serde_json::to_value).transpose().map_err(
                                |error| {
                                    AgentError::HandlerError(format!(
                                        "terminal output exit status serialization failed: {error}"
                                    ))
                                },
                            )?;
                        Ok(ClientRequestResponse::ProxyValue(
                            serde_json::to_value(ee_mcp::TerminalOutputResult {
                                output: snapshot.combined_output,
                                chunks,
                                total_bytes: snapshot.total_bytes,
                                truncated: snapshot.truncated,
                                exit_status,
                                running: snapshot.running,
                                elapsed_ms: snapshot.elapsed_ms,
                            })
                            .map_err(|error| {
                                AgentError::HandlerError(format!(
                                    "terminal output serialization failed: {error}"
                                ))
                            })?,
                        ))
                    })
                }
                ClientRequest::WaitForTerminalExit(request) => {
                    let response = self.terminals.wait_for_exit(&request).await?;
                    if let Ok(completion) = self.terminals.completion(&request) {
                        let _ = self.tx.send(BridgeUiMessage::TerminalCompleted { completion });
                    }
                    Ok(ClientRequestResponse::WaitForTerminalExit(response))
                }
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
pub(crate) enum WriteExpectation {
    Blind,
    MustNotExist,
    ExpectRevision(String),
}

/// How the approved write is answered to the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteReplyKind {
    FsWrite,
    ProxyStructured,
}

/// One prepared text write awaiting approval.
#[derive(Debug)]
pub(crate) struct PreparedWrite {
    pub(crate) path: PathBuf,
    pub(crate) content: String,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) expectation: WriteExpectation,
    pub(crate) reply_kind: WriteReplyKind,
    pub(crate) proxy_edit_count: u32,
}

#[derive(Debug)]
struct ProxyWriteSpec {
    title: String,
    detail: String,
    prepared: PreparedWrite,
}

enum WebApprovalCall {
    Search { query: String },
    Fetch { url: String },
    BrowserRun { request: ee_mcp::BrowserRunRequest },
}

impl std::fmt::Debug for WebApprovalCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self {
            Self::Search { .. } => "search",
            Self::Fetch { .. } => "fetch",
            Self::BrowserRun { request } => request.action.as_str(),
        };
        formatter.debug_tuple("WebApprovalCall").field(&action).finish()
    }
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
    Filesystem {
        operation: super::agent_filesystem::FilesystemOperation,
    },
    TerminalCreate {
        request: CreateTerminalRequest,
    },
    /// External network approval carries only host/route in visible or
    /// persisted session state. Query and URL remain private call payloads.
    Network {
        route: ProxyRoute,
        /// Canonical host at original tool invocation.
        requested_host: String,
        /// Canonical host about to receive the current request/redirect.
        current_host: String,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
    },
}

/// Session-local tool approval behavior selected from the agents TUI.
///
/// This controls only whether the UI approval dialog is shown. It never
/// bypasses request validation, workspace boundaries, revision checks, or ACP
/// permission and elicitation prompts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ToolApprovalMode {
    #[default]
    Default,
    Autopilot,
    Bypass,
}

impl ToolApprovalMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Autopilot => "autopilot",
            Self::Bypass => "bypass",
        }
    }
}

/// One approval decision the user can pick (Phase 2 policy).
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
    /// Preview and persist the exact bounded candidate using default limits.
    AllowPersistent,
    /// Preview and persist the exact bounded candidate using shorter fixed limits.
    AllowPersistentShort,
    /// Preview and persist a command argv prefix ending at selected token boundary.
    AllowPersistentPrefix(usize),
    /// Preview and persist a command argv prefix using shorter fixed limits.
    AllowPersistentPrefixShort(usize),
    /// Preview and persist a narrow host-local deny rule before denying.
    DenyPersistent,
}

impl ApprovalChoice {
    fn label(self) -> &'static str {
        match self {
            ApprovalChoice::AllowOnce => "Allow once",
            ApprovalChoice::AllowSession => "Allow session",
            ApprovalChoice::DenyOnce => "Deny",
            ApprovalChoice::DenySession => "Deny session",
            ApprovalChoice::AllowPersistent => PERSISTENT_TERMINAL_OPTION_LABEL,
            ApprovalChoice::AllowPersistentShort => "Allow for 10 minutes / 5 uses",
            ApprovalChoice::AllowPersistentPrefix(_) => "Allow structured command prefix",
            ApprovalChoice::AllowPersistentPrefixShort(_) => {
                "Allow structured command prefix for 10 minutes"
            }
            ApprovalChoice::DenyPersistent => "Deny for this workspace",
        }
    }

    fn allows(self) -> bool {
        matches!(
            self,
            ApprovalChoice::AllowOnce
                | ApprovalChoice::AllowSession
                | ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        )
    }
}

/// Session-scoped approval policy (shared precedence contract, Phase 1
/// foundation).
///
/// `allow_once` / `deny_once` decisions are resolved by the approval UI
/// layer and never recorded; `allow_session` / `deny_session` decisions are
/// remembered per session, keyed by action kind and fingerprint (path for
/// writes, command+args fingerprint for terminals), and invalidated when the
/// session closes.  Allow-always persistence is deliberately not
/// implemented: persistent grants live only in the host-local trust store,
/// and the option does not exist at the schema level.
pub(crate) use crate::policy::session::{SessionChoice, SessionPolicy as ApprovalPolicy};

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
        ApprovalKind::Filesystem { operation } => operation.fingerprint(),
        ApprovalKind::TerminalCreate { request } => {
            let command = [request.command.clone()]
                .into_iter()
                .chain(request.args.clone())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            format!("terminal:{command}")
        }
        ApprovalKind::Network { route, current_host, call, .. } => {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            format!("network:{}:{action}:{current_host}", route.transport_identity())
        }
    }
}

/// Session-scoped counterpart of an approval choice; once-only choices are
/// never recorded (shared precedence contract, Phase 1 foundation), and
/// persistent grants are host-local rules, not session decisions.
fn session_decision(choice: ApprovalChoice) -> Option<SessionChoice> {
    match choice {
        ApprovalChoice::AllowOnce
        | ApprovalChoice::DenyOnce
        | ApprovalChoice::AllowPersistent
        | ApprovalChoice::AllowPersistentShort
        | ApprovalChoice::AllowPersistentPrefix(_)
        | ApprovalChoice::AllowPersistentPrefixShort(_)
        | ApprovalChoice::DenyPersistent => None,
        ApprovalChoice::AllowSession => Some(SessionChoice::Allow),
        ApprovalChoice::DenySession => Some(SessionChoice::Deny),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DenyScopePreview {
    pub(crate) workspace: String,
    pub(crate) agent: String,
    pub(crate) matcher_fields: Vec<(String, String)>,
    pub(crate) expires: String,
}

#[derive(Debug)]
struct PersistentDenyCandidate {
    rule: TrustRule,
    preview: DenyScopePreview,
}

#[derive(Debug, Clone)]
pub(crate) struct MandatoryConfirmation {
    pub(crate) rule_id: String,
    pub(crate) template_id: Option<String>,
}

/// A pending file-write or terminal-create approval.
#[derive(Debug)]
pub(crate) struct ApprovalPrompt {
    pub(crate) thread_index: Option<usize>,
    pub(crate) session_id: String,
    /// Agent id of the requesting session (rule scoping; `None` for the
    /// MCP proxy session).
    agent_id: Option<String>,
    write_lease: Option<WriteLeaseId>,
    write_lease_owner: Option<WriteLeaseOwner>,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// `(label, choice)` option list; the user picks one with Enter.
    pub(crate) options: Vec<(String, ApprovalChoice)>,
    pub(crate) selected: usize,
    kind: ApprovalKind,
    /// Phase 3: validated generic MCP invocation behind this prompt, when
    /// the request is an eligible proxy tool call.  Presence gates the
    /// persistent `Allow for 1 hour / 20 uses` option.
    mcp: Option<McpInvocation>,
    allow_candidates: Vec<(ApprovalChoice, BoundedRuleCandidate)>,
    confirming_allow: Option<ApprovalChoice>,
    deny_candidate: Option<PersistentDenyCandidate>,
    confirming_deny: bool,
    mandatory_confirmation: Option<MandatoryConfirmation>,
    pub(crate) reply: oneshot::Sender<ClientRequestResult>,
}

impl ApprovalPrompt {
    fn write(
        thread_index: Option<usize>,
        session_id: &SessionId,
        request: &WriteTextFileRequest,
        persistent_label: Option<&'static str>,
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
            None,
            persistent_label,
            reply,
        )
    }

    fn filesystem(
        operation: super::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index: None,
            session_id: SessionId::new("proxy").0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title: operation.tool_name().to_string(),
            detail: operation.detail(),
            options: approval_options(None),
            selected: 0,
            kind: ApprovalKind::Filesystem { operation },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    fn proxy_write(
        spec: ProxyWriteSpec,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self::write_with(
            None,
            &SessionId::new("proxy"),
            spec.title,
            mcp.as_ref().map(mcp_approval_detail).unwrap_or(spec.detail),
            spec.prepared,
            mcp,
            persistent_label,
            reply,
        )
    }

    /// Internal constructor shared by the write prompt builders; the
    /// argument count is inherent to the prompt shape.
    #[allow(clippy::too_many_arguments)]
    fn write_with(
        thread_index: Option<usize>,
        session_id: &SessionId,
        title: String,
        detail: String,
        prepared: PreparedWrite,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title,
            detail,
            options: approval_options(persistent_label),
            selected: 0,
            kind: ApprovalKind::Write {
                path: prepared.path,
                content: prepared.content,
                tool_call_id: prepared.tool_call_id,
                expectation: prepared.expectation,
                reply_kind: prepared.reply_kind,
                proxy_edit_count: prepared.proxy_edit_count,
            },
            mcp,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    fn proxy_write_batch(
        title: String,
        detail: String,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index: None,
            session_id: SessionId::new("proxy").0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title,
            detail: mcp.as_ref().map(mcp_approval_detail).unwrap_or(detail),
            options: approval_options(persistent_label),
            selected: 0,
            kind: ApprovalKind::WriteBatch { writes, total_edit_count },
            mcp,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn web(
        route: ProxyRoute,
        network_session_id: String,
        requested_host: String,
        current_host: String,
        provider_label: Option<&str>,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let action = match &call {
            WebApprovalCall::Search { .. } => "web search",
            WebApprovalCall::Fetch { .. } => "fetch URL",
            WebApprovalCall::BrowserRun { request } => match request.action {
                ee_mcp::BrowserRunAction::Content => "Browser Run content",
                ee_mcp::BrowserRunAction::Screenshot => "Browser Run screenshot",
                ee_mcp::BrowserRunAction::Markdown => "Browser Run markdown",
                ee_mcp::BrowserRunAction::Scrape => "Browser Run scrape",
                ee_mcp::BrowserRunAction::Json => "Browser Run JSON extraction",
                ee_mcp::BrowserRunAction::Links => "Browser Run links",
            },
        };
        Self {
            thread_index: None,
            // Network grants bind both transport and opaque connection scope.
            // A later stdio or ACP connection cannot reuse this decision.
            session_id: network_session_id,
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title: format!("network/{action}"),
            detail: match provider_label {
                Some(provider) => format!("provider: {provider} · host: {current_host}"),
                None => format!("host: {current_host}"),
            },
            options: approval_options(None),
            selected: 0,
            kind: ApprovalKind::Network {
                route,
                requested_host,
                current_host,
                call,
                approved_hosts,
                cancellation,
            },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    fn terminal(
        thread_index: Option<usize>,
        agent_id: Option<String>,
        session_id: &SessionId,
        request: &CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
        persistent_allowed: bool,
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
            agent_id,
            write_lease: None,
            write_lease_owner: None,
            title: String::from("terminal/create"),
            detail: format!("{command} · cwd: {cwd} · env: {env_text}"),
            options: approval_options(
                persistent_allowed.then_some(PERSISTENT_TERMINAL_OPTION_LABEL),
            ),
            selected: 0,
            kind: ApprovalKind::TerminalCreate { request: request.clone() },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(crate) fn allow_confirmation_preview(&self) -> Option<&BoundedRulePreview> {
        let choice = self.confirming_allow?;
        self.allow_candidates.iter().find_map(|(candidate_choice, candidate)| {
            (*candidate_choice == choice).then_some(&candidate.preview)
        })
    }

    pub(crate) fn deny_confirmation_preview(&self) -> Option<&DenyScopePreview> {
        self.confirming_deny
            .then(|| self.deny_candidate.as_ref().map(|candidate| &candidate.preview))
            .flatten()
    }

    pub(crate) fn is_confirming_rule(&self) -> bool {
        self.confirming_deny || self.confirming_allow.is_some()
    }

    pub(crate) fn confirming_allow_choice(&self) -> Option<ApprovalChoice> {
        self.confirming_allow
    }

    pub(crate) fn mandatory_confirmation(&self) -> Option<&MandatoryConfirmation> {
        self.mandatory_confirmation.as_ref()
    }
}

/// The approval option list.  Allow-always (unlimited persistence) is
/// intentionally absent; the bounded persistent option exists only for
/// eligible terminal requests (Phase 2 command trust), eligible generic MCP
/// invocations (Phase 3), and eligible bounded native writes (Phase 5).
fn approval_options(persistent_label: Option<&'static str>) -> Vec<(String, ApprovalChoice)> {
    let mut options = [
        ApprovalChoice::AllowOnce,
        ApprovalChoice::AllowSession,
        ApprovalChoice::DenyOnce,
        ApprovalChoice::DenySession,
    ]
    .into_iter()
    .map(|choice| (choice.label().to_string(), choice))
    .collect::<Vec<_>>();
    if let Some(label) = persistent_label {
        options.push((label.to_string(), ApprovalChoice::AllowPersistent));
    }
    options
}

/// Redacted MCP approval text: server, tool, side-effect class, and bounded
/// canonical arguments only (Phase 3); never renders secrets, environment
/// values, or file contents.
fn mcp_approval_detail(invocation: &McpInvocation) -> String {
    format!(
        "server: {} · tool: {} · class: {} · args: {}",
        invocation.server,
        invocation.tool,
        invocation.category.as_str(),
        redact_arguments_display(&invocation.arguments_json),
    )
}

/// Bounded argument display; oversized canonical payloads are truncated.
fn redact_arguments_display(arguments: &str) -> String {
    const MAX_DISPLAY_BYTES: usize = 200;
    if arguments.len() <= MAX_DISPLAY_BYTES {
        arguments.to_string()
    } else {
        format!("{}…", &arguments[..MAX_DISPLAY_BYTES])
    }
}

// ── Action log ───────────────────────────────────────────────────────────────

/// Redacted lifecycle metadata about a matched persistent grant (Phase 6).
struct TrustGrantStatus {
    remaining_uses: Option<u64>,
    expires_at: Option<SystemTime>,
}

/// UTC RFC3339 display for a grant expiry; no paths or secrets.
fn format_expiry_utc(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// One recorded agent file operation (future checkpoint/restore source) or
/// redacted automatic trust decision (Phase 6 audit).
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
    /// Redacted automatic trust decision: rule id, operation category,
    /// machine-readable reason, and remaining use budget.  Never carries
    /// raw paths, command environment, secret values, or MCP arguments.
    TrustDecision {
        rule_id: Option<String>,
        category: TrustCategory,
        reason: DecisionReason,
        remaining_uses: Option<u64>,
        session_id: String,
    },
    /// Redacted durable trust-rule lifecycle event, separate from decisions.
    TrustRuleMutation {
        rule_id: Option<String>,
        action: String,
        source: String,
    },
    /// External provenance only. Retains final canonical source URL, never a
    /// separate request body, response text, headers, credentials, or search query.
    ExternalSource {
        action: String,
        host: String,
        url: String,
        retrieved_at: String,
        sha256: Option<String>,
        byte_count: usize,
        result_count: usize,
        cached: bool,
        truncated: bool,
        provenance: String,
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
        self.prune_cancelled_bridge_approvals();
        while let Ok(message) = self.agents.bridge_rx.try_recv() {
            match message {
                BridgeUiMessage::ReadFile { request, reply } => {
                    // Phase 4: normalize + evaluate before serving; reads
                    // stay prompt-free, but protected/external reads can
                    // never match a persistent rule.
                    let _ = self.native_read_decision(&request.path, request.limit.map(u64::from));
                    self.bridge_read_file(&request, reply);
                }
                BridgeUiMessage::WriteFile { request, reply } => {
                    if let Err(error) = self.validate_workspace_write_path(&request.path) {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    let thread = self.session_thread(&request.session_id);
                    let persistent_label = self.native_write_persistent_label(
                        &request.path,
                        &request.content,
                        &WriteExpectation::Blind,
                    );
                    self.request_bridge_approval(ApprovalPrompt::write(
                        thread,
                        &request.session_id,
                        &request,
                        persistent_label,
                        reply,
                    ));
                }
                BridgeUiMessage::TerminalCreate { request, reply } => {
                    let thread = self.session_thread(&request.session_id);
                    let agent_id = thread
                        .and_then(|index| self.agents.threads.get(index))
                        .map(|thread| thread.agent_id.clone());
                    // Normalize after request validation and before approval
                    // queue insertion: only validated invocations may offer
                    // persistent command trust (Phase 2).
                    let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
                    self.request_bridge_approval(ApprovalPrompt::terminal(
                        thread,
                        agent_id,
                        &request.session_id,
                        &request,
                        reply,
                        persistent_allowed,
                    ));
                }
                BridgeUiMessage::Elicitation { session_id, request, reply } => {
                    self.present_elicitation(session_id, request, reply);
                }
                BridgeUiMessage::ProxyTool { call, route, reply } => {
                    self.handle_proxy_tool(call, route, reply);
                }
                BridgeUiMessage::ProxyConnectionClosed { scope } => {
                    self.clear_proxy_network_scope(&scope);
                }
                BridgeUiMessage::TerminalCompleted { completion } => {
                    self.record_terminal_validation(completion);
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
        route: super::agents_mcp::ProxyRoute,
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
            super::agents_mcp::ProxyToolCall::WebSearch { query, approval_scope, cancellation } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::Search { query },
                    cancellation,
                    reply,
                );
            }
            super::agents_mcp::ProxyToolCall::FetchUrl { url, approval_scope, cancellation } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::Fetch { url },
                    cancellation,
                    reply,
                );
            }
            super::agents_mcp::ProxyToolCall::BrowserRun {
                request,
                approval_scope,
                cancellation,
            } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::BrowserRun { request },
                    cancellation,
                    reply,
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
            super::agents_mcp::ProxyToolCall::CreateDirectory { path } => {
                self.queue_proxy_filesystem(
                    super::agent_filesystem::FilesystemOperation::CreateDirectory {
                        path: PathBuf::from(path),
                    },
                    reply,
                );
            }
            super::agents_mcp::ProxyToolCall::DeletePath { path } => {
                self.queue_proxy_filesystem(
                    super::agent_filesystem::FilesystemOperation::DeletePath {
                        path: PathBuf::from(path),
                    },
                    reply,
                );
            }
            super::agents_mcp::ProxyToolCall::CopyPath { source_path, destination_path } => {
                self.queue_proxy_filesystem(
                    super::agent_filesystem::FilesystemOperation::CopyPath {
                        source: PathBuf::from(source_path),
                        destination: PathBuf::from(destination_path),
                    },
                    reply,
                );
            }
            super::agents_mcp::ProxyToolCall::MovePath { source_path, destination_path } => {
                self.queue_proxy_filesystem(
                    super::agent_filesystem::FilesystemOperation::MovePath {
                        source: PathBuf::from(source_path),
                        destination: PathBuf::from(destination_path),
                    },
                    reply,
                );
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
                self.queue_proxy_apply_code_action(&path, &action_id, route, reply);
            }
            super::agents_mcp::ProxyToolCall::FormatFile { path } => {
                self.queue_proxy_format_file(&path, route, reply);
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
                self.queue_proxy_rename_symbol(&path, line, character, &new_name, route, reply);
            }
            super::agents_mcp::ProxyToolCall::GitStatus => {
                let _ = reply.send(self.proxy_git_status().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::GitDiff => {
                let _ = reply.send(self.proxy_git_diff().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::GitDiffStaged => {
                let _ =
                    reply.send(self.proxy_git_diff_staged().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::GitDiffFile { path } => {
                let _ = reply.send(
                    self.proxy_git_diff_file(Path::new(&path))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            super::agents_mcp::ProxyToolCall::ChangedFiles => {
                let _ =
                    reply.send(self.proxy_changed_files().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::ReviewContext => {
                let _ =
                    reply.send(self.proxy_review_context().map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::ProjectInstructions => {
                let result = self
                    .active_root_path()
                    .ok_or_else(|| AgentError::invalid_params("no active workspace root"))
                    .and_then(|root| super::agent_knowledge::project_instructions(&root))
                    .and_then(|result| {
                        serde_json::to_value(result)
                            .map_err(|error| AgentError::HandlerError(error.to_string()))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::SaveNote { scope, key, content } => {
                let result =
                    self.project_knowledge.save_note(&scope, &key, &content).and_then(|result| {
                        serde_json::to_value(result)
                            .map_err(|error| AgentError::HandlerError(error.to_string()))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::ReadNotes { scope } => {
                let result = self.project_knowledge.read_notes(&scope).and_then(|result| {
                    serde_json::to_value(result)
                        .map_err(|error| AgentError::HandlerError(error.to_string()))
                });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::ReadNote { scope, key } => {
                let result = self.project_knowledge.read_note(&scope, &key).and_then(|result| {
                    serde_json::to_value(result)
                        .map_err(|error| AgentError::HandlerError(error.to_string()))
                });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::FileDependencyMap { path } => {
                let result = self.proxy_file_dependency_map(Path::new(&path));
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::SymbolDependencyMap { path, line, character } => {
                let result = self.proxy_symbol_dependency_map(Path::new(&path), line, character);
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            super::agents_mcp::ProxyToolCall::Read(request) => {
                // Existing read-only MCP behavior remains prompt-free until
                // this workspace explicitly enables the broad safe-read
                // profile. Once present, require its exact route/tool/schema
                // authorization before serving bytes.
                let decision = self.mcp_read_decision(&request, route);
                if self.mcp_read_profile_enforced() && decision.outcome != TrustOutcome::Allow {
                    let _ = reply.send(Err(AgentError::PermissionDenied {
                        reason: "MCP safe-read profile does not authorize this request".to_string(),
                    }));
                    return;
                }
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
                    self.native_write_persistent_label(
                        &request.path,
                        &request.content,
                        &WriteExpectation::Blind,
                    ),
                    reply,
                ));
            }
            super::agents_mcp::ProxyToolCall::Terminal(request) => {
                let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
                self.request_bridge_approval(ApprovalPrompt::terminal(
                    None,
                    None,
                    &session_id,
                    &request,
                    reply,
                    persistent_allowed,
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

    /// Queues or dispatches one network tool call. Trusted global hosts bypass
    /// UI; all other hosts require an isolated route-and-host session decision.
    fn queue_web_approval(
        &mut self,
        route: ProxyRoute,
        approval_scope: String,
        call: WebApprovalCall,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let (host, provider_label) = {
            let service = match self.web_context_service() {
                Ok(service) => service,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            match &call {
                WebApprovalCall::Search { .. } => (
                    service.search_initial_host(),
                    Some(service.search_provider_approval_label().to_owned()),
                ),
                WebApprovalCall::Fetch { url } => (
                    service
                        .fetch_initial_host(&ee_agent_host::WebFetchRequest { url: url.clone() }),
                    None,
                ),
                WebApprovalCall::BrowserRun { request } => (
                    service.browser_run_initial_host(request),
                    Some(String::from("Cloudflare Browser Run")),
                ),
            }
        };
        let host = match host {
            Ok(host) => host,
            Err(error) => {
                let _ = reply.send(Err(web_context_agent_error(error)));
                return;
            }
        };
        let preapproved =
            self.web_context_service().is_ok_and(|service| service.is_preapproved_host(&host));
        let network_session_id =
            format!("proxy-network:{}:{approval_scope}", route.transport_identity());
        if preapproved {
            self.dispatch_web_call(
                route,
                network_session_id,
                host,
                call,
                BTreeSet::new(),
                cancellation,
                reply,
            );
        } else {
            self.request_web_approval(ApprovalPrompt::web(
                route,
                network_session_id,
                host.clone(),
                host,
                provider_label.as_deref(),
                call,
                BTreeSet::new(),
                cancellation,
                reply,
            ));
        }
    }

    fn attach_bounded_allows(&self, prompt: &mut ApprovalPrompt, operation: &TrustOperation) {
        prompt.options.retain(|(_, choice)| {
            !matches!(
                choice,
                ApprovalChoice::AllowPersistent
                    | ApprovalChoice::AllowPersistentShort
                    | ApprovalChoice::AllowPersistentPrefix(_)
                    | ApprovalChoice::AllowPersistentPrefixShort(_)
            )
        });
        prompt.allow_candidates.clear();
        let now = self.trust_clock.now();
        let agent = prompt.agent_id.as_deref();
        let mut candidates = Vec::new();
        match &prompt.kind {
            ApprovalKind::TerminalCreate { request } => {
                let Ok(invocation) = self.command_invocation_for_request(request) else {
                    return;
                };
                if let Ok(candidate) = BoundedRuleCandidate::command_exact(&invocation, agent, now)
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistent,
                        PERSISTENT_TERMINAL_OPTION_LABEL.to_string(),
                        candidate,
                    ));
                }
                if let Ok(candidate) =
                    BoundedRuleCandidate::command_exact_short(&invocation, agent, now)
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistentShort,
                        "Allow for 10 minutes / 5 uses".to_string(),
                        candidate,
                    ));
                }
                // Offer two deliberate token boundaries at most: first argument
                // and full argv. This keeps scope selection explicit without
                // allowing long requests to hide approval controls.
                if !invocation.argv.is_empty() {
                    for argument_count in [1, invocation.argv.len()] {
                        if argument_count == invocation.argv.len()
                            && argument_count == 1
                            && candidates.iter().any(|(choice, _, _)| {
                                *choice == ApprovalChoice::AllowPersistentPrefix(1)
                            })
                        {
                            continue;
                        }
                        if let Ok(candidate) = BoundedRuleCandidate::command_prefix(
                            &invocation,
                            agent,
                            argument_count,
                            now,
                        ) {
                            candidates.push((
                                ApprovalChoice::AllowPersistentPrefix(argument_count),
                                format!(
                                    "Allow prefix through argument {argument_count} for 1 hour / 20 uses"
                                ),
                                candidate,
                            ));
                        }
                        if let Ok(candidate) = BoundedRuleCandidate::command_prefix_short(
                            &invocation,
                            agent,
                            argument_count,
                            now,
                        ) {
                            candidates.push((
                                ApprovalChoice::AllowPersistentPrefixShort(argument_count),
                                format!(
                                    "Allow prefix through argument {argument_count} for 10 minutes / 5 uses"
                                ),
                                candidate,
                            ));
                        }
                    }
                }
            }
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. }
                if prompt.mcp.is_some() =>
            {
                if let Some(invocation) = prompt.mcp.as_ref()
                    && let Ok(candidate) = BoundedRuleCandidate::mcp_exact(invocation, agent, now)
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistent,
                        PERSISTENT_TERMINAL_OPTION_LABEL.to_string(),
                        candidate,
                    ));
                }
                if let Some(invocation) = prompt.mcp.as_ref()
                    && let Ok(candidate) =
                        BoundedRuleCandidate::mcp_exact_short(invocation, agent, now)
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistentShort,
                        "Allow for 10 minutes / 5 uses".to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Write { path, content, expectation, .. } => {
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_single_write_rule_shape(path, content, expectation)
                    && let Ok(candidate) = BoundedRuleCandidate::write_prefix(
                        operation.workspace,
                        agent,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistent,
                        PERSISTENT_WRITE_OPTION_LABEL.to_string(),
                        candidate,
                    ));
                }
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_single_write_rule_shape(path, content, expectation)
                    && let Ok(candidate) = BoundedRuleCandidate::write_prefix_short(
                        operation.workspace,
                        agent,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistentShort,
                        "Allow for 10 minutes / 1 use".to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::WriteBatch { writes, .. } => {
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_batch_write_rule_shape(writes)
                    && let Ok(candidate) = BoundedRuleCandidate::write_prefix(
                        operation.workspace,
                        agent,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistent,
                        PERSISTENT_WRITE_OPTION_LABEL.to_string(),
                        candidate,
                    ));
                }
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_batch_write_rule_shape(writes)
                    && let Ok(candidate) = BoundedRuleCandidate::write_prefix_short(
                        operation.workspace,
                        agent,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistentShort,
                        "Allow for 10 minutes / 1 use".to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Network { .. } => {
                if let OperationIdentity::Network { scheme, host, port, method, browser_action } =
                    &operation.identity
                    && let Ok(candidate) = BoundedRuleCandidate::network_exact_read(
                        operation.workspace,
                        agent,
                        *scheme,
                        host.clone(),
                        *port,
                        *method,
                        *browser_action,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistent,
                        "Allow exact host for 1 hour / 20 uses".to_string(),
                        candidate,
                    ));
                }
                if let OperationIdentity::Network { scheme, host, port, method, browser_action } =
                    &operation.identity
                    && let Ok(candidate) = BoundedRuleCandidate::network_exact_read_short(
                        operation.workspace,
                        agent,
                        *scheme,
                        host.clone(),
                        *port,
                        *method,
                        *browser_action,
                        now,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowPersistentShort,
                        "Allow exact host for 10 minutes / 5 uses".to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Filesystem { .. } => {}
        }
        for (choice, label, candidate) in candidates {
            prompt.options.push((label, choice));
            prompt.allow_candidates.push((choice, candidate));
        }
    }

    fn attach_persistent_deny(&self, prompt: &mut ApprovalPrompt, operation: &TrustOperation) {
        let Some(candidate) = self.persistent_deny_candidate(prompt, operation) else {
            return;
        };
        prompt.options.push((
            ApprovalChoice::DenyPersistent.label().to_string(),
            ApprovalChoice::DenyPersistent,
        ));
        prompt.deny_candidate = Some(candidate);
    }

    fn persistent_deny_candidate(
        &self,
        prompt: &ApprovalPrompt,
        operation: &TrustOperation,
    ) -> Option<PersistentDenyCandidate> {
        let scope = TrustRuleScope {
            workspace: operation.workspace,
            agent: prompt.agent_id.clone(),
            expires_at: None,
            max_uses: None,
        };
        let (rule, matcher_fields) = match &operation.identity {
            OperationIdentity::Command { executable, argv } => {
                let rule = TrustRule::Command(CommandRule {
                    id: generate_command_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    executable: executable.clone(),
                    match_mode: MatchMode::ArgvExact,
                    argv: argv.clone(),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "command".into()),
                        ("executable".into(), executable.clone()),
                        ("arguments".into(), format!("exact · {} tokens", argv.len())),
                    ],
                )
            }
            OperationIdentity::Mcp {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            }
            | OperationIdentity::McpRead {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            } => {
                let rule = TrustRule::mcp_deny(McpDenyRule {
                    id: generate_mcp_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    server: server.clone(),
                    transport_identity: transport_identity.clone(),
                    tool: tool.clone(),
                    tool_schema_version: *tool_schema_version,
                    category: Some(operation.category),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "mcp".into()),
                        ("server".into(), server.clone()),
                        ("transport".into(), transport_identity.clone()),
                        ("tool".into(), tool.clone()),
                        ("schema".into(), tool_schema_version.to_string()),
                        ("category".into(), operation.category.as_str().into()),
                    ],
                )
            }
            OperationIdentity::Write { relative_path, .. } => {
                let operation_kind = match operation.category {
                    TrustCategory::WriteCreate => WriteOperationKind::Create,
                    TrustCategory::WriteModify => WriteOperationKind::Modify,
                    _ => return None,
                };
                let rule = TrustRule::Write(WriteRule {
                    id: generate_write_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    operation: operation_kind,
                    path_prefix: PathPrefix::parse(relative_path).ok()?,
                    max_files: 0,
                    max_total_bytes: 0,
                    max_file_bytes: 0,
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "filesystem write".into()),
                        ("operation".into(), format!("{operation_kind:?}").to_ascii_lowercase()),
                        ("path prefix".into(), relative_path.clone()),
                    ],
                )
            }
            OperationIdentity::Filesystem {
                operation: filesystem_operation,
                source_path,
                destination_path,
            } => {
                let path = source_path.as_ref().or(destination_path.as_ref())?;
                let rule = TrustRule::filesystem(FilesystemRule {
                    id: generate_filesystem_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    operations: vec![*filesystem_operation],
                    path_prefix: PathPrefix::parse(path).ok()?,
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "filesystem".into()),
                        (
                            "operation".into(),
                            format!("{filesystem_operation:?}").to_ascii_lowercase(),
                        ),
                        ("path prefix".into(), path.clone()),
                    ],
                )
            }
            OperationIdentity::Network { scheme, host, port, method, browser_action } => {
                let rule = TrustRule::Network(
                    NetworkRule::deny(
                        generate_network_rule_id(),
                        scope,
                        *scheme,
                        host.clone(),
                        HostMatchMode::Exact,
                        *port,
                        *method,
                        *browser_action,
                    )
                    .ok()?,
                );
                (
                    rule,
                    vec![
                        ("kind".into(), "network".into()),
                        ("scheme".into(), format!("{scheme:?}").to_ascii_lowercase()),
                        ("host".into(), host.clone()),
                        ("port".into(), port.to_string()),
                        ("method class".into(), format!("{method:?}").to_ascii_lowercase()),
                        (
                            "browser action".into(),
                            format!("{browser_action:?}").to_ascii_lowercase(),
                        ),
                    ],
                )
            }
            OperationIdentity::NativeTool { tool } => {
                let rule = TrustRule::tool(ToolRule {
                    id: generate_tool_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    identity: ToolRuleIdentity::Native { tool: tool.clone() },
                    category: Some(operation.category),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "native tool/category".into()),
                        ("tool".into(), tool.clone()),
                        ("category".into(), operation.category.as_str().into()),
                    ],
                )
            }
            _ => return None,
        };
        Some(PersistentDenyCandidate {
            rule,
            preview: DenyScopePreview {
                workspace: operation.workspace.as_string(),
                agent: prompt.agent_id.clone().unwrap_or_else(|| "all agents".into()),
                matcher_fields,
                expires: "never".into(),
            },
        })
    }

    /// Network approvals use typed persistent deny and session policy, but
    /// never persistent allow or approval-mode bypass.
    fn request_web_approval(&mut self, mut prompt: ApprovalPrompt) {
        let fingerprint = approval_fingerprint(&prompt.kind);
        let operation = self.trust_operation_for_prompt(&prompt);
        self.attach_bounded_allows(&mut prompt, &operation);
        self.attach_persistent_deny(&mut prompt, &operation);
        let decision = self.evaluate_operation(&operation, &prompt.session_id, &fingerprint);
        self.mark_mandatory_confirmation(&mut prompt, &operation, &decision);
        self.push_trust_audit(&operation, &decision, &prompt.session_id);
        match &decision {
            TrustDecision {
                outcome: TrustOutcome::Allow,
                reason: DecisionReason::PersistentAllow,
                rule_id: Some(rule_id),
            } => {
                self.resolve_persistent_allow(prompt, rule_id.clone());
                return;
            }
            TrustDecision { outcome: TrustOutcome::Allow, .. } => {
                self.resolve_approval(prompt, ApprovalChoice::AllowSession);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Deny, .. } => {
                self.resolve_policy_deny(prompt, &decision);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Confirm, .. } => {}
        }
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "approval required");
        }
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(String::from("network approval required"));
    }

    fn clear_proxy_network_scope(&mut self, scope: &str) {
        let prefix = format!("proxy-network:{}:{scope}", ProxyRoute::Stdio.transport_identity());
        self.agents.approval_policy.invalidate_session(&prefix);
        // Dropping each sender makes any caller resolve as cancelled. Sending
        // from `retain` would require moving out of a borrowed prompt.
        self.agents.approvals.retain(|prompt| prompt.session_id != prefix);
    }

    fn record_web_failure(&mut self, action: &str, host: &str, status: &str) {
        let lifecycle_id = NEXT_WEB_LIFECYCLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.record_web_lifecycle(
            &format!("web-{lifecycle_id}"),
            &format!("web/{action}"),
            status,
            &format!(
                "kind: fetch · host: {host} · outcome: {status} · trust: untrusted external content"
            ),
        );
    }

    pub(crate) fn prune_cancelled_bridge_approvals(&mut self) {
        let mut retained = VecDeque::with_capacity(self.agents.approvals.len());
        while let Some(mut prompt) = self.agents.approvals.pop_front() {
            if prompt.reply.is_closed() {
                self.release_prompt_write_lease(&mut prompt);
            } else {
                retained.push_back(prompt);
            }
        }
        self.agents.approvals = retained;
    }

    fn record_write_lease_rejection(&self, prompt: &ApprovalPrompt) {
        let paths = match &prompt.kind {
            ApprovalKind::Write { path, .. } => vec![path.clone()],
            ApprovalKind::WriteBatch { writes, .. } => {
                writes.iter().map(|write| write.path.clone()).collect()
            }
            ApprovalKind::Filesystem { .. }
            | ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::Network { .. } => return,
        };
        let revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            &prompt.session_id,
            TurnObservation::Revision { revision: revision.clone() },
        );
        self.observe_active_turn(
            &prompt.session_id,
            TurnObservation::Write {
                revision: revision.clone(),
                outcome: WriteEvidenceOutcome::Conflicted,
            },
        );
        self.observe_transaction_stage(
            &prompt.session_id,
            revision,
            WriteTransactionStage::Read,
            EvidenceCheck::Failed,
        );
    }

    fn acquire_prompt_write_lease(
        &mut self,
        prompt: &mut ApprovalPrompt,
    ) -> Result<(), AgentError> {
        let scopes = match &prompt.kind {
            ApprovalKind::Write { path, .. } => {
                vec![self.canonical_workspace_write_target(path).ok_or_else(|| {
                    AgentError::invalid_params("write target has no canonical workspace identity")
                })?]
            }
            ApprovalKind::WriteBatch { writes, .. } => writes
                .iter()
                .map(|write| {
                    self.canonical_workspace_write_target(&write.path).ok_or_else(|| {
                        AgentError::invalid_params(
                            "write target has no canonical workspace identity",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            ApprovalKind::Filesystem { operation } => operation
                .canonical_write_scopes(&self.allowed_fs_roots())
                .map_err(|error| AgentError::invalid_params(error.to_string()))?,
            ApprovalKind::TerminalCreate { .. } | ApprovalKind::Network { .. } => return Ok(()),
        };

        let blocks_dirty = match &prompt.kind {
            ApprovalKind::Write { expectation, .. } => {
                matches!(expectation, WriteExpectation::Blind | WriteExpectation::MustNotExist)
            }
            ApprovalKind::WriteBatch { writes, .. } => writes.iter().any(|write| {
                matches!(
                    write.expectation,
                    WriteExpectation::Blind | WriteExpectation::MustNotExist
                )
            }),
            ApprovalKind::Filesystem { .. } => true,
            ApprovalKind::TerminalCreate { .. } | ApprovalKind::Network { .. } => false,
        } && self.has_dirty_buffer(&scopes);
        if blocks_dirty {
            return Err(AgentError::invalid_params(
                "dirty editor buffer conflicts with requested agent write scope",
            ));
        }

        if prompt.agent_id.is_none() {
            prompt.agent_id = prompt
                .thread_index
                .and_then(|index| self.agents.threads.get(index))
                .map(|thread| thread.agent_id.clone());
        }
        let connection_id = prompt.agent_id.clone().unwrap_or_else(|| String::from("proxy"));
        let turn_id = match &prompt.kind {
            ApprovalKind::Write { tool_call_id: Some(id), .. } => id.clone(),
            ApprovalKind::Write { tool_call_id: None, .. } => {
                format!("write-{}", self.agents.next_write_turn_id)
            }
            ApprovalKind::WriteBatch { writes, .. } => writes
                .iter()
                .find_map(|write| write.tool_call_id.clone())
                .unwrap_or_else(|| format!("write-{}", self.agents.next_write_turn_id)),
            ApprovalKind::Filesystem { .. } => {
                format!("filesystem-{}", self.agents.next_write_turn_id)
            }
            ApprovalKind::TerminalCreate { .. } | ApprovalKind::Network { .. } => unreachable!(),
        };
        self.agents.next_write_turn_id = self.agents.next_write_turn_id.wrapping_add(1);
        let owner =
            WriteLeaseOwner { connection_id, session_id: prompt.session_id.clone(), turn_id };
        let revisions = self.write_scope_revisions(&scopes)?;
        let id =
            self.agents.write_leases.acquire(owner.clone(), scopes, revisions).map_err(
                |conflict| AgentError::PermissionDenied { reason: conflict.to_string() },
            )?;
        prompt.write_lease = Some(id);
        prompt.write_lease_owner = Some(owner);
        Ok(())
    }

    fn write_scope_revisions(
        &self,
        scopes: &[PathBuf],
    ) -> Result<BTreeMap<PathBuf, String>, AgentError> {
        scopes.iter().map(|path| Ok((path.clone(), self.write_scope_revision(path)?))).collect()
    }

    fn write_scope_revision(&self, path: &Path) -> Result<String, AgentError> {
        let dirty = self.backend.all_bufs().iter().any(|buffer| {
            !buffer.pristine
                && buffer.path.as_deref().is_some_and(|candidate| paths_equivalent(candidate, path))
        });
        if path.is_file() {
            let revision =
                self.current_text_revision(path)?.unwrap_or_else(|| String::from("missing"));
            return Ok(format!("file:{revision}:dirty={dirty}"));
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(String::from("missing"));
            }
            Err(error) => {
                return Err(AgentError::Io(format!(
                    "cannot inspect write scope {}: {error}",
                    path.display()
                )));
            }
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Ok(format!(
            "metadata:{}:{}:{}:{modified}:dirty={dirty}",
            metadata.is_dir(),
            metadata.is_file(),
            metadata.len()
        ))
    }

    fn validate_prompt_write_lease(&self, prompt: &ApprovalPrompt) -> Result<(), AgentError> {
        let Some(id) = prompt.write_lease else {
            return Ok(());
        };
        let owner = prompt.write_lease_owner.as_ref().ok_or_else(|| {
            AgentError::PermissionDenied { reason: String::from("write lease owner is missing") }
        })?;
        let scopes = self.agents.write_leases.scopes(id).ok_or_else(|| {
            AgentError::PermissionDenied { reason: String::from("write lease is no longer active") }
        })?;
        let revisions = self.write_scope_revisions(scopes)?;
        self.agents
            .write_leases
            .validate(id, owner, &revisions)
            .map_err(|reason| AgentError::PermissionDenied { reason: reason.to_string() })
    }

    fn release_prompt_write_lease(&mut self, prompt: &mut ApprovalPrompt) {
        if let Some(id) = prompt.write_lease.take() {
            self.agents.write_leases.release(id);
        }
        prompt.write_lease_owner = None;
    }

    /// Queues an approval prompt (front of the queue wins) and notifies,
    /// unless the shared policy (session state first, then persistent
    /// rules) already resolves it without UI.
    fn request_bridge_approval(&mut self, mut prompt: ApprovalPrompt) {
        let thread_index = prompt.thread_index;
        let session_id = prompt.session_id.clone();
        let fingerprint = approval_fingerprint(&prompt.kind);
        let operation = self.trust_operation_for_prompt(&prompt);
        let safeguard = self.built_in_safeguard_for_prompt(&prompt);
        self.attach_bounded_allows(&mut prompt, &operation);
        self.attach_persistent_deny(&mut prompt, &operation);
        let mut decision = self.evaluate_operation_with_safeguard(
            &operation,
            &session_id,
            &fingerprint,
            safeguard,
        );
        // Phase 4 curated-profile fallback: a terminal request that matches
        // a fixed registry entry is evaluated as its profile when the exact
        // command grant did not cover it.  The narrower exact grant always
        // wins; the profile grant fills the gap.
        let mut audited_operation = operation.clone();
        if matches!(decision.outcome, TrustOutcome::Confirm)
            && matches!(
                decision.reason,
                DecisionReason::NoMatchingRule
                    | DecisionReason::WorkspaceDisabled
                    | DecisionReason::ToolDefaultConfirm
                    | DecisionReason::CategoryDefaultConfirm
                    | DecisionReason::GlobalDefaultConfirm
            )
            && let ApprovalKind::TerminalCreate { request } = &prompt.kind
            && let Some(profile) = self.profile_id_for_request(request)
        {
            let profile_operation = TrustOperation {
                workspace: audited_operation.workspace,
                agent: audited_operation.agent.clone(),
                transport: audited_operation.transport,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Profile { profile: profile.to_string() },
            };
            decision = self.evaluate_operation(&profile_operation, &session_id, &fingerprint);
            audited_operation = profile_operation;
        }
        self.mark_mandatory_confirmation(&mut prompt, &audited_operation, &decision);
        // Phase 6 audit: every automatic decision (allow or prompt fallback)
        // records redacted rule/category/reason/remaining-use metadata.
        self.push_trust_audit(&audited_operation, &decision, &session_id);
        if matches!(decision.outcome, TrustOutcome::Deny) {
            self.resolve_policy_deny(prompt, &decision);
            return;
        }
        if let Err(error) = self.acquire_prompt_write_lease(&mut prompt) {
            self.record_write_lease_rejection(&prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        match &decision {
            TrustDecision {
                outcome: TrustOutcome::Allow,
                reason: DecisionReason::PersistentAllow,
                rule_id: Some(rule_id),
            } => {
                // Persistent rules auto-dispatch terminal creates (phases 2
                // and 4 profiles), eligible generic MCP invocations (phase
                // 3), and bounded native writes (phase 5).  Operations
                // without a matched rule stay on the UI path.
                self.resolve_persistent_allow(prompt, rule_id.clone());
                // Redacted grant metadata on the status surfaces once the
                // dispatch settled: the remaining-use count then reflects
                // the successful use, and async save alerts cannot clobber
                // the transcript notice.
                if let Some(status) =
                    self.matched_grant_status(&audited_operation, &decision, &session_id)
                {
                    let summary = match (status.remaining_uses, status.expires_at) {
                        (Some(remaining), Some(expires)) => format!(
                            "trusted by {rule_id} · {remaining} uses left · expires {}",
                            format_expiry_utc(expires)
                        ),
                        (Some(remaining), None) => {
                            format!("trusted by {rule_id} · {remaining} uses left")
                        }
                        _ => format!("trusted by {rule_id}"),
                    };
                    if let Some(thread_index) = thread_index
                        && let Some(thread) = self.agents.threads.get_mut(thread_index)
                    {
                        thread.push_system(summary.clone());
                    }
                    self.backend.status_message = Some(summary);
                }
                return;
            }
            TrustDecision { outcome: TrustOutcome::Allow, .. } => {
                // Session allow: resolve silently, no UI.
                self.resolve_approval(prompt, ApprovalChoice::AllowSession);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Deny, .. } => unreachable!(),
            _ => {}
        }
        let approval_mode =
            self.agents.approval_modes.get(&session_id).copied().unwrap_or_default();
        if decision.reason != DecisionReason::MandatoryConfirm
            && self.tool_approval_mode_allows(approval_mode, &prompt, &operation)
        {
            let summary = format!("tool auto-approved ({})", approval_mode.label());
            if let Some(thread_index) = thread_index
                && let Some(thread) = self.agents.threads.get_mut(thread_index)
            {
                thread.push_system(summary.clone());
            }
            self.backend.status_message = Some(summary);
            self.resolve_approval(prompt, ApprovalChoice::AllowOnce);
            return;
        }
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(if self.agents.layout == AgentPaneLayout::Closed {
            String::from("agent approval required (open :agents)")
        } else {
            String::from("agent approval required")
        });
    }

    fn mark_mandatory_confirmation(
        &self,
        prompt: &mut ApprovalPrompt,
        operation: &TrustOperation,
        decision: &TrustDecision,
    ) {
        let TrustDecision {
            outcome: TrustOutcome::Confirm,
            reason: DecisionReason::MandatoryConfirm,
            rule_id: Some(rule_id),
        } = decision
        else {
            return;
        };
        let template_id = self
            .effective_trust_document(operation.workspace)
            .rules
            .iter()
            .find(|rule| rule.id() == rule_id)
            .and_then(TrustRule::template_id)
            .map(str::to_string);
        prompt.mandatory_confirmation =
            Some(MandatoryConfirmation { rule_id: rule_id.clone(), template_id });
        prompt.options.retain(|(_, choice)| {
            !matches!(
                choice,
                ApprovalChoice::AllowSession
                    | ApprovalChoice::AllowPersistent
                    | ApprovalChoice::AllowPersistentShort
                    | ApprovalChoice::AllowPersistentPrefix(_)
                    | ApprovalChoice::AllowPersistentPrefixShort(_)
            )
        });
        prompt.allow_candidates.clear();
        prompt.selected = prompt.selected.min(prompt.options.len().saturating_sub(1));
    }

    /// Whether a pending bridge operation is eligible for the active local
    /// approval mode. Invalid or unnormalizable operations always stay on the
    /// explicit approval path.
    fn tool_approval_mode_allows(
        &self,
        mode: ToolApprovalMode,
        prompt: &ApprovalPrompt,
        operation: &TrustOperation,
    ) -> bool {
        if operation.is_unknown() {
            return false;
        }
        if matches!(prompt.kind, ApprovalKind::Network { .. }) {
            return false;
        }
        if let ApprovalKind::TerminalCreate { request } = &prompt.kind
            && self.command_invocation_for_request(request).is_err()
        {
            return false;
        }
        match mode {
            ToolApprovalMode::Default => false,
            ToolApprovalMode::Autopilot => match &prompt.kind {
                ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                    prompt.mcp.is_none()
                        && matches!(
                            operation.category,
                            TrustCategory::WriteCreate | TrustCategory::WriteModify
                        )
                }
                ApprovalKind::TerminalCreate { request } => {
                    self.profile_id_for_request(request).is_some()
                }
                ApprovalKind::Filesystem { .. } | ApprovalKind::Network { .. } => false,
            },
            ToolApprovalMode::Bypass => true,
        }
    }

    /// Normalizes one pending approval into the shared policy operation
    /// (Phase 1 foundation).  Session lookups still key on the legacy
    /// fingerprint; the normalized operation is what persistent rules match.
    /// Terminal requests carry a validated command invocation; invalid
    /// requests (shell wrappers, bad cwd) normalize to `Unknown` and can
    /// never match a persistent rule.
    fn trust_operation_for_prompt(&self, prompt: &ApprovalPrompt) -> TrustOperation {
        let workspace = self.primary_workspace_identity();
        let (category, identity) = match &prompt.kind {
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                // Phase 3: eligible proxy tool calls carry a validated MCP
                // invocation; everything else normalizes as a native write.
                if let Some(invocation) = &prompt.mcp {
                    (invocation.category, invocation.to_identity())
                } else {
                    match &prompt.kind {
                        ApprovalKind::Write { path, content, expectation, .. } => self
                            .native_write_operation(path, content, expectation)
                            .unwrap_or_else(|| {
                                let category = match expectation {
                                    WriteExpectation::MustNotExist => TrustCategory::WriteCreate,
                                    WriteExpectation::ExpectRevision(_) => {
                                        TrustCategory::WriteModify
                                    }
                                    WriteExpectation::Blind if !path.exists() => {
                                        TrustCategory::WriteCreate
                                    }
                                    WriteExpectation::Blind => TrustCategory::WriteModify,
                                };
                                (
                                    category,
                                    OperationIdentity::native_tool("fs/write_text_file")
                                        .unwrap_or(OperationIdentity::Unknown),
                                )
                            }),
                        ApprovalKind::WriteBatch { writes, .. } => {
                            self.native_write_batch_operation(writes).unwrap_or_else(|| {
                                let category = if writes.iter().all(|write| {
                                    matches!(write.expectation, WriteExpectation::MustNotExist)
                                        || (matches!(write.expectation, WriteExpectation::Blind)
                                            && !write.path.exists())
                                }) {
                                    TrustCategory::WriteCreate
                                } else {
                                    TrustCategory::WriteModify
                                };
                                (
                                    category,
                                    OperationIdentity::native_tool("fs/write_text_file_batch")
                                        .unwrap_or(OperationIdentity::Unknown),
                                )
                            })
                        }
                        _ => unreachable!(),
                    }
                }
            }
            ApprovalKind::TerminalCreate { request } => {
                let identity = self
                    .command_identity_for_policy_request(request)
                    .unwrap_or(OperationIdentity::Unknown);
                (TrustCategory::Execute, identity)
            }
            ApprovalKind::Filesystem { operation } => (
                match operation {
                    super::agent_filesystem::FilesystemOperation::CreateDirectory { .. }
                    | super::agent_filesystem::FilesystemOperation::CopyPath { .. } => {
                        TrustCategory::WriteCreate
                    }
                    super::agent_filesystem::FilesystemOperation::DeletePath { .. } => {
                        TrustCategory::Delete
                    }
                    super::agent_filesystem::FilesystemOperation::MovePath { .. } => {
                        TrustCategory::WriteModify
                    }
                },
                self.filesystem_policy_identity(operation).unwrap_or_else(|| {
                    OperationIdentity::native_tool(operation.tool_name())
                        .unwrap_or(OperationIdentity::Unknown)
                }),
            ),
            ApprovalKind::Network { route, current_host, call, .. } => {
                let browser_action = match call {
                    WebApprovalCall::Search { .. } | WebApprovalCall::Fetch { .. } => {
                        BrowserActionClass::Fetch
                    }
                    WebApprovalCall::BrowserRun { .. } => BrowserActionClass::Navigate,
                };
                let identity = OperationIdentity::network(
                    NetworkScheme::Https,
                    current_host,
                    443,
                    NetworkMethodClass::Read,
                    browser_action,
                )
                .unwrap_or(OperationIdentity::Unknown);
                let mut operation = TrustOperation {
                    workspace,
                    agent: prompt.agent_id.clone(),
                    transport: route.transport_kind(),
                    category: TrustCategory::Network,
                    identity,
                };
                if operation.is_unknown() {
                    operation.category = TrustCategory::Unknown;
                }
                return operation;
            }
        };
        TrustOperation {
            workspace,
            agent: prompt.agent_id.clone(),
            transport: TransportKind::Acp,
            category,
            identity,
        }
    }

    /// Runs application-owned safeguards against raw typed request fields before
    /// configurable policy. Returned metadata is redacted and versioned.
    fn built_in_safeguard_for_prompt(&self, prompt: &ApprovalPrompt) -> Option<SafeguardMatch> {
        match &prompt.kind {
            ApprovalKind::TerminalCreate { request } => {
                let cwd = request.cwd.as_deref().unwrap_or(&self.working_dir);
                inspect_terminal_command(
                    &request.command,
                    &request.args,
                    cwd,
                    &self.canonical_workspace_roots(),
                    dirs::home_dir().as_deref(),
                    &self.protected_state_paths(),
                )
            }
            ApprovalKind::Write { path, .. } => self.inspect_mutation_path(path),
            ApprovalKind::WriteBatch { writes, .. } => {
                writes.iter().find_map(|write| self.inspect_mutation_path(&write.path))
            }
            ApprovalKind::Filesystem { operation } => {
                use super::agent_filesystem::FilesystemOperation;
                let roots = self.canonical_workspace_roots();
                let paths: Vec<&Path> = match operation {
                    FilesystemOperation::CreateDirectory { path }
                    | FilesystemOperation::DeletePath { path } => vec![path],
                    FilesystemOperation::CopyPath { source, destination }
                    | FilesystemOperation::MovePath { source, destination } => {
                        vec![source, destination]
                    }
                };
                if matches!(operation, FilesystemOperation::DeletePath { .. })
                    && paths.iter().any(|path| {
                        std::fs::canonicalize(path)
                            .ok()
                            .is_some_and(|candidate| roots.contains(&candidate))
                    })
                {
                    return Some(SafeguardMatch::new(
                        CATASTROPHIC_DELETE_RULE_ID,
                        SafeguardCategory::CatastrophicDeletion,
                    ));
                }
                paths.into_iter().find_map(|path| self.inspect_mutation_path(path))
            }
            ApprovalKind::Network { .. } => None,
        }
    }

    fn protected_state_paths(&self) -> Vec<PathBuf> {
        let mut protected = Vec::new();
        if let Some(store) = self.workspace_trust_store() {
            protected.push(store.path().to_path_buf());
        }
        if let Ok(vault) = crate::secrets::default_vault_path() {
            protected.push(vault);
        }
        protected
    }

    fn inspect_mutation_path(&self, path: &Path) -> Option<SafeguardMatch> {
        inspect_protected_state_path(path, &self.protected_state_paths())
            .or_else(|| inspect_special_file(path))
            .or_else(|| inspect_path_escape(path, &self.canonical_workspace_roots()))
    }

    fn command_identity_for_policy_request(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<OperationIdentity, String> {
        if request.command.is_empty()
            || request.command.chars().any(|character| character.is_control())
        {
            return Err("invalid executable token".into());
        }
        validate_argv_tokens(&request.args)?;
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots())?;
        Ok(OperationIdentity::Command {
            executable: request.command.clone(),
            argv: request.args.clone(),
        })
    }

    fn filesystem_policy_identity(
        &self,
        operation: &super::agent_filesystem::FilesystemOperation,
    ) -> Option<OperationIdentity> {
        let relative = |path: &Path| {
            let canonical = self.canonical_native_write_target(path)?;
            self.workspace_relative_segments(&canonical)
        };
        match operation {
            super::agent_filesystem::FilesystemOperation::CreateDirectory { path } => {
                let path = relative(path)?;
                OperationIdentity::filesystem(FilesystemOperationKind::Create, Some(&path), None)
                    .ok()
            }
            super::agent_filesystem::FilesystemOperation::DeletePath { path } => {
                let path = relative(path)?;
                OperationIdentity::filesystem(FilesystemOperationKind::Delete, Some(&path), None)
                    .ok()
            }
            super::agent_filesystem::FilesystemOperation::CopyPath { destination, .. } => {
                let destination = relative(destination)?;
                OperationIdentity::filesystem(
                    FilesystemOperationKind::Create,
                    None,
                    Some(&destination),
                )
                .ok()
            }
            super::agent_filesystem::FilesystemOperation::MovePath { source, destination } => {
                let source = relative(source)?;
                let destination = relative(destination)?;
                OperationIdentity::filesystem(
                    FilesystemOperationKind::Rename,
                    Some(&source),
                    Some(&destination),
                )
                .ok()
            }
        }
    }

    /// Validated command invocation for a terminal request: structured
    /// executable + argv tokens, canonical in-workspace cwd, and the
    /// canonical workspace identity.  Invalid requests (shell wrappers,
    /// control characters, empty tokens, external/relative/traversal/
    /// symlink-escape cwd) return an error and are never eligible for
    /// persistent trust.
    fn command_invocation_for_request(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<CommandInvocation, String> {
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        if request
            .command
            .chars()
            .any(|character| character.is_whitespace() || ";&|()<>$`'\"\\".contains(character))
        {
            return Err("shell command text is ineligible for persistent allow".into());
        }
        validate_command_tokens(&request.command, &request.args)?;
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        let canonical_cwd = resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots())?;
        Ok(CommandInvocation {
            workspace: WorkspaceIdentity::from_canonical_root_bytes(
                primary.as_os_str().as_encoded_bytes(),
            ),
            executable: request.command.clone(),
            argv: request.args.clone(),
            canonical_cwd,
        })
    }

    /// The host-local trust store for the primary workspace, or `None` when
    /// no state directory is available (fail closed: empty effective rules).
    pub(super) fn workspace_trust_store(&self) -> Option<TrustStore> {
        #[cfg(test)]
        if let Some(base) = self.agents.test_trust_store_base.as_deref() {
            return TrustStore::at(base, &self.working_dir).ok();
        }
        TrustStore::default_for(&self.working_dir).ok()
    }

    fn empty_trust_document(&self, workspace: WorkspaceIdentity) -> TrustStoreDocument {
        TrustStoreDocument {
            workspace,
            workspace_enabled: false,
            rules: Vec::new(),
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: crate::policy::FallbackEffect::Confirm,
        }
    }

    fn effective_trust_document(&self, workspace: WorkspaceIdentity) -> TrustStoreDocument {
        if self.agents.trust_policy.borrow().is_none() {
            let document = self
                .workspace_trust_store()
                .map(|store| store.effective_at(self.trust_clock.now()))
                .unwrap_or_else(|| self.empty_trust_document(workspace));
            self.agents.trust_policy.replace(Some(document));
        }
        self.agents
            .trust_policy
            .borrow()
            .clone()
            .unwrap_or_else(|| self.empty_trust_document(workspace))
    }

    pub(crate) fn reload_workspace_trust_store(&self) -> Result<(), TrustStoreError> {
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        let document = store.load_at(self.trust_clock.now())?;
        self.agents.trust_policy.replace(Some(document));
        Ok(())
    }

    /// Canonical workspace identity for the primary (working-directory)
    /// root.
    pub(super) fn primary_workspace_identity(&self) -> WorkspaceIdentity {
        let root =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        WorkspaceIdentity::from_canonical_root_bytes(root.as_os_str().as_encoded_bytes())
    }

    /// Canonical workspace-relative slash-joined path, or `None` when the
    /// path is outside every canonical workspace root.
    fn workspace_relative_segments(&self, canonical: &Path) -> Option<String> {
        let roots = self.canonical_workspace_roots();
        let root = roots.iter().find(|root| canonical.starts_with(root))?;
        let relative = canonical.strip_prefix(root).ok()?;
        if relative.as_os_str().is_empty() {
            return None;
        }
        Some(
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// Shared policy evaluation for one normalized operation against the
    /// host-local store, session state, and the usage ledger (Phase 4).
    /// Time comes from the injected policy clock (Phase 6); the ledger
    /// snapshot is keyed by workspace identity, session, and rule id.
    fn evaluate_operation(
        &self,
        operation: &TrustOperation,
        session_id: &str,
        fingerprint: &str,
    ) -> TrustDecision {
        self.evaluate_operation_with_safeguard(operation, session_id, fingerprint, None)
    }

    fn evaluate_operation_with_safeguard(
        &self,
        operation: &TrustOperation,
        session_id: &str,
        fingerprint: &str,
        built_in_deny: Option<SafeguardMatch>,
    ) -> TrustDecision {
        let now = self.trust_clock.now();
        let effective = self.effective_trust_document(operation.workspace);
        let usage = self.agents.usage_ledger.snapshot(operation.workspace, session_id);
        let tool_key = operation.tool_key();
        let tool_default = effective
            .tool_defaults
            .iter()
            .find(|rule| rule.tool == tool_key)
            .map(|rule| rule.effect);
        let category_default = effective
            .category_defaults
            .iter()
            .find(|rule| rule.category == operation.category)
            .map(|rule| rule.effect);
        evaluate(&PolicyInput {
            session_id,
            fingerprint,
            operation,
            session: &self.agents.approval_policy,
            rules: &effective.rules,
            now,
            usage: &usage,
            workspace_enabled: effective.workspace_enabled,
            built_in_deny,
            tool_default,
            category_default,
            global_default: Some(effective.global_default),
        })
    }

    /// Phase 6 audit: records one redacted automatic-decision event.  The
    /// entry carries the matched rule id, operation category, machine-
    /// readable reason, and remaining use budget only — never raw paths,
    /// command environment, secret values, or MCP arguments.
    fn push_trust_audit(
        &mut self,
        operation: &TrustOperation,
        decision: &TrustDecision,
        session_id: &str,
    ) {
        let remaining_uses = self
            .matched_grant_status(operation, decision, session_id)
            .and_then(|status| status.remaining_uses);
        self.agents.action_log.push(ActionLogEntry::TrustDecision {
            rule_id: decision.rule_id.clone(),
            category: operation.category,
            reason: decision.reason,
            remaining_uses,
            session_id: session_id.to_string(),
        });
    }

    /// Redacted metadata about the rule behind a persistent allow: the
    /// remaining use budget and the absolute expiry (Phase 6 lifecycle).
    fn matched_grant_status(
        &self,
        operation: &TrustOperation,
        decision: &TrustDecision,
        session_id: &str,
    ) -> Option<TrustGrantStatus> {
        let TrustDecision {
            outcome: TrustOutcome::Allow,
            reason: DecisionReason::PersistentAllow,
            rule_id: Some(rule_id),
        } = decision
        else {
            return None;
        };
        let usage = self.agents.usage_ledger.snapshot(operation.workspace, session_id);
        let scope = self
            .effective_trust_document(operation.workspace)
            .rules
            .into_iter()
            .find(|rule| rule.id() == rule_id)?
            .scope()
            .clone();
        Some(TrustGrantStatus {
            remaining_uses: scope.max_uses.map(|max| max.saturating_sub(usage.used(rule_id))),
            expires_at: scope.expires_at,
        })
    }

    /// Curated profile covering a terminal request. Profiles require the
    /// primary workspace-root cwd. Fixed commands use exact argv; `cat` uses
    /// one validated workspace-relative regular-file operand.
    fn profile_id_for_request(&self, request: &CreateTerminalRequest) -> Option<&'static str> {
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        let canonical_cwd = resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots()).ok()?;
        if canonical_cwd != primary {
            return None;
        }
        if let Some((id, _)) = match_profile_entry(&request.command, &request.args) {
            return Some(id);
        }
        self.terminal_readonly_cat_profile(&primary, request)
    }

    /// Matches `cat <one-relative-regular-workspace-file>` for the built-in
    /// terminal-read profile. Shell syntax, flags, traversal, protected paths,
    /// secret-store files, missing files, special files, and escapes fail
    /// closed before a profile rule is considered.
    fn terminal_readonly_cat_profile(
        &self,
        primary: &Path,
        request: &CreateTerminalRequest,
    ) -> Option<&'static str> {
        if request.command != "cat" || request.args.len() != 1 {
            return None;
        }
        validate_command_tokens(&request.command, &request.args).ok()?;
        let operand = &request.args[0];
        let path = Path::new(operand);
        if operand.starts_with('-')
            || path.is_absolute()
            || !path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return None;
        }

        let canonical = std::fs::canonicalize(primary.join(path)).ok()?;
        let relative = canonical.strip_prefix(primary).ok()?;
        if relative.as_os_str().is_empty() || !std::fs::metadata(&canonical).ok()?.is_file() {
            return None;
        }
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if is_protected_relative_path(&relative) || self.is_secret_store_path(&canonical) {
            return None;
        }
        Some(TERMINAL_READONLY_PROFILE)
    }

    /// Phase 4: normalized evaluation for one native workspace read.  Reads
    /// stay prompt-free today; the decision feeds the phase 6 audit trail
    /// and guarantees protected, secret-store, and external reads can never
    /// match a persistent rule.
    pub(crate) fn native_read_decision(
        &mut self,
        path: &Path,
        byte_count: Option<u64>,
    ) -> TrustDecision {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let relative = self.workspace_relative_segments(&canonical);
        let eligible = relative.as_deref().is_some_and(|relative| {
            !is_protected_relative_path(relative) && !self.is_secret_store_path(path)
        });
        let operation = if eligible {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::ReadPath {
                    relative_path: relative.expect("eligible implies relative"),
                    byte_count,
                },
            }
        } else {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::Unknown,
            }
        };
        let decision = self.evaluate_operation(&operation, READ_SESSION, READ_FINGERPRINT);
        self.push_trust_audit(&operation, &decision, READ_SESSION);
        decision
    }

    /// Phase 4: normalized evaluation for one MCP read invocation (stdio
    /// route): ee-pinned `read` classification, matching server/transport/
    /// tool/schema, and a bounded canonical workspace-relative path.
    pub(crate) fn mcp_read_decision(
        &mut self,
        request: &ReadTextFileRequest,
        route: super::agents_mcp::ProxyRoute,
    ) -> TrustDecision {
        let tool = "ee_read_text_file";
        let canonical =
            std::fs::canonicalize(&request.path).unwrap_or_else(|_| request.path.clone());
        let relative = self.workspace_relative_segments(&canonical);
        let eligible = ee_mcp::classify::side_effect_class(tool) == ee_mcp::SideEffectClass::Read
            && relative.as_deref().is_some_and(|relative| {
                !is_protected_relative_path(relative) && !self.is_secret_store_path(&request.path)
            });
        let operation = if eligible {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: route.transport_kind(),
                category: TrustCategory::Read,
                identity: OperationIdentity::McpRead {
                    server: String::from("ee"),
                    transport_identity: route.transport_identity().to_string(),
                    tool: tool.to_string(),
                    tool_schema_version: crate::policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION,
                    relative_path: relative.expect("eligible implies relative"),
                    byte_count: request.limit.map(u64::from),
                },
            }
        } else {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: route.transport_kind(),
                category: TrustCategory::Read,
                identity: OperationIdentity::Unknown,
            }
        };
        let decision = self.evaluate_operation(&operation, READ_SESSION, READ_FINGERPRINT);
        self.push_trust_audit(&operation, &decision, READ_SESSION);
        decision
    }

    /// Whether a host-local broad MCP read profile exists for this workspace.
    /// Legacy narrow read rules remain audit-only, preserving prompt-free read
    /// behavior unless users explicitly opt into this profile.
    fn mcp_read_profile_enforced(&self) -> bool {
        self.workspace_trust_store().is_some_and(|store| {
            store
                .effective_at(self.trust_clock.now())
                .rules
                .iter()
                .any(|rule| matches!(rule, TrustRule::McpReadProfile(_)))
        })
    }

    /// Whether the path is the configured host-local secret-store vault
    /// (never covered by persistent read trust).
    pub(super) fn is_secret_store_path(&self, path: &Path) -> bool {
        let Ok(vault) = crate::secrets::default_vault_path() else {
            return false;
        };
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canonical_vault = std::fs::canonicalize(&vault).unwrap_or(vault);
        paths_equivalent(&canonical, &canonical_vault)
    }

    fn resolve_policy_deny(&mut self, mut prompt: ApprovalPrompt, decision: &TrustDecision) {
        let summary = if decision.reason == DecisionReason::BuiltInDeny {
            let rule_id = decision.rule_id.as_deref().unwrap_or("builtin.unknown");
            format!("blocked by non-overridable safeguard {rule_id}")
        } else {
            decision
                .rule_id
                .as_deref()
                .map(|rule_id| format!("blocked by workspace deny rule {rule_id}"))
                .unwrap_or_else(|| format!("operation denied ({})", decision.reason.as_str()))
        };
        self.record_denied_write(&prompt.session_id, &prompt.kind);
        self.record_denied_validation(&prompt.session_id, &prompt.kind);
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "denied");
        }
        let error = if decision.reason == DecisionReason::BuiltInDeny {
            AgentError::NonOverridableDenied {
                rule_id: decision.rule_id.clone().unwrap_or_else(|| "builtin.unknown".into()),
                category: self
                    .built_in_safeguard_for_prompt(&prompt)
                    .map(|matched| matched.category.as_str().to_string())
                    .unwrap_or_else(|| "unknown".into()),
            }
        } else {
            AgentError::PermissionDenied { reason: summary.clone() }
        };
        self.release_prompt_write_lease(&mut prompt);
        let _ = prompt.reply.send(Err(error));
        if let Some(thread_index) = prompt.thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn persist_deny_rule(&mut self, prompt: &ApprovalPrompt) -> Result<String, TrustStoreError> {
        let candidate = prompt.deny_candidate.as_ref().ok_or_else(|| {
            TrustStoreError::ValidationFailure(
                "approval has no narrow persistent deny scope".into(),
            )
        })?;
        let rule_id = candidate.rule.id().to_string();
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        store.add_rule(candidate.rule.clone())?;
        self.reload_workspace_trust_store()?;
        self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
            rule_id: Some(rule_id.clone()),
            action: "create".into(),
            source: "approval-deny".into(),
        });
        Ok(rule_id)
    }

    fn resolve_persistent_deny_choice(&mut self, mut prompt: ApprovalPrompt) {
        self.record_denied_write(&prompt.session_id, &prompt.kind);
        self.record_denied_validation(&prompt.session_id, &prompt.kind);
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "denied");
        }
        let (reason, summary) = match self.persist_deny_rule(&prompt) {
            Ok(rule_id) => (
                format!("denied and saved workspace rule {rule_id}"),
                format!("workspace deny rule saved: {rule_id}"),
            ),
            Err(_) => (
                "user denied the operation; workspace deny rule was not saved".to_string(),
                "operation denied; workspace deny rule was not saved".to_string(),
            ),
        };
        self.release_prompt_write_lease(&mut prompt);
        let _ = prompt.reply.send(Err(AgentError::PermissionDenied { reason }));
        if let Some(thread_index) = prompt.thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn persist_allow_candidate(
        &mut self,
        candidate: &BoundedRuleCandidate,
    ) -> Result<String, TrustStoreError> {
        if candidate.rule.effect() != TrustEffect::Allow
            || candidate.rule.scope().expires_at.is_none()
            || candidate.rule.scope().max_uses.is_none()
        {
            return Err(TrustStoreError::ValidationFailure(
                "bounded allow candidate lacks mandatory limits".into(),
            ));
        }
        let rule_id = candidate.rule.id().to_string();
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        store.add_rule(candidate.rule.clone())?;
        self.reload_workspace_trust_store()?;
        self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
            rule_id: Some(rule_id.clone()),
            action: "create".into(),
            source: "approval-bounded-allow".into(),
        });
        Ok(rule_id)
    }

    fn resolve_persistent_allow_choice(
        &mut self,
        mut prompt: ApprovalPrompt,
        choice: ApprovalChoice,
    ) {
        let Some(candidate) =
            prompt.allow_candidates.iter().find_map(|(candidate_choice, candidate)| {
                (*candidate_choice == choice).then_some(candidate.clone())
            })
        else {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                reason: "persistent approval has no previewed bounded candidate".into(),
            }));
            return;
        };
        let rule_id = match self.persist_allow_candidate(&candidate) {
            Ok(rule_id) => rule_id,
            Err(error) => {
                self.record_denied_write(&prompt.session_id, &prompt.kind);
                self.release_prompt_write_lease(&mut prompt);
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: format!("persistent approval unavailable: {error}"),
                }));
                if let Some(thread) = prompt.thread_index
                    && let Some(thread) = self.agents.threads.get_mut(thread)
                {
                    thread.push_system("approval denied");
                }
                return;
            }
        };
        self.resolve_persistent_allow(prompt, rule_id);
    }

    /// Resolves one approval with the chosen policy decision.
    fn resolve_approval(&mut self, mut prompt: ApprovalPrompt, choice: ApprovalChoice) {
        // A disconnected proxy client has dropped its receiver. Do not record
        // approval state or dispatch a side effect without a live requester.
        if prompt.reply.is_closed() {
            self.release_prompt_write_lease(&mut prompt);
            return;
        }

        if choice == ApprovalChoice::DenyPersistent {
            self.resolve_persistent_deny_choice(prompt);
            return;
        }

        let fingerprint = approval_fingerprint(&prompt.kind);
        if let Some(decision) = session_decision(choice) {
            self.agents.approval_policy.record(&prompt.session_id, &fingerprint, decision);
        }
        if matches!(
            choice,
            ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        ) {
            self.resolve_persistent_allow_choice(prompt, choice);
            return;
        }
        let allow = choice.allows();
        if !allow {
            self.record_denied_write(&prompt.session_id, &prompt.kind);
            self.record_denied_validation(&prompt.session_id, &prompt.kind);
            if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
                let action = match call {
                    WebApprovalCall::Search { .. } => "search",
                    WebApprovalCall::Fetch { .. } => "fetch",
                    WebApprovalCall::BrowserRun { request } => request.action.as_str(),
                };
                self.record_web_failure(action, current_host, "denied");
            }
            self.release_prompt_write_lease(&mut prompt);
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
        if let Err(error) = self.validate_prompt_write_lease(&prompt) {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        let write_lease = prompt.write_lease.take();
        prompt.write_lease_owner = None;
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
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::WriteBatch { writes, total_edit_count } => {
                self.apply_bridge_write_batch(
                    writes,
                    total_edit_count,
                    &prompt.session_id,
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::TerminalCreate { request } => {
                self.spawn_trusted_terminal(
                    &request,
                    &prompt.session_id,
                    prompt.agent_id.as_deref(),
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::Filesystem { operation } => {
                self.apply_proxy_filesystem(operation, prompt.reply);
            }
            ApprovalKind::Network {
                route,
                requested_host,
                current_host,
                call,
                mut approved_hosts,
                cancellation,
            } => {
                approved_hosts.insert(current_host);
                self.dispatch_web_call(
                    route,
                    prompt.session_id,
                    requested_host,
                    call,
                    approved_hosts,
                    cancellation,
                    prompt.reply,
                );
            }
        }
        if let Some(id) = write_lease {
            self.agents.write_leases.release(id);
        }
    }

    /// Auto-resolves a prompt matched by a persisted host-local rule: the
    /// operation dispatches through the existing pipeline and the successful
    /// dispatch consumes one rule use.
    fn resolve_persistent_allow(&mut self, mut prompt: ApprovalPrompt, rule_id: String) {
        let session_id = prompt.session_id.clone();
        match &prompt.kind {
            ApprovalKind::TerminalCreate { .. } => match prompt.kind {
                ApprovalKind::TerminalCreate { request } => self.spawn_trusted_terminal(
                    &request,
                    &session_id,
                    prompt.agent_id.as_deref(),
                    Some(rule_id),
                    prompt.reply,
                ),
                _ => unreachable!(),
            },
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                self.dispatch_write_prompt(prompt, Some(rule_id));
            }
            ApprovalKind::Network { .. } => match prompt.kind {
                ApprovalKind::Network {
                    route,
                    requested_host,
                    current_host,
                    call,
                    mut approved_hosts,
                    cancellation,
                } => {
                    // Network dispatch completion is asynchronous. Consume before
                    // dispatch so failed/cancelled attempts cannot expand authority.
                    self.agents.usage_ledger.record_use(
                        self.primary_workspace_identity(),
                        &session_id,
                        &rule_id,
                    );
                    approved_hosts.insert(current_host);
                    self.dispatch_web_call(
                        route,
                        session_id,
                        requested_host,
                        call,
                        approved_hosts,
                        cancellation,
                        prompt.reply,
                    );
                }
                _ => unreachable!(),
            },
            ApprovalKind::Filesystem { .. } => {
                self.release_prompt_write_lease(&mut prompt);
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: String::from("persistent filesystem approval is not supported"),
                }));
            }
        }
    }

    /// Dispatches an approved write or write batch and consumes the matched
    /// persistent rule use only after the write succeeds.
    fn dispatch_write_prompt(&mut self, mut prompt: ApprovalPrompt, rule_id: Option<String>) {
        if let Err(error) = self.validate_prompt_write_lease(&prompt) {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        let session_id = prompt.session_id.clone();
        let write_lease = prompt.write_lease.take();
        prompt.write_lease_owner = None;
        match prompt.kind {
            ApprovalKind::Write {
                path,
                content,
                tool_call_id,
                expectation,
                reply_kind,
                proxy_edit_count,
            } => self.apply_bridge_write(
                PreparedWrite {
                    path,
                    content,
                    tool_call_id,
                    expectation,
                    reply_kind,
                    proxy_edit_count,
                },
                &session_id,
                rule_id.as_deref(),
                prompt.reply,
            ),
            ApprovalKind::WriteBatch { writes, total_edit_count } => {
                self.apply_bridge_write_batch(
                    writes,
                    total_edit_count,
                    &session_id,
                    rule_id.as_deref(),
                    prompt.reply,
                );
            }
            _ => {
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: String::from("persistent approval not available for this operation"),
                }));
            }
        }
        if let Some(id) = write_lease {
            self.agents.write_leases.release(id);
        }
    }

    // ── Phase 5: native write normalization and bounded write grants ─────

    /// Canonical path identity shared by workspace validation, write leases,
    /// and policy normalization.
    fn canonical_workspace_write_target(&self, path: &Path) -> Option<PathBuf> {
        let candidate = if path.exists() {
            std::fs::canonicalize(path).ok()?
        } else {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            parent.join(path.file_name()?)
        };
        self.workspace_relative_segments(&candidate)?;
        Some(candidate)
    }

    /// Canonical in-workspace target eligible for bounded native-write trust.
    /// Protected and secret-store targets never qualify.
    fn canonical_native_write_target(&self, path: &Path) -> Option<PathBuf> {
        let candidate = self.canonical_workspace_write_target(path)?;
        let relative = self.workspace_relative_segments(&candidate)?;
        if is_protected_relative_path(&relative) || self.is_secret_store_path(&candidate) {
            return None;
        }
        Some(candidate)
    }

    /// Normalized native write operation: canonical in-workspace target,
    /// create/modify category from the file-existence expectation, and the
    /// exact file count and byte deltas of this request.  Ineligible targets
    /// (external, traversal, symlink escape, protected) normalize to `None`
    /// and can never match a persistent rule.
    fn native_write_operation(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<(TrustCategory, OperationIdentity)> {
        let candidate = self.canonical_native_write_target(path)?;
        let relative_path = self.workspace_relative_segments(&candidate)?;
        let exists = candidate.is_file();
        let category = match expectation {
            WriteExpectation::MustNotExist => TrustCategory::WriteCreate,
            WriteExpectation::ExpectRevision(_) => TrustCategory::WriteModify,
            WriteExpectation::Blind if exists => TrustCategory::WriteModify,
            WriteExpectation::Blind => TrustCategory::WriteCreate,
        };
        let bytes = content.len() as u64;
        Some((
            category,
            OperationIdentity::Write {
                relative_path,
                file_count: 1,
                total_bytes: Some(bytes),
                max_file_bytes: Some(bytes),
            },
        ))
    }

    /// Normalized native write-batch operation: every target must resolve
    /// canonically in-workspace, share one canonical parent directory, and
    /// agree on create vs modify; otherwise the batch is unknown and can
    /// never match a persistent rule.
    pub(crate) fn native_write_batch_operation(
        &self,
        writes: &[PreparedWrite],
    ) -> Option<(TrustCategory, OperationIdentity)> {
        if writes.is_empty() {
            return None;
        }
        let mut parent_dir: Option<PathBuf> = None;
        let mut relative_dir: Option<String> = None;
        let mut total_bytes = 0u64;
        let mut max_file_bytes = 0u64;
        let mut all_existing = true;
        let mut all_new = true;
        for write in writes {
            let candidate = self.canonical_native_write_target(&write.path)?;
            let dir = candidate.parent()?;
            match &parent_dir {
                None => {
                    parent_dir = Some(dir.to_path_buf());
                    relative_dir = self.workspace_relative_segments(dir);
                }
                Some(known) if known != dir => return None,
                _ => {}
            }
            total_bytes = total_bytes.saturating_add(write.content.len() as u64);
            max_file_bytes = max_file_bytes.max(write.content.len() as u64);
            let exists = candidate.is_file();
            all_existing &= exists;
            all_new &= !exists;
        }
        let category = if all_existing {
            TrustCategory::WriteModify
        } else if all_new {
            TrustCategory::WriteCreate
        } else {
            return None;
        };
        Some((
            category,
            OperationIdentity::Write {
                relative_path: relative_dir?,
                file_count: writes.len() as u64,
                total_bytes: Some(total_bytes),
                max_file_bytes: Some(max_file_bytes),
            },
        ))
    }

    /// Bounded persistent write rule derivable from one native write
    /// request: canonical directory prefix, exact request sizes, and the
    /// create/modify operation kind — all within the application safety
    /// maxima.  Root-level targets (no directory prefix) and over-maximum
    /// requests are ineligible.
    fn native_single_write_rule_shape(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let (category, identity) = self.native_write_operation(path, content, expectation)?;
        self.write_rule_shape_from(category, identity, path)
    }

    pub(crate) fn native_batch_write_rule_shape(
        &self,
        writes: &[PreparedWrite],
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let (category, identity) = self.native_write_batch_operation(writes)?;
        self.write_rule_shape_from(category, identity, &writes.first()?.path)
    }

    /// Shared shape derivation: directory prefix from the first target and
    /// request bounds checked against the application safety maxima.
    fn write_rule_shape_from(
        &self,
        category: TrustCategory,
        identity: OperationIdentity,
        first_target: &Path,
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let OperationIdentity::Write { file_count, total_bytes, max_file_bytes, .. } = identity
        else {
            return None;
        };
        let candidate = self.canonical_native_write_target(first_target)?;
        let dir = self.workspace_relative_segments(candidate.parent()?)?;
        let prefix = PathPrefix::parse(&dir).ok()?;
        let operation = match category {
            TrustCategory::WriteCreate => WriteOperationKind::Create,
            TrustCategory::WriteModify => WriteOperationKind::Modify,
            _ => return None,
        };
        if file_count == 0 || file_count > MAX_WRITE_FILES {
            return None;
        }
        let total = total_bytes?;
        let max_file = max_file_bytes?;
        if total == 0 || total > MAX_WRITE_TOTAL_BYTES || max_file > MAX_WRITE_FILE_BYTES {
            return None;
        }
        Some((operation, prefix, file_count, total, max_file))
    }

    /// Persistent option label for one eligible native write; `None` keeps
    /// the prompt on the four-choice UI (protected, external, root-level,
    /// and over-maximum requests never get a persistent grant).
    fn native_write_persistent_label(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<&'static str> {
        self.native_single_write_rule_shape(path, content, expectation)
            .map(|_| PERSISTENT_WRITE_OPTION_LABEL)
    }

    /// Spawns an approved terminal through the existing pipeline and records
    /// the matched persistent rule use only after a successful spawn.
    fn spawn_trusted_terminal(
        &mut self,
        request: &CreateTerminalRequest,
        session_id: &str,
        agent_id: Option<&str>,
        rule_id: Option<String>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let validation = self.validation_run_for_terminal(session_id, request);
        let result = self.agents.terminals.spawn(request, agent_id);
        match (&result, validation) {
            (Ok(response), Some(validation)) => {
                if let Err(error) = self.agents.terminals.register_validation_run(
                    &response.terminal_id,
                    &request.session_id,
                    validation,
                ) {
                    tracing::warn!(
                        session_id,
                        ?error,
                        "cannot record terminal validation lifecycle"
                    );
                }
            }
            (Err(_), Some(validation)) => {
                self.record_unavailable_validation(session_id, request, validation);
            }
            _ => {}
        }
        if result.is_ok()
            && let Some(rule_id) = rule_id
        {
            self.agents.usage_ledger.record_use(
                self.primary_workspace_identity(),
                session_id,
                &rule_id,
            );
        }
        let _ = reply.send(result.map(ClientRequestResponse::CreateTerminal));
    }

    #[cfg(test)]
    pub(crate) fn queue_terminal_approval_for_test(
        &mut self,
        session_id: &str,
        agent_id: Option<&str>,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: Option<PathBuf>,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request = CreateTerminalRequest::new(SessionId::new(session_id), command)
            .args(args.iter().map(|value| (*value).to_string()).collect())
            .env(env.iter().map(|(name, value)| EnvVariable::new(*name, *value)).collect())
            .cwd(cwd);
        let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::terminal(
            None,
            agent_id.map(str::to_string),
            &SessionId::new(session_id),
            &request,
            reply,
            persistent_allowed,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_write_approval_for_test(
        &mut self,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request =
            WriteTextFileRequest::new(SessionId::new("persistent-deny-write"), path, content);
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::write(
            None,
            &request.session_id,
            &request,
            None,
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_session_write_approval_for_test(
        &mut self,
        agent_id: &str,
        session_id: &str,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request = WriteTextFileRequest::new(SessionId::new(session_id), path, content);
        let (reply, receiver) = oneshot::channel();
        let mut prompt = ApprovalPrompt::write(None, &request.session_id, &request, None, reply);
        prompt.agent_id = Some(agent_id.to_string());
        self.request_bridge_approval(prompt);
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_filesystem_create_approval_for_test(
        &mut self,
        path: PathBuf,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.queue_proxy_filesystem(
            super::agent_filesystem::FilesystemOperation::CreateDirectory { path },
            reply,
        );
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_filesystem_delete_approval_for_test(
        &mut self,
        path: PathBuf,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.queue_proxy_filesystem(
            super::agent_filesystem::FilesystemOperation::DeletePath { path },
            reply,
        );
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_network_fetch_approval_for_test(
        &mut self,
        host: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.request_web_approval(ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:persistent-deny-test"),
            host.to_string(),
            host.to_string(),
            None,
            WebApprovalCall::Fetch { url: format!("https://{host}/blocked") },
            BTreeSet::new(),
            CancellationToken::new(),
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_generic_mcp_write_approval_for_test(
        &mut self,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let invocation = self
            .mcp_invocation_for_tool(
                "ee_format_file",
                serde_json::json!({ "path": path }),
                ProxyRoute::Stdio,
            )
            .expect("format tool must have exact MCP identity");
        let spec = ProxyWriteSpec {
            title: String::from("ee_format_file"),
            detail: path.display().to_string(),
            prepared: PreparedWrite {
                path,
                content: content.to_string(),
                tool_call_id: None,
                expectation: WriteExpectation::Blind,
                reply_kind: WriteReplyKind::ProxyStructured,
                proxy_edit_count: 1,
            },
        };
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::proxy_write(
            spec,
            Some(invocation),
            None,
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn reset_web_dispatch_count_for_test() {
        WEB_DISPATCH_TEST_COUNT.store(0, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn web_dispatch_count_for_test() -> usize {
        WEB_DISPATCH_TEST_COUNT.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn confirm_bridge_approval_for_test(&mut self, choice: ApprovalChoice) {
        self.confirm_bridge_approval(choice);
    }

    /// Confirms the front approval with the selected option.
    pub(super) fn confirm_bridge_approval(&mut self, choice: ApprovalChoice) {
        if matches!(
            choice,
            ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        ) && let Some(prompt) = self.agents.approvals.front_mut()
            && prompt.confirming_allow != Some(choice)
        {
            prompt.confirming_allow = Some(choice);
            self.backend.status_message =
                Some(String::from("confirm bounded workspace allow rule"));
            return;
        }
        if choice == ApprovalChoice::DenyPersistent
            && let Some(prompt) = self.agents.approvals.front_mut()
            && !prompt.confirming_deny
        {
            prompt.confirming_deny = true;
            self.backend.status_message = Some(String::from("confirm workspace deny rule"));
            return;
        }
        let Some(prompt) = self.agents.approvals.pop_front() else {
            return;
        };
        self.resolve_approval(prompt, choice);
    }

    #[cfg(test)]
    pub(crate) fn cancel_rule_confirmation_for_test(&mut self) {
        self.cancel_rule_confirmation();
    }

    pub(super) fn cancel_rule_confirmation(&mut self) {
        if let Some(prompt) = self.agents.approvals.front_mut() {
            prompt.confirming_deny = false;
            prompt.confirming_allow = None;
            self.backend.status_message = Some(String::from("trust rule confirmation cancelled"));
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pre_write_verification_test_hook(
        hook: impl FnOnce(&mut App) + Send + 'static,
    ) {
        *PRE_WRITE_VERIFICATION_TEST_HOOK
            .lock()
            .expect("pre-write verification test hook poisoned") = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_pre_write_verification_test_hook(&mut self) {
        if let Some(hook) = PRE_WRITE_VERIFICATION_TEST_HOOK
            .lock()
            .expect("pre-write verification test hook poisoned")
            .take()
        {
            hook(self);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_post_write_test_hook(hook: impl FnOnce(&mut App) + Send + 'static) {
        *POST_WRITE_TEST_HOOK.lock().expect("post-write test hook poisoned") = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_post_write_test_hook(&mut self) {
        if let Some(hook) =
            POST_WRITE_TEST_HOOK.lock().expect("post-write test hook poisoned").take()
        {
            hook(self);
        }
    }

    /// Records the current revision after a test-controlled editor mutation.
    ///
    /// This captures the real buffer state at the same reduction boundary as a
    /// user edit; tests never construct `TurnObservation` values themselves.
    #[cfg(test)]
    fn observe_post_write_test_revision(&self, session_id: &str, paths: &[PathBuf]) {
        if let Ok(revision) = self.evidence_revision_for_paths(paths) {
            self.observe_active_turn(session_id, TurnObservation::Revision { revision });
        }
    }

    /// Performs an approved buffer write: open/reuse buffer, diff, edit,
    /// verify, save — all through existing buffer/save semantics.  A matched
    /// persistent rule consumes one use only after the write succeeds.
    fn apply_bridge_write(
        &mut self,
        prepared: PreparedWrite,
        session_id: &str,
        rule_id: Option<&str>,
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

        let paths = vec![path.to_path_buf()];
        let pre_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: pre_write_revision.clone() },
        );
        let blocks_dirty_buffer = self.has_dirty_buffer(&paths)
            && matches!(
                prepared.expectation,
                WriteExpectation::Blind | WriteExpectation::MustNotExist
            );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Read,
            if blocks_dirty_buffer { EvidenceCheck::Failed } else { EvidenceCheck::Passed },
        );
        if blocks_dirty_buffer {
            self.observe_active_turn(
                session_id,
                TurnObservation::Write {
                    revision: pre_write_revision,
                    outcome: WriteEvidenceOutcome::Conflicted,
                },
            );
            let _ = reply.send(Err(AgentError::invalid_params(
                "dirty editor buffer requires explicit user handoff before blind agent write",
            )));
            return;
        }
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Preview,
            EvidenceCheck::Passed,
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: pre_write_revision.clone(),
                outcome: WriteEvidenceOutcome::Approved,
            },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Passed,
        );
        let diagnostics_before = self.refresh_diagnostic_error_count(&paths).ok();
        match self.write_through_buffer(path, content) {
            Ok(outcome) => {
                if let Some(rule_id) = rule_id {
                    self.agents.usage_ledger.record_use(
                        self.primary_workspace_identity(),
                        session_id,
                        rule_id,
                    );
                }
                let changed = outcome.old_content != content;
                self.agents.action_log.push(ActionLogEntry::Write {
                    path: path.to_path_buf(),
                    old_fingerprint: fingerprint(&outcome.old_content),
                    new_fingerprint: fingerprint(content),
                    tool_call_id: prepared.tool_call_id,
                    session_id: session_id.to_string(),
                });
                let response = match prepared.reply_kind {
                    WriteReplyKind::FsWrite => {
                        ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new())
                    }
                    WriteReplyKind::ProxyStructured => {
                        ClientRequestResponse::ProxyValue(json!(ee_mcp::EditTextResult {
                            changed_file: path.display().to_string(),
                            byte_count: outcome.byte_count,
                            edit_count: prepared.proxy_edit_count,
                            new_revision: outcome.new_revision.clone(),
                            saved: outcome.saved,
                            dirty: outcome.dirty,
                        }))
                    }
                };
                // A successful response confirms only this completed write.
                // Host-owned diagnostics and validation evidence continue below.
                let _ = reply.send(Ok(response));
                let post_write_revision = self
                    .evidence_revision_for_paths(&paths)
                    .unwrap_or_else(|_| EvidenceRevision::new(&outcome.new_revision));
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Revision { revision: post_write_revision.clone() },
                );
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Write {
                        revision: post_write_revision.clone(),
                        outcome: if changed {
                            WriteEvidenceOutcome::Applied
                        } else {
                            WriteEvidenceOutcome::NoOp
                        },
                    },
                );
                self.observe_transaction_stage(
                    session_id,
                    post_write_revision,
                    WriteTransactionStage::Apply,
                    EvidenceCheck::Passed,
                );
                if changed {
                    #[cfg(test)]
                    self.run_pre_write_verification_test_hook();
                    self.collect_post_write_verification(session_id, &paths, diagnostics_before);
                    #[cfg(test)]
                    {
                        self.run_post_write_test_hook();
                        self.observe_post_write_test_revision(session_id, &paths);
                    }
                }
                if let Some(thread) = self.session_thread_by_id(session_id) {
                    self.agents.threads[thread]
                        .push_system(format!("agent wrote: {}", path.display()));
                }
            }
            Err(error) => {
                let revision = self
                    .evidence_revision_for_paths(&paths)
                    .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
                let outcome = if error.to_string().contains("conflict") {
                    WriteEvidenceOutcome::Conflicted
                } else {
                    WriteEvidenceOutcome::Failed
                };
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Revision { revision: revision.clone() },
                );
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Write { revision: revision.clone(), outcome },
                );
                self.observe_transaction_stage(
                    session_id,
                    revision.clone(),
                    WriteTransactionStage::Apply,
                    EvidenceCheck::Failed,
                );
                self.observe_transaction_stage(
                    session_id,
                    revision,
                    WriteTransactionStage::RollbackSafety,
                    EvidenceCheck::Unavailable,
                );
                let _ = reply.send(Err(error));
            }
        }
    }

    fn apply_bridge_write_batch(
        &mut self,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        session_id: &str,
        rule_id: Option<&str>,
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
        let paths = writes.iter().map(|prepared| prepared.path.clone()).collect::<Vec<_>>();
        let blocks_dirty_buffer = self.has_dirty_buffer(&paths)
            && writes.iter().any(|prepared| {
                matches!(
                    prepared.expectation,
                    WriteExpectation::Blind | WriteExpectation::MustNotExist
                )
            });
        let pre_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: pre_write_revision.clone() },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Read,
            if blocks_dirty_buffer { EvidenceCheck::Failed } else { EvidenceCheck::Passed },
        );
        if blocks_dirty_buffer {
            self.observe_active_turn(
                session_id,
                TurnObservation::Write {
                    revision: pre_write_revision,
                    outcome: WriteEvidenceOutcome::Conflicted,
                },
            );
            let _ = reply.send(Err(AgentError::invalid_params(
                "dirty editor buffer requires explicit user handoff before agent write",
            )));
            return;
        }
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Preview,
            EvidenceCheck::Passed,
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: pre_write_revision.clone(),
                outcome: WriteEvidenceOutcome::Approved,
            },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Passed,
        );
        let diagnostics_before = self.refresh_diagnostic_error_count(&paths).ok();
        let mut changed = false;
        let mut files = Vec::new();
        for prepared in writes {
            let path = prepared.path.clone();
            match self.write_through_buffer(path.as_path(), prepared.content.as_str()) {
                Ok(outcome) => {
                    changed |= outcome.old_content != prepared.content;
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
                    let revision = self
                        .evidence_revision_for_paths(&paths)
                        .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
                    let outcome = if error.to_string().contains("conflict") {
                        WriteEvidenceOutcome::Conflicted
                    } else {
                        WriteEvidenceOutcome::Failed
                    };
                    self.observe_active_turn(
                        session_id,
                        TurnObservation::Revision { revision: revision.clone() },
                    );
                    self.observe_active_turn(
                        session_id,
                        TurnObservation::Write { revision: revision.clone(), outcome },
                    );
                    self.observe_transaction_stage(
                        session_id,
                        revision.clone(),
                        WriteTransactionStage::Apply,
                        EvidenceCheck::Failed,
                    );
                    if !files.is_empty() {
                        self.observe_transaction_stage(
                            session_id,
                            revision,
                            WriteTransactionStage::RollbackSafety,
                            EvidenceCheck::Unavailable,
                        );
                    }
                    let _ = reply.send(Err(error));
                    return;
                }
            }
        }
        let post_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: post_write_revision.clone() },
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: post_write_revision.clone(),
                outcome: if changed {
                    WriteEvidenceOutcome::Applied
                } else {
                    WriteEvidenceOutcome::NoOp
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            post_write_revision,
            WriteTransactionStage::Apply,
            EvidenceCheck::Passed,
        );
        if changed {
            #[cfg(test)]
            self.run_pre_write_verification_test_hook();
            self.collect_post_write_verification(session_id, &paths, diagnostics_before);
            #[cfg(test)]
            {
                self.run_post_write_test_hook();
                self.observe_post_write_test_revision(session_id, &paths);
            }
        }
        if let Some(rule_id) = rule_id {
            self.agents.usage_ledger.record_use(
                self.primary_workspace_identity(),
                session_id,
                rule_id,
            );
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

    fn buffer_id_for_path(&self, path: &Path) -> Option<crate::buffer::BufferId> {
        self.backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
            .map(|buf| buf.id)
    }

    /// Captures one explicitly selected, primary-workspace context file.
    /// Open buffers win so unsaved user edits are the snapshot sent to agent.
    pub(super) fn agent_context_file_snapshot(
        &self,
        relative_path: &Path,
        max_bytes: usize,
    ) -> Result<(PathBuf, String, String), String> {
        if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
            return Err(String::from("context path must be workspace-relative"));
        }
        let root = std::fs::canonicalize(&self.working_dir)
            .map_err(|error| format!("cannot access workspace: {error}"))?;
        let canonical = std::fs::canonicalize(root.join(relative_path)).map_err(|error| {
            format!("cannot access context file {}: {error}", relative_path.display())
        })?;
        let relative = canonical
            .strip_prefix(&root)
            .map_err(|_| String::from("context file outside primary workspace"))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if relative.is_empty()
            || is_protected_relative_path(&relative)
            || self.is_secret_store_path(&canonical)
        {
            return Err(format!("context file is protected: {relative}"));
        }
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("cannot inspect context file {relative}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("context path is not a regular file: {relative}"));
        }

        let content = if let Some(buffer) = self.backend.all_bufs().iter().find(|buffer| {
            buffer.path.as_deref().is_some_and(|path| paths_equivalent(path, &canonical))
        }) {
            if buffer.is_vlf {
                return Err(format!("context file is too large to snapshot: {relative}"));
            }
            buffer
                .whole_text()
                .ok_or_else(|| format!("cannot snapshot context file: {relative}"))?
        } else {
            if metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
                return Err(format!("context file exceeds {max_bytes} byte limit: {relative}"));
            }
            std::fs::read_to_string(&canonical)
                .map_err(|error| format!("cannot read context file {relative}: {error}"))?
        };
        if content.len() > max_bytes {
            return Err(format!("context file exceeds {max_bytes} byte limit: {relative}"));
        }
        let secrets = self.agents_secret_values();
        let content = ee_agent_host::redact::redact_secret_values(&content, &secrets);
        Ok((canonical, relative, content))
    }

    fn session_thread_by_id(&self, session_id: &str) -> Option<usize> {
        self.agents.thread_index(session_id)
    }

    /// Appends one host-owned observation only while this exact ACP session
    /// owns an active turn. Generic stdio MCP proxy calls use the synthetic
    /// `proxy` session and deliberately cannot borrow a pane turn's evidence.
    fn observe_active_turn(&self, session_id: &str, observation: TurnObservation) {
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let thread = &self.agents.threads[index].host;
        let Some(turn) = thread.active_turn_key() else {
            return;
        };
        if let Err(error) = thread.observe_turn_evidence(turn.turn_id(), observation) {
            tracing::warn!(
                session_id,
                turn_id = turn.turn_id(),
                ?error,
                "bridge evidence was not recorded"
            );
        }
    }

    fn validation_run_for_terminal(
        &mut self,
        session_id: &str,
        request: &CreateTerminalRequest,
    ) -> Option<TerminalValidationRun> {
        let index = self.session_thread_by_id(session_id)?;
        let (revision, scope) = {
            let thread = &self.agents.threads[index];
            (thread.verification_revision.clone()?, thread.verification_paths.clone())
        };
        Some(TerminalValidationRun {
            revision,
            selector: terminal_command_line(request),
            diagnostics_before: self.refresh_diagnostic_error_count(&scope).ok(),
        })
    }

    fn observe_transaction_stage(
        &self,
        session_id: &str,
        revision: EvidenceRevision,
        stage: WriteTransactionStage,
        outcome: EvidenceCheck,
    ) {
        self.observe_active_turn(
            session_id,
            TurnObservation::WriteTransaction { revision, stage, outcome },
        );
    }

    fn record_unavailable_validation(
        &self,
        session_id: &str,
        request: &CreateTerminalRequest,
        validation: TerminalValidationRun,
    ) {
        self.observe_active_turn(
            session_id,
            TurnObservation::ValidationRecord {
                revision: validation.revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: format!("unavailable:{}", terminal_command_line(request)),
                    command_id: terminal_command_line(request),
                    command: terminal_command_line(request),
                    tool: Some(String::from("terminal")),
                    selector: Some(validation.selector),
                    outcome: EvidenceCheck::Unavailable,
                    exit_status: None,
                    elapsed_ms: None,
                    affected_tests: Vec::new(),
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: Some(String::from("terminal_unavailable")),
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            validation.revision,
            WriteTransactionStage::Validation,
            EvidenceCheck::Unavailable,
        );
    }

    fn record_denied_validation(&self, session_id: &str, kind: &ApprovalKind) {
        let ApprovalKind::TerminalCreate { request } = kind else {
            return;
        };
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let Some(revision) = self.agents.threads[index].verification_revision.clone() else {
            return;
        };
        self.observe_active_turn(
            session_id,
            TurnObservation::ValidationRecord {
                revision: revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: format!("denied:{}", terminal_command_line(request)),
                    command_id: terminal_command_line(request),
                    command: terminal_command_line(request),
                    tool: Some(String::from("terminal")),
                    selector: Some(String::from("approved_terminal")),
                    outcome: EvidenceCheck::Denied,
                    exit_status: None,
                    elapsed_ms: None,
                    affected_tests: Vec::new(),
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: Some(String::from("approval_denied")),
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision,
            WriteTransactionStage::Validation,
            EvidenceCheck::Denied,
        );
    }

    fn record_denied_write(&self, session_id: &str, kind: &ApprovalKind) {
        let paths = match kind {
            ApprovalKind::Write { path, .. } => vec![path.clone()],
            ApprovalKind::WriteBatch { writes, .. } => {
                writes.iter().map(|write| write.path.clone()).collect()
            }
            ApprovalKind::Filesystem { .. }
            | ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::Network { .. } => return,
        };
        let revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: revision.clone(),
                outcome: WriteEvidenceOutcome::Denied,
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Denied,
        );
    }

    /// Hashes current buffer/disk revisions for exactly the write sequence.
    /// Raw paths and buffer contents never enter the evidence store.
    fn evidence_revision_for_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<EvidenceRevision, AgentError> {
        let mut members = Vec::with_capacity(paths.len());
        for path in paths {
            let revision =
                self.current_text_revision(path)?.unwrap_or_else(|| String::from("missing"));
            let dirty = self
                .backend
                .all_bufs()
                .iter()
                .find(|buffer| {
                    buffer
                        .path
                        .as_deref()
                        .is_some_and(|candidate| paths_equivalent(candidate, path))
                })
                .is_some_and(|buffer| !buffer.pristine);
            members.push(format!("{}:{revision}:{dirty}", path.display()));
        }
        members.sort();
        Ok(EvidenceRevision::new(format!("sha256:{}", sha256_hex(members.join("\n").as_bytes()))))
    }

    fn has_dirty_buffer(&self, paths: &[PathBuf]) -> bool {
        self.backend.all_bufs().iter().any(|buffer| {
            !buffer.pristine
                && buffer.path.as_deref().is_some_and(|candidate| {
                    paths.iter().any(|path| paths_equivalent(candidate, path))
                })
        })
    }

    fn refresh_diagnostic_error_count(&mut self, paths: &[PathBuf]) -> Result<u32, AgentError> {
        // Drain pending editor/LSP events before reading diagnostics. If the
        // host cannot produce a complete current snapshot, callers record an
        // unavailable fact rather than treating cached diagnostics as passing.
        let _ = self.backend.drain_events();
        let value = self.proxy_get_diagnostics(None)?;
        let diagnostics = serde_json::from_value::<ee_mcp::DiagnosticsResult>(value)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?;
        if diagnostics.truncated {
            return Err(AgentError::HandlerError(String::from(
                "current diagnostics snapshot is truncated",
            )));
        }
        let mut path_set = paths.to_vec();
        path_set.sort();
        path_set.dedup();
        Ok(u32::try_from(
            diagnostics
                .diagnostics
                .into_iter()
                .filter(|entry| {
                    entry.severity == "error"
                        && path_set
                            .iter()
                            .any(|path| paths_equivalent(path, Path::new(&entry.path)))
                })
                .count(),
        )
        .unwrap_or(u32::MAX))
    }

    /// Collects current editor/Git facts after a successful buffer write. A
    /// missing tool, dirty user buffer, conflict, truncated response, or
    /// unavailable Git diff leaves the turn blocked/unverified instead of
    /// treating model prose or ACP completion as verification.
    fn collect_post_write_verification(
        &mut self,
        session_id: &str,
        paths: &[PathBuf],
        diagnostics_before: Option<u32>,
    ) {
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let scope = {
            let thread = &mut self.agents.threads[index];
            for path in paths {
                if !thread.verification_paths.iter().any(|known| paths_equivalent(known, path)) {
                    thread.verification_paths.push(path.clone());
                }
            }
            thread.verification_paths.clone()
        };
        // Apply pending editor updates before snapshotting one revision for every
        // verification fact collected below. Otherwise later host observations can
        // correctly invalidate evidence that was already stale at collection time.
        let _ = self.backend.drain_events();
        let Ok(revision) = self.evidence_revision_for_paths(&scope) else {
            return;
        };
        self.agents.threads[index].verification_revision = Some(revision.clone());
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: revision.clone() },
        );

        let changed_files = match self.proxy_changed_files_result() {
            Ok(result) => result,
            Err(_) => return,
        };
        let expected_present = scope.iter().all(|expected| {
            changed_files
                .files
                .iter()
                .any(|entry| paths_equivalent(expected, Path::new(&entry.path)))
        });
        let has_unsafe_buffer = changed_files.files.iter().any(|entry| {
            scope.iter().any(|expected| paths_equivalent(expected, Path::new(&entry.path)))
                && (entry.conflicted || entry.dirty || !entry.saved)
        });
        self.observe_active_turn(
            session_id,
            TurnObservation::ChangedFiles {
                revision: revision.clone(),
                files: changed_files.files.iter().map(|entry| entry.path.clone()).collect(),
                truncated: changed_files.truncated || !expected_present || has_unsafe_buffer,
            },
        );

        let diagnostics_outcome =
            match (diagnostics_before, self.refresh_diagnostic_error_count(&scope)) {
                (Some(before), Ok(after)) if after <= before => EvidenceCheck::Passed,
                (Some(_), Ok(_)) => EvidenceCheck::Failed,
                _ => EvidenceCheck::Unavailable,
            };
        self.observe_active_turn(
            session_id,
            TurnObservation::Diagnostics {
                revision: revision.clone(),
                outcome: diagnostics_outcome,
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision.clone(),
            WriteTransactionStage::Diagnostics,
            diagnostics_outcome,
        );

        let diff_outcome = self.proxy_git_diff().and_then(|value| {
            serde_json::from_value::<ee_mcp::GitDiffResult>(value)
                .map_err(|error| AgentError::HandlerError(error.to_string()))
        });
        let review_passed = diff_outcome.is_ok_and(|diff| {
            !diff.truncated
                && !diff.diff.is_empty()
                && !has_unsafe_buffer
                && changed_files.files.iter().all(|entry| !entry.conflicted)
        });
        let diff_outcome =
            if review_passed { EvidenceCheck::Passed } else { EvidenceCheck::Unavailable };
        self.observe_active_turn(
            session_id,
            TurnObservation::DiffReview { revision: revision.clone(), outcome: diff_outcome },
        );
        self.observe_transaction_stage(
            session_id,
            revision.clone(),
            WriteTransactionStage::FinalDiff,
            diff_outcome,
        );

        // Do not synthesize a pending validation command. A selected terminal
        // is registered only after its approved spawn and contributes evidence
        // only after its observed lifecycle completes.
    }

    fn record_terminal_validation(&mut self, completion: TerminalCompletion) {
        let Some(validation) = completion.validation else {
            return;
        };
        let Some(index) = self.session_thread_by_id(&completion.session_id) else {
            return;
        };
        let scope = self.agents.threads[index].verification_paths.clone();
        let diagnostics_after = self.refresh_diagnostic_error_count(&scope);
        let (diagnostics_outcome, diagnostics_delta) =
            match (validation.diagnostics_before, diagnostics_after) {
                (Some(before), Ok(after)) if after <= before => {
                    (EvidenceCheck::Passed, i64::from(after) - i64::from(before))
                }
                (Some(before), Ok(after)) => {
                    (EvidenceCheck::Failed, i64::from(after) - i64::from(before))
                }
                _ => (EvidenceCheck::Unavailable, 0),
            };
        self.observe_active_turn(
            &completion.session_id,
            TurnObservation::Diagnostics {
                revision: validation.revision.clone(),
                outcome: diagnostics_outcome,
            },
        );
        let outcome = if matches!(diagnostics_outcome, EvidenceCheck::Passed) {
            match completion.exit_code {
                Some(0) => EvidenceCheck::Passed,
                Some(_) => EvidenceCheck::Failed,
                None => EvidenceCheck::Unavailable,
            }
        } else {
            EvidenceCheck::Unavailable
        };
        self.observe_active_turn(
            &completion.session_id,
            TurnObservation::ValidationRecord {
                revision: validation.revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: completion.terminal_id.clone(),
                    command_id: completion.terminal_id,
                    command: completion.command.clone(),
                    tool: Some(String::from("terminal")),
                    selector: Some(validation.selector),
                    outcome,
                    exit_status: completion.exit_code,
                    elapsed_ms: Some(completion.elapsed_ms),
                    affected_tests: Vec::new(),
                    diagnostics_delta,
                    output_truncated: completion.output_truncated,
                    skip_or_denial: None,
                },
            },
        );
        self.observe_transaction_stage(
            &completion.session_id,
            validation.revision,
            WriteTransactionStage::Validation,
            outcome,
        );
        self.agents.threads[index].push_system(format!(
            "validation terminal completed (exit: {}; {}ms; output truncated: {})",
            completion.exit_code.map_or_else(|| String::from("unknown"), |code| code.to_string()),
            completion.elapsed_ms,
            completion.output_truncated,
        ));
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

    /// Validated generic MCP invocation for an eligible proxy tool call
    /// (Phase 3): server identity `ee`, pinned manifest schema version,
    /// side-effect classification, canonical exact JSON arguments, and the
    /// delivering transport.  Returns `None` for tools that never qualify
    /// (content-bearing writes, terminal-create, read/unknown tools).
    fn mcp_invocation_for_tool(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        route: super::agents_mcp::ProxyRoute,
    ) -> Option<McpInvocation> {
        if !ee_mcp::classify::exact_trust_eligible(tool) {
            return None;
        }
        let arguments_json =
            crate::policy::rules::canonicalize_arguments_json(&arguments.to_string()).ok()?;
        let category = match ee_mcp::classify::side_effect_class(tool) {
            ee_mcp::SideEffectClass::Read => TrustCategory::Read,
            ee_mcp::SideEffectClass::Write => TrustCategory::WriteModify,
            ee_mcp::SideEffectClass::Execute => TrustCategory::Execute,
            ee_mcp::SideEffectClass::Unknown => TrustCategory::Unknown,
        };
        Some(McpInvocation {
            workspace: self.primary_workspace_identity(),
            agent: None,
            transport: route.transport_kind(),
            transport_identity: route.transport_identity().to_string(),
            server: String::from("ee"),
            tool: tool.to_string(),
            tool_schema_version: ee_mcp::classify::EE_TOOL_SCHEMA_VERSION,
            category,
            arguments_json,
        })
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
                let persistent_label =
                    self.native_write_persistent_label(&path, &content, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_replace_text"),
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
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
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
                let persistent_label =
                    self.native_write_persistent_label(&path, &content, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_apply_patch"),
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
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_filesystem(
        &mut self,
        operation: super::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        // Application safeguards inspect typed paths before ordinary validation;
        // executor validates again immediately before mutation.
        if let Some(path) = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buffer| buffer.path.as_deref())
            .find(|path| operation.affected_open_path(path))
        {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "filesystem operation affects open buffer: {}",
                path.display()
            ))));
            return;
        }
        self.request_bridge_approval(ApprovalPrompt::filesystem(operation, reply));
    }

    fn apply_proxy_filesystem(
        &mut self,
        operation: super::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        if reply.is_closed() {
            return;
        }
        if let Some(path) = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buffer| buffer.path.as_deref())
            .find(|path| operation.affected_open_path(path))
        {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "filesystem operation affects open buffer: {}",
                path.display()
            ))));
            return;
        }
        match super::agent_filesystem::execute(&operation, &self.allowed_fs_roots()) {
            Ok(result) => match serde_json::to_value(result) {
                Ok(value) => {
                    let _ = reply.send(Ok(ClientRequestResponse::ProxyValue(value)));
                }
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(format!(
                        "filesystem result serialization failed: {error}"
                    ))));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(AgentError::Io(format!(
                    "{} failed: {error}",
                    operation.tool_name()
                ))));
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
                let persistent_label = self.native_write_persistent_label(
                    &path,
                    &created,
                    &WriteExpectation::MustNotExist,
                );
                let spec = ProxyWriteSpec {
                    title: String::from("ee_create_text_file"),
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
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
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
                let expectation = WriteExpectation::ExpectRevision(revision);
                let persistent_label =
                    self.native_write_persistent_label(&path, &updated, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_overwrite_text_file"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), updated.len()),
                    prepared: PreparedWrite {
                        path,
                        content: updated,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
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

    fn proxy_git_repository(&self) -> Result<crate::git::GitRepository, AgentError> {
        let root = self
            .active_root_path()
            .or_else(|| std::fs::canonicalize(&self.working_dir).ok())
            .ok_or_else(|| {
                AgentError::HandlerError(String::from("active workspace root is unavailable"))
            })?;
        crate::git::GitRepository::discover(&root)
            .map_err(|error| {
                AgentError::HandlerError(format!("Git repository discovery failed: {error}"))
            })?
            .ok_or_else(|| {
                AgentError::HandlerError(String::from("active workspace is not a Git repository"))
            })
    }

    fn web_context_service(
        &mut self,
    ) -> Result<Arc<ee_agent_host::WebContextService<ee_agent_host::ReqwestWebTransport>>, AgentError>
    {
        // Config is frontend-resolved and secret-redacted in `Debug`. A changed
        // semantic configuration must not reuse prior remote cache entries or
        // session network approvals.
        let fingerprint = self.config.agents.web_context.semantic_fingerprint();
        if self.agents.web_context_service.is_some()
            && self.agents.web_context_config_fingerprint.as_deref() != Some(fingerprint.as_str())
        {
            self.agents.web_context_service = None;
            self.agents.web_context_config_fingerprint = None;
            self.agents.approval_policy = ApprovalPolicy::default();
        }
        if self.agents.web_context_service.is_none() {
            let mut config = self.config.agents.web_context.clone();
            if config.enabled
                && let Some(reference) = config.provider_secret_reference.take()
            {
                let reference =
                    crate::secrets::SecretReference::parse(&reference).map_err(|_| {
                        AgentError::HandlerError(String::from(
                            "invalid agents.web_context.provider_secret_reference",
                        ))
                    })?;
                let store = self.build_agents_secret_store().ok_or_else(|| {
                    AgentError::HandlerError(String::from(
                        "web search authorization unavailable: secrets store unavailable",
                    ))
                })?;
                let secret = store.get(reference.name()).map_err(|_| {
                    AgentError::HandlerError(String::from(
                        "web search authorization unavailable: provider secret could not be resolved",
                    ))
                })?;
                self.agents.resolved_secret_values.push(secret.to_string());
                config = config.with_search_authorization(secret);
            }
            if config.enabled
                && let Some(reference) = config.browser_run_api_token_reference.take()
            {
                let reference =
                    crate::secrets::SecretReference::parse(&reference).map_err(|_| {
                        AgentError::HandlerError(String::from(
                            "invalid agents.web_context.browser_run_api_token_reference",
                        ))
                    })?;
                let store = self.build_agents_secret_store().ok_or_else(|| {
                    AgentError::HandlerError(String::from(
                        "Browser Run authorization unavailable: secrets store unavailable",
                    ))
                })?;
                let secret = store.get(reference.name()).map_err(|_| {
                    AgentError::HandlerError(String::from(
                        "Browser Run authorization unavailable: API token could not be resolved",
                    ))
                })?;
                self.agents.resolved_secret_values.push(secret.to_string());
                config = config.with_browser_run_api_token(secret);
            }
            let limits = config.limits.clone();
            let transport = ee_agent_host::ReqwestWebTransport::new(&limits)
                .map_err(web_context_agent_error)?;
            let service = ee_agent_host::WebContextService::new(config, transport)
                .map_err(web_context_config_agent_error)?;
            self.agents.web_context_service = Some(Arc::new(service));
            self.agents.web_context_config_fingerprint = Some(fingerprint);
        }
        self.agents.web_context_service.clone().ok_or_else(|| {
            AgentError::HandlerError(String::from("web context service unavailable"))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_web_call(
        &mut self,
        route: ProxyRoute,
        network_session_id: String,
        requested_host: String,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        #[cfg(test)]
        WEB_DISPATCH_TEST_COUNT.fetch_add(1, Ordering::SeqCst);

        match call {
            WebApprovalCall::Search { query } => {
                let service = match self.web_context_service() {
                    Ok(service) => service,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let provider_label = service.search_provider_approval_label();
                let response = TokioBuilder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("web context runtime")
                    .block_on(service.search_with_approved_hosts_and_cancellation(
                        ee_agent_host::WebSearchRequest { query: query.clone() },
                        &approved_hosts,
                        &cancellation,
                    ));
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let source_url = service
                            .search_initial_host()
                            .map(|host| format!("https://{host}/"))
                            .unwrap_or_else(|_| String::from("https://search.invalid/"));
                        let retrieved_at = i64::try_from(response.provenance.retrieved_at_unix_ms)
                            .ok()
                            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                        let provenance = response.provenance.identity();
                        self.record_web_source(
                            &network_session_id,
                            "search",
                            source_url,
                            retrieved_at,
                            None,
                            0,
                            response.results.len(),
                            response.cached,
                            response.truncated,
                            provenance,
                        );
                        let _ = reply.send(
                            Self::web_search_value(query, response)
                                .map(ClientRequestResponse::ProxyValue),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            Some(provider_label),
                            WebApprovalCall::Search { query },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("configured search");
                        self.record_web_failure("search", host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
            WebApprovalCall::Fetch { url } => {
                let response = match self.web_context_service() {
                    Ok(service) => TokioBuilder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("web context runtime")
                        .block_on(service.fetch_with_approved_hosts_and_cancellation(
                            ee_agent_host::WebFetchRequest { url: url.clone() },
                            &approved_hosts,
                            &cancellation,
                        )),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let sha256 = sha256_hex(response.text.as_bytes());
                        let retrieved_at = i64::try_from(response.retrieved_at_unix_ms)
                            .ok()
                            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                        self.record_web_source(
                            &network_session_id,
                            "fetch",
                            response.final_url.clone(),
                            retrieved_at.clone(),
                            Some(sha256.clone()),
                            response.text.len(),
                            1,
                            response.cached,
                            response.truncated,
                            response.final_url.clone(),
                        );
                        let _ = reply.send(
                            Self::web_fetch_value(response, sha256, retrieved_at)
                                .map(ClientRequestResponse::ProxyValue),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            None,
                            WebApprovalCall::Fetch { url },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("requested host");
                        self.record_web_failure("fetch", host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
            WebApprovalCall::BrowserRun { request } => {
                let action = request.action.as_str();
                let response = match self.web_context_service() {
                    Ok(service) => TokioBuilder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("web context runtime")
                        .block_on(service.browser_run_with_approved_hosts_and_cancellation(
                            request.clone(),
                            &approved_hosts,
                            &cancellation,
                        )),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let byte_count =
                            serde_json::to_vec(&response.result).map_or(0, |result| result.len());
                        let retrieved_at = chrono::Utc::now().to_rfc3339();
                        self.record_web_source(
                            &network_session_id,
                            action,
                            response.requested_url.clone(),
                            retrieved_at,
                            None,
                            byte_count,
                            1,
                            false,
                            response.truncated,
                            String::from("cloudflare_browser_run"),
                        );
                        let _ = reply.send(
                            serde_json::to_value(response)
                                .map(ClientRequestResponse::ProxyValue)
                                .map_err(|error| AgentError::HandlerError(error.to_string())),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            Some("Cloudflare Browser Run"),
                            WebApprovalCall::BrowserRun { request },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("requested host");
                        self.record_web_failure(action, host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
        }
    }

    /// Retains one compact source record and lifecycle row without copying
    /// untrusted remote bytes or agent-supplied query text into local state.
    #[allow(clippy::too_many_arguments)]
    fn record_web_source(
        &mut self,
        network_session_id: &str,
        action: &str,
        url: String,
        retrieved_at: String,
        sha256: Option<String>,
        byte_count: usize,
        result_count: usize,
        cached: bool,
        truncated: bool,
        provenance: String,
    ) {
        let host = ee_agent_host::web_context::validate_https_url(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| String::from("unknown"));
        let session_id = network_session_id.to_owned();
        self.agents.action_log.push(ActionLogEntry::ExternalSource {
            action: action.to_owned(),
            host: host.clone(),
            url: url.clone(),
            retrieved_at: retrieved_at.clone(),
            sha256: sha256.clone(),
            byte_count,
            result_count,
            cached,
            truncated,
            provenance: provenance.clone(),
            session_id,
        });
        let lifecycle_id = NEXT_WEB_LIFECYCLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cache_state = if cached { "cached" } else { "fresh" };
        let detail = match action {
            "search" => format!(
                "kind: search · host: {host} · results: {result_count} · cache: {cache_state} · provenance: {provenance} · trust: untrusted external content"
            ),
            _ => format!(
                "kind: fetch · host: {host} · url: {url} · bytes: {byte_count} · cache: {cache_state} · SHA-256: {} · retrieved: {retrieved_at} · truncated: {truncated} · trust: untrusted external content",
                sha256.as_deref().unwrap_or("none")
            ),
        };
        self.record_web_lifecycle(
            &format!("web-{lifecycle_id}"),
            &format!("web/{action}"),
            "completed",
            &detail,
        );
    }

    fn web_search_value(
        query: String,
        response: ee_agent_host::WebSearchResponse,
    ) -> Result<serde_json::Value, AgentError> {
        let result = ee_mcp::WebSearchResult {
            query,
            results: response
                .results
                .into_iter()
                .map(|entry| ee_mcp::WebSearchEntry {
                    title: entry.title,
                    url: entry.url,
                    host: entry.host,
                    snippet: entry.snippet,
                    rank: entry.rank as u32,
                })
                .collect(),
            provenance: response.provenance.identity(),
            trust: String::from("untrusted_external_content"),
            cached: response.cached,
            truncated: response.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn web_fetch_value(
        response: ee_agent_host::WebFetchResponse,
        sha256: String,
        retrieved_at: String,
    ) -> Result<serde_json::Value, AgentError> {
        let result = ee_mcp::FetchUrlResult {
            requested_url: response.requested_url,
            url: response.final_url.clone(),
            title: response.title,
            content_type: response.content_type,
            sha256,
            text: response.text,
            retrieved_at,
            links: Vec::new(),
            provenance: response.final_url,
            trust: String::from("untrusted_external_content"),
            cached: response.cached,
            truncated: response.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_git_status(&self) -> Result<serde_json::Value, AgentError> {
        let report = self
            .proxy_git_repository()?
            .status(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git status failed: {error}")))?;
        let result = ee_mcp::GitStatusResult {
            repo_root: report.repo_root.display().to_string(),
            branch: report.branch,
            detached: report.detached,
            staged: report
                .staged
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            unstaged: report
                .unstaged
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            untracked: report
                .untracked
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            conflicts: report
                .conflicts
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            file_limit: u32::try_from(report.file_limit).unwrap_or(u32::MAX),
            returned_file_count: u32::try_from(report.returned_file_count).unwrap_or(u32::MAX),
            total_file_count: u32::try_from(report.total_file_count).unwrap_or(u32::MAX),
            omitted_file_count: u32::try_from(report.omitted_file_count).unwrap_or(u32::MAX),
            truncated: report.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_git_diff(&self) -> Result<serde_json::Value, AgentError> {
        let diff = self
            .proxy_git_repository()?
            .unstaged_diff(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git diff failed: {error}")))?;
        self.proxy_git_diff_value(diff)
    }

    fn proxy_git_diff_staged(&self) -> Result<serde_json::Value, AgentError> {
        let diff = self
            .proxy_git_repository()?
            .staged_diff(crate::git::GitReadLimits::default())
            .map_err(|error| {
            AgentError::HandlerError(format!("Git staged diff failed: {error}"))
        })?;
        self.proxy_git_diff_value(diff)
    }

    fn proxy_git_diff_file(&self, path: &Path) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let diff = self
            .proxy_git_repository()?
            .unstaged_diff_for_path(path, crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git file diff failed: {error}")))?;
        self.proxy_git_diff_value(diff)
    }

    fn proxy_git_diff_value(
        &self,
        diff: crate::git::GitDiff,
    ) -> Result<serde_json::Value, AgentError> {
        serde_json::to_value(ee_mcp::GitDiffResult {
            diff: diff.text,
            bytes_returned: u64::try_from(diff.bytes_returned).unwrap_or(u64::MAX),
            byte_limit: u64::try_from(diff.byte_limit).unwrap_or(u64::MAX),
            truncated: diff.truncated,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_changed_files_result(&self) -> Result<ee_mcp::ChangedFilesResult, AgentError> {
        let repository = self.proxy_git_repository()?;
        let report = repository
            .status(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git status failed: {error}")))?;
        let mut files = BTreeMap::<PathBuf, ee_mcp::ChangedFileEntry>::new();
        let mut insert_status =
            |path: &Path, staged: bool, unstaged: bool, untracked: bool, conflicted: bool| {
                let path = report.repo_root.join(path);
                let entry = files.entry(path.clone()).or_insert_with(|| ee_mcp::ChangedFileEntry {
                    path: path.display().to_string(),
                    staged: false,
                    unstaged: false,
                    untracked: false,
                    conflicted: false,
                    dirty: false,
                    saved: true,
                });
                entry.staged |= staged;
                entry.unstaged |= unstaged;
                entry.untracked |= untracked;
                entry.conflicted |= conflicted;
            };
        for path in &report.staged {
            insert_status(path, true, false, false, false);
        }
        for path in &report.unstaged {
            insert_status(path, false, true, false, false);
        }
        for path in &report.untracked {
            insert_status(path, false, false, true, false);
        }
        for path in &report.conflicts {
            insert_status(path, false, false, false, true);
        }
        for buffer in self.backend.all_bufs() {
            let Some(path) = &buffer.path else { continue };
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if !canonical.starts_with(repository.root()) {
                continue;
            }
            let entry =
                files.entry(canonical.clone()).or_insert_with(|| ee_mcp::ChangedFileEntry {
                    path: canonical.display().to_string(),
                    staged: false,
                    unstaged: false,
                    untracked: false,
                    conflicted: false,
                    dirty: false,
                    saved: true,
                });
            entry.dirty = !buffer.pristine;
            entry.saved = buffer_saved_state(buffer);
        }
        let file_limit = report.file_limit;
        let mut files = files.into_values().collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.retain(|entry| {
            entry.staged || entry.unstaged || entry.untracked || entry.conflicted || entry.dirty
        });
        let total_file_count = report.total_file_count.max(files.len());
        files.truncate(file_limit);
        let omitted_file_count = total_file_count.saturating_sub(files.len());
        Ok(ee_mcp::ChangedFilesResult {
            files,
            file_limit: u32::try_from(file_limit).unwrap_or(u32::MAX),
            total_file_count: u32::try_from(total_file_count).unwrap_or(u32::MAX),
            omitted_file_count: u32::try_from(omitted_file_count).unwrap_or(u32::MAX),
            truncated: report.truncated || omitted_file_count > 0,
        })
    }

    fn proxy_changed_files(&self) -> Result<serde_json::Value, AgentError> {
        serde_json::to_value(self.proxy_changed_files_result()?)
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(crate) fn proxy_review_context(&mut self) -> Result<serde_json::Value, AgentError> {
        let changed_files = self.proxy_changed_files_result()?;
        let changed_paths =
            changed_files.files.iter().map(|entry| entry.path.as_str()).collect::<BTreeSet<_>>();
        let mut diagnostics = self
            .proxy_diagnostic_entries(None)
            .into_iter()
            .filter(|entry| changed_paths.contains(entry.path.as_str()))
            .collect::<Vec<_>>();
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.range.start_line.cmp(&right.range.start_line))
                .then(left.range.start_character.cmp(&right.range.start_character))
        });
        let diagnostic_total = u32::try_from(diagnostics.len()).unwrap_or(u32::MAX);
        let diagnostics_truncated = diagnostics.len() > PROXY_DIAGNOSTICS_LIMIT;
        diagnostics.truncate(PROXY_DIAGNOSTICS_LIMIT);

        let mut nearby_symbols = Vec::new();
        let mut symbols_truncated = false;
        for entry in changed_files.files.iter().take(PROXY_REVIEW_SYMBOL_FILE_LIMIT) {
            if nearby_symbols.len() >= PROXY_REVIEW_SYMBOLS_LIMIT {
                symbols_truncated = true;
                break;
            }
            let path = Path::new(&entry.path);
            if self.buffer_id_for_path(path).is_none() {
                continue;
            }
            let payload = match self.proxy_document_symbols(path) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            let Ok(result) = serde_json::from_value::<ee_mcp::DocumentSymbolsResult>(payload)
            else {
                continue;
            };
            symbols_truncated |= result.truncated;
            nearby_symbols.extend(
                result
                    .symbols
                    .into_iter()
                    .take(PROXY_REVIEW_SYMBOLS_LIMIT.saturating_sub(nearby_symbols.len())),
            );
        }
        symbols_truncated |= changed_files.files.len() > PROXY_REVIEW_SYMBOL_FILE_LIMIT;

        let test_suggestions = self.proxy_git_repository().map_or_else(
            |_| Vec::new(),
            |repository| {
                if repository.root().join("Cargo.toml").is_file() {
                    vec![String::from("cargo test --quiet")]
                } else {
                    Vec::new()
                }
            },
        );
        serde_json::to_value(ee_mcp::ReviewContextResult {
            changed_files,
            diagnostics: ee_mcp::DiagnosticsResult {
                diagnostics,
                truncated: diagnostics_truncated,
                total: diagnostic_total,
            },
            nearby_symbols,
            symbols_truncated,
            // Only suggest a quiet Cargo validation when workspace shape proves it applies.
            test_suggestions,
        })
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
        route: super::agents_mcp::ProxyRoute,
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
                    title: String::from("ee_apply_code_action"),
                    detail,
                    prepared,
                };
                let mcp = self.mcp_invocation_for_tool(
                    "ee_apply_code_action",
                    json!({ "action_id": action_id, "path": path }),
                    route,
                );
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    mcp,
                    Some(PERSISTENT_TERMINAL_OPTION_LABEL),
                    reply,
                ));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn queue_proxy_format_file(
        &mut self,
        path: &str,
        route: super::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
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
                            title: String::from("ee_format_file"),
                            detail,
                            prepared,
                        };
                        let mcp = self.mcp_invocation_for_tool(
                            "ee_format_file",
                            json!({ "path": path }),
                            route,
                        );
                        self.request_bridge_approval(ApprovalPrompt::proxy_write(
                            spec,
                            mcp,
                            Some(PERSISTENT_TERMINAL_OPTION_LABEL),
                            reply,
                        ));
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
        route: super::agents_mcp::ProxyRoute,
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
                    let mcp = self.mcp_invocation_for_tool(
                        "ee_rename_symbol",
                        json!({ "character": character, "line": line, "new_name": new_name, "path": path }),
                        route,
                    );
                    self.request_bridge_approval(ApprovalPrompt::proxy_write_batch(
                        String::from("ee_rename_symbol"),
                        detail,
                        writes,
                        total_edits,
                        mcp,
                        Some(PERSISTENT_TERMINAL_OPTION_LABEL),
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

    fn proxy_symbol_dependency_map(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            AgentError::Io(format!("cannot resolve symbol-dependency path: {error}"))
        })?;
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params("path outside allowed workspace"));
        }
        let buffer_id = self.ensure_proxy_buffer(&canonical)?;
        let buffer =
            self.backend.all_bufs().iter().find(|buffer| buffer.id == buffer_id).ok_or_else(
                || AgentError::HandlerError("opened buffer is unavailable".to_string()),
            )?;
        let language_id = self.buffer_language_id(buffer).ok_or_else(|| {
            AgentError::HandlerError(
                "dependency_index_unavailable: buffer language is unavailable".to_string(),
            )
        })?;
        self.backend
            .symbol_dependency_map(
                buffer_id,
                canonical.display().to_string(),
                line,
                character,
                language_id,
            )
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn proxy_file_dependency_map(&self, path: &Path) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            AgentError::Io(format!("cannot resolve dependency-map path: {error}"))
        })?;
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params("path outside allowed workspace"));
        }
        serde_json::to_value(super::agent_knowledge::unavailable_dependency_map(canonical))
            .map_err(|error| AgentError::HandlerError(error.to_string()))
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

    // ── Approval policy (Phase 1 foundation) ────────────────────────────────
    // Session state lives in `crate::policy::session`; these tests pin the
    // once/session precedence contract the shared evaluator consumes.

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
    fn once_choices_are_never_recorded() {
        let policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        assert_eq!(session_decision(ApprovalChoice::AllowOnce), None);
        assert_eq!(session_decision(ApprovalChoice::DenyOnce), None);
        assert!(policy.lookup("s1", &fp).is_none(), "once decisions must not persist");
    }

    #[test]
    fn session_choices_map_to_shared_policy_state() {
        assert_eq!(session_decision(ApprovalChoice::AllowSession), Some(SessionChoice::Allow));
        assert_eq!(session_decision(ApprovalChoice::DenySession), Some(SessionChoice::Deny));
    }

    #[test]
    fn policy_session_allow_and_deny_are_scoped_and_invalidated() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, SessionChoice::Allow);
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Allow));
        // A different session is unaffected.
        assert!(policy.lookup("s2", &fp).is_none());

        policy.record("s1", &fp, SessionChoice::Deny);
        // Deny wins over allow for the same key.
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Deny));

        policy.invalidate_session("s1");
        assert!(policy.lookup("s1", &fp).is_none(), "policy dies with the session");
    }

    #[test]
    fn policy_deny_wins_over_allow_for_same_fingerprint() {
        let mut policy = ApprovalPolicy::default();
        let fp = approval_fingerprint(&write_kind("/work/a.txt"));
        policy.record("s1", &fp, SessionChoice::Allow);
        policy.record("s1", &fp, SessionChoice::Deny);
        assert_eq!(policy.lookup("s1", &fp), Some(SessionChoice::Deny));
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
    fn network_approval_exposes_only_route_and_canonical_host() {
        let query = "private search query";
        let url = "https://docs.example/private/path?token=super-secret";
        let (reply, _receiver) = oneshot::channel();
        let prompt = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("docs.example"),
            String::from("docs.example"),
            None,
            WebApprovalCall::Fetch { url: String::from(url) },
            BTreeSet::new(),
            CancellationToken::new(),
            reply,
        );
        let rendered = format!("{prompt:?}");
        assert_eq!(prompt.title, "network/fetch URL");
        assert_eq!(prompt.detail, "host: docs.example");
        assert!(!rendered.contains(query));
        assert!(!rendered.contains(url));
        assert!(!rendered.contains("super-secret"));
        assert_eq!(prompt.options.len(), 4);
        assert!(
            prompt.options.iter().all(|(_, choice)| *choice != ApprovalChoice::AllowPersistent)
        );
    }

    #[test]
    fn network_redirect_prompt_keeps_requested_host_but_scopes_grant_to_current_host() {
        let (reply, _receiver) = oneshot::channel();
        let prompt = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("origin.example"),
            String::from("redirect.example"),
            Some("Exa"),
            WebApprovalCall::Search { query: String::from("private") },
            BTreeSet::from([String::from("origin.example")]),
            CancellationToken::new(),
            reply,
        );
        assert_eq!(prompt.detail, "provider: Exa · host: redirect.example");
        let rendered = format!("{prompt:?}");
        assert!(!rendered.contains("private"));
        assert_eq!(
            approval_fingerprint(&prompt.kind),
            "network:stdio:ee --mcp-proxy:search:redirect.example"
        );
        match &prompt.kind {
            ApprovalKind::Network { requested_host, current_host, approved_hosts, .. } => {
                assert_eq!(requested_host, "origin.example");
                assert_eq!(current_host, "redirect.example");
                assert!(approved_hosts.contains("origin.example"));
                assert!(!approved_hosts.contains("redirect.example"));
            }
            _ => panic!("expected network prompt"),
        }
    }

    #[test]
    fn network_search_and_fetch_grants_are_scoped_to_their_actions() {
        let (search_reply, _search_receiver) = oneshot::channel();
        let search = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("api.exa.ai"),
            String::from("api.exa.ai"),
            Some("Exa"),
            WebApprovalCall::Search { query: String::from("private") },
            BTreeSet::new(),
            CancellationToken::new(),
            search_reply,
        );
        let (fetch_reply, _fetch_receiver) = oneshot::channel();
        let fetch = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("api.exa.ai"),
            String::from("api.exa.ai"),
            None,
            WebApprovalCall::Fetch { url: String::from("https://api.exa.ai/source") },
            BTreeSet::new(),
            CancellationToken::new(),
            fetch_reply,
        );

        assert_ne!(approval_fingerprint(&search.kind), approval_fingerprint(&fetch.kind));
    }

    #[test]
    fn network_session_fingerprints_are_route_and_connection_scoped() {
        let make_prompt = |route, scope: &str| {
            let (reply, _receiver) = oneshot::channel();
            ApprovalPrompt::web(
                route,
                format!("proxy-network:{}:{scope}", route.transport_identity()),
                String::from("docs.example"),
                String::from("docs.example"),
                Some("Exa"),
                WebApprovalCall::Search { query: String::from("must stay private") },
                BTreeSet::new(),
                CancellationToken::new(),
                reply,
            )
        };
        let stdio = make_prompt(ProxyRoute::Stdio, "connection-a");
        let second_stdio = make_prompt(ProxyRoute::Stdio, "connection-b");
        let acp = make_prompt(ProxyRoute::AcpNative, "connection-a");
        let stdio_fingerprint = approval_fingerprint(&stdio.kind);
        let acp_fingerprint = approval_fingerprint(&acp.kind);
        assert_ne!(stdio_fingerprint, acp_fingerprint);
        assert_ne!(stdio.session_id, second_stdio.session_id);
        assert!(!stdio_fingerprint.contains("must stay private"));
        assert!(!acp_fingerprint.contains("must stay private"));

        let mut policy = ApprovalPolicy::default();
        policy.record(&stdio.session_id, &stdio_fingerprint, SessionChoice::Allow);
        assert_eq!(
            policy.lookup(&acp.session_id, &acp_fingerprint),
            None,
            "stdio host decision must not apply to ACP-native route"
        );
        assert_eq!(
            policy.lookup(&second_stdio.session_id, &stdio_fingerprint),
            None,
            "one stdio connection must not reuse another connection's network grant"
        );
    }

    #[test]
    fn web_value_conversions_preserve_provenance_and_untrusted_markers() {
        let search = App::web_search_value(
            String::from("Rust MCP"),
            ee_agent_host::WebSearchResponse {
                results: vec![ee_agent_host::WebSearchResult {
                    title: String::from("Docs"),
                    url: String::from("https://docs.example/search"),
                    host: String::from("docs.example"),
                    snippet: String::from("MCP reference"),
                    rank: 1,
                }],
                provenance: ee_agent_host::WebSearchProvenance {
                    provider: ee_agent_host::web_context::WebSearchProvider::Exa,
                    adapter: String::from("v1"),
                    retrieved_at_unix_ms: 1,
                },
                truncated: false,
                cached: true,
            },
        )
        .expect("search response converts");
        assert_eq!(search["provenance"], "exa:v1");
        assert_eq!(search["trust"], "untrusted_external_content");
        assert_eq!(search["results"][0]["rank"], 1);
        assert_eq!(search["cached"], true);

        let fetch = App::web_fetch_value(
            ee_agent_host::WebFetchResponse {
                requested_url: String::from("https://docs.example/start"),
                final_url: String::from("https://docs.example/final"),
                title: Some(String::from("Docs")),
                content_type: String::from("text/html"),
                text: String::from("untrusted response"),
                retrieved_at_unix_ms: 1,
                truncated: true,
                redirects: 1,
                cached: false,
            },
            String::from("sha256"),
            String::from("2026-08-25T00:00:00Z"),
        )
        .expect("fetch response converts");
        assert_eq!(fetch["requestedUrl"], "https://docs.example/start");
        assert_eq!(fetch["url"], "https://docs.example/final");
        assert_eq!(fetch["provenance"], "https://docs.example/final");
        assert_eq!(fetch["trust"], "untrusted_external_content");
        assert_eq!(fetch["truncated"], true);
    }

    #[test]
    fn approval_options_offer_persistent_only_when_eligible() {
        // Ineligible prompts never get a persistent option; the option list
        // stays at four choices with no unlimited allow.
        let base = approval_options(None);
        assert_eq!(base.len(), 4);
        for (label, _) in &base {
            assert!(!label.contains("Always"), "allow-always must stay disabled: {label}");
            assert!(!label.contains("1 hour"));
        }
        // Eligible terminal prompts append the bounded persistent option.
        let persistent = approval_options(Some(PERSISTENT_TERMINAL_OPTION_LABEL));
        assert_eq!(persistent.len(), 5);
        assert_eq!(
            persistent.last().unwrap().0,
            PERSISTENT_TERMINAL_OPTION_LABEL,
            "persistent option label"
        );
        assert_eq!(persistent.last().unwrap().1, ApprovalChoice::AllowPersistent);
        assert!(ApprovalChoice::AllowPersistent.allows());
        // Persistent grants are host-local rules, never session decisions.
        assert_eq!(session_decision(ApprovalChoice::AllowPersistent), None);
        // Eligible bounded writes carry the write option label (phase 5).
        let writes = approval_options(Some(PERSISTENT_WRITE_OPTION_LABEL));
        assert_eq!(writes.len(), 5);
        assert_eq!(writes.last().unwrap().0, PERSISTENT_WRITE_OPTION_LABEL);
        assert_eq!(writes.last().unwrap().1, ApprovalChoice::AllowPersistent);
        assert_ne!(PERSISTENT_WRITE_OPTION_LABEL, PERSISTENT_TERMINAL_OPTION_LABEL);
    }
}
