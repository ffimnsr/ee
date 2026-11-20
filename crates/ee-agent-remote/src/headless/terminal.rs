//! Bounded terminal registry for the headless handler.
//!
//! One registry per remote host. Terminals are owned by the session that
//! created them: output, wait, kill, and release requests from any other
//! session fail closed. Output is bounded, truncated from the front at a
//! character boundary (the ACP rule), and redacted on read.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_host::AgentError;
use ee_agent_host::redact::redact_sensitive_text;
use ee_agent_protocol::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId,
};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

/// Default retained output bytes per terminal.
pub const DEFAULT_TERMINAL_OUTPUT_BYTES: usize = 256 * 1024;
/// Default bounded wait for `terminal/wait_for_exit`.
pub const DEFAULT_TERMINAL_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
/// Maximum command string length accepted from an agent.
pub const MAX_TERMINAL_COMMAND_CHARS: usize = 512;
/// Maximum argument count accepted from an agent.
pub const MAX_TERMINAL_ARGS: usize = 256;

/// Terminal registry tuning.
#[derive(Debug, Clone)]
pub struct TerminalOptions {
    /// Retained output bytes per terminal.
    pub max_output_bytes: usize,
    /// Deadline for `terminal/wait_for_exit`.
    pub wait_timeout: Duration,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            max_output_bytes: DEFAULT_TERMINAL_OUTPUT_BYTES,
            wait_timeout: DEFAULT_TERMINAL_WAIT_TIMEOUT,
        }
    }
}

#[derive(Debug, Default)]
struct OutputBuffer {
    text: String,
    truncated: bool,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &str, limit: usize) {
        self.text.push_str(chunk);
        if self.text.len() <= limit {
            return;
        }
        self.truncated = true;
        // Keep the newest `limit` bytes without splitting a character.
        let mut start = self.text.len() - limit;
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }
        self.text.drain(..start);
    }

    fn snapshot(&self) -> (String, bool) {
        (redact_sensitive_text(&self.text), self.truncated)
    }
}

struct TerminalEntry {
    session_id: String,
    output: Arc<Mutex<OutputBuffer>>,
    exit: watch::Receiver<Option<TerminalExitStatus>>,
    kill: Option<oneshot::Sender<()>>,
}

/// Shared terminal registry.
#[derive(Clone)]
pub struct TerminalRegistry {
    inner: Arc<Mutex<BTreeMap<String, TerminalEntry>>>,
    next_id: Arc<AtomicU64>,
    options: TerminalOptions,
}

impl std::fmt::Debug for TerminalRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalRegistry")
            .field("terminals", &self.inner.lock().expect("terminals poisoned").len())
            .finish_non_exhaustive()
    }
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self::new(TerminalOptions::default())
    }
}

impl TerminalRegistry {
    /// Creates an empty registry with explicit bounds.
    #[must_use]
    pub fn new(options: TerminalOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            options,
        }
    }

    fn next_terminal_id(&self) -> String {
        let count = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("term-{}-{count}", std::process::id())
    }

    /// Spawns one terminal after validating command shape. Containment and
    /// approval are the caller's responsibility.
    ///
    /// # Errors
    ///
    /// `InvalidParams` for an empty/oversized command, invalid env names, or
    /// spawn failure.
    pub fn create(
        &self,
        request: &CreateTerminalRequest,
        cwd: &std::path::Path,
    ) -> Result<CreateTerminalResponse, AgentError> {
        validate_command(&request.command, &request.args)?;
        let limit = request
            .output_byte_limit
            .map(|limit| limit as usize)
            .unwrap_or(self.options.max_output_bytes)
            .min(self.options.max_output_bytes);

        let mut command = Command::new(&request.command);
        command
            .args(&request.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for variable in &request.env {
            validate_env_name(&variable.name)?;
            command.env(&variable.name, &variable.value);
        }

        let mut child = command.spawn().map_err(|error| {
            AgentError::Io(format!("failed to spawn terminal command: {error}"))
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let output = Arc::new(Mutex::new(OutputBuffer::default()));
        if let Some(stdout) = stdout {
            tokio::spawn(pump(stdout, Arc::clone(&output), limit));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(pump(stderr, Arc::clone(&output), limit));
        }

        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, kill_rx) = oneshot::channel();
        tokio::spawn(wait_for_child(child, kill_rx, exit_tx));

        let terminal_id = self.next_terminal_id();
        self.inner.lock().expect("terminals poisoned").insert(
            terminal_id.clone(),
            TerminalEntry {
                session_id: request.session_id.0.to_string(),
                output,
                exit: exit_rx,
                kill: Some(kill_tx),
            },
        );
        Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
    }

    /// Current output and exit status for an owned terminal.
    ///
    /// # Errors
    ///
    /// `PermissionDenied` when the session does not own the terminal,
    /// `NotFound` for an unknown id.
    pub fn output(
        &self,
        terminal_id: &TerminalId,
        session_id: &str,
    ) -> Result<(String, bool, Option<TerminalExitStatus>), AgentError> {
        let (output, exit) = {
            let entries = self.inner.lock().expect("terminals poisoned");
            let entry = owned_entry(&entries, terminal_id, session_id)?;
            (Arc::clone(&entry.output), entry.exit.clone())
        };
        let (text, truncated) = output.lock().expect("terminal output poisoned").snapshot();
        Ok((text, truncated, exit.borrow().clone()))
    }

    /// Waits for the terminal to exit, bounded by [`TerminalOptions::wait_timeout`].
    ///
    /// # Errors
    ///
    /// Ownership/not-found errors, or `RequestTimeout` when the deadline
    /// elapses before the process exits.
    pub async fn wait(
        &self,
        terminal_id: &TerminalId,
        session_id: &str,
    ) -> Result<TerminalExitStatus, AgentError> {
        let mut exit = {
            let entries = self.inner.lock().expect("terminals poisoned");
            owned_entry(&entries, terminal_id, session_id)?.exit.clone()
        };
        if let Some(status) = exit.borrow().clone() {
            return Ok(status);
        }
        let wait = async {
            loop {
                if exit.changed().await.is_err() {
                    return Err(AgentError::ConnectionClosed { agent_id: String::new() });
                }
                if let Some(status) = exit.borrow().clone() {
                    return Ok(status);
                }
            }
        };
        tokio::time::timeout(self.options.wait_timeout, wait).await.map_err(|_| {
            AgentError::RequestTimeout { method: String::from("terminal/wait_for_exit") }
        })?
    }

    /// Kills an owned terminal. A second kill is a typed no-op returning
    /// `false`.
    ///
    /// # Errors
    ///
    /// Ownership/not-found errors.
    pub fn kill(&self, terminal_id: &TerminalId, session_id: &str) -> Result<bool, AgentError> {
        let mut entries = self.inner.lock().expect("terminals poisoned");
        let entry = entries.get_mut(&terminal_id.0.to_string());
        let entry = entry.ok_or_else(|| {
            AgentError::InvalidParams(format!("unknown terminal {}", terminal_id.0))
        })?;
        if entry.session_id != session_id {
            return Err(AgentError::PermissionDenied {
                reason: String::from("terminal belongs to another session"),
            });
        }
        let kill = entry.kill.take();
        match kill {
            Some(kill) => Ok(kill.send(()).is_ok()),
            None => Ok(false),
        }
    }

    /// Kills and forgets an owned terminal.
    ///
    /// # Errors
    ///
    /// Ownership errors. Releasing an unknown id is a typed no-op.
    pub fn release(&self, terminal_id: &TerminalId, session_id: &str) -> Result<(), AgentError> {
        let mut entries = self.inner.lock().expect("terminals poisoned");
        let Some(mut entry) = entries.remove(&terminal_id.0.to_string()) else {
            return Ok(());
        };
        if entry.session_id != session_id {
            let owner = entry.session_id.clone();
            entries.insert(terminal_id.0.to_string(), entry);
            return Err(AgentError::PermissionDenied {
                reason: format!("terminal belongs to session {owner}"),
            });
        }
        if let Some(kill) = entry.kill.take() {
            let _ = kill.send(());
        }
        Ok(())
    }

    /// Number of tracked terminals (tests and status).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("terminals poisoned").len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn owned_entry<'a>(
    entries: &'a BTreeMap<String, TerminalEntry>,
    terminal_id: &TerminalId,
    session_id: &str,
) -> Result<&'a TerminalEntry, AgentError> {
    let entry = entries
        .get(&terminal_id.0.to_string())
        .ok_or_else(|| AgentError::InvalidParams(format!("unknown terminal {}", terminal_id.0)))?;
    if entry.session_id != session_id {
        return Err(AgentError::PermissionDenied {
            reason: String::from("terminal belongs to another session"),
        });
    }
    Ok(entry)
}

fn validate_command(command: &str, args: &[String]) -> Result<(), AgentError> {
    if command.trim().is_empty() {
        return Err(AgentError::InvalidParams(String::from("terminal command must not be empty")));
    }
    if command.chars().count() > MAX_TERMINAL_COMMAND_CHARS {
        return Err(AgentError::InvalidParams(format!(
            "terminal command exceeds {MAX_TERMINAL_COMMAND_CHARS} characters"
        )));
    }
    if command.contains('\0') || args.iter().any(|arg| arg.contains('\0')) {
        return Err(AgentError::InvalidParams(String::from(
            "terminal command and arguments must not contain NUL",
        )));
    }
    if args.len() > MAX_TERMINAL_ARGS {
        return Err(AgentError::InvalidParams(format!(
            "terminal accepts at most {MAX_TERMINAL_ARGS} arguments"
        )));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), AgentError> {
    let valid = !name.is_empty()
        && !name.contains('=')
        && !name.contains('\0')
        && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_');
    if valid {
        Ok(())
    } else {
        Err(AgentError::InvalidParams(format!("invalid env variable name {name:?}")))
    }
}

async fn pump<R>(mut reader: R, output: Arc<Mutex<OutputBuffer>>, limit: usize)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let chunk = String::from_utf8_lossy(&buffer[..read]);
                output.lock().expect("terminal output poisoned").push(&chunk, limit);
            }
        }
    }
}

async fn wait_for_child(
    mut child: Child,
    kill: oneshot::Receiver<()>,
    exit: watch::Sender<Option<TerminalExitStatus>>,
) {
    let status = tokio::select! {
        status = child.wait() => status.ok(),
        _ = kill => {
            let _ = child.kill().await;
            child.wait().await.ok()
        }
    };
    let _ = exit.send(Some(exit_status(status)));
}

fn exit_status(status: Option<std::process::ExitStatus>) -> TerminalExitStatus {
    let Some(status) = status else {
        return TerminalExitStatus::new();
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return TerminalExitStatus::new().signal(signal.to_string());
        }
    }
    TerminalExitStatus::new().exit_code(status.code().and_then(|code| u32::try_from(code).ok()))
}
