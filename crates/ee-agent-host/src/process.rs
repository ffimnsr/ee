//! Agent subprocess lifecycle: spawn with explicit `command`, `args`, `env`,
//! and `cwd`, and capture stderr into a bounded ring buffer.
//!
//! Agent stdout is reserved for ACP JSON-RPC and is handed to the SDK
//! transport; stderr is never parsed as protocol.  Every line is truncated
//! to [`STDERR_MAX_LINE_BYTES`] and only the newest [`STDERR_MAX_LINES`]
//! lines are retained.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::AgentError;
use crate::events::AgentEvent;

/// Maximum number of stderr lines retained per agent process.
pub const STDERR_MAX_LINES: usize = 256;
/// Maximum bytes retained per stderr line (long lines are truncated).
pub const STDERR_MAX_LINE_BYTES: usize = 4096;
/// Timeout for the stderr reader task to drain after process kill.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How to launch one agent subprocess (mirrors the resolved config model in
/// `ee-cli`; the host owns the launch, not the config file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcessConfig {
    /// Executable to spawn.
    pub command: String,
    /// Explicit argument list (never a shell string).
    pub args: Vec<String>,
    /// Additional environment variables layered over the parent env.
    pub env: std::collections::BTreeMap<String, String>,
    /// Working directory; defaults to the host's current directory.
    pub cwd: Option<PathBuf>,
}

impl AgentProcessConfig {
    /// Builds a config with the required command.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        }
    }

    /// Validates launch invariants: non-empty command, explicit args, and an
    /// absolute `cwd` when present.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.command.trim().is_empty() {
            return Err(AgentError::invalid_params("agent command must not be empty"));
        }
        if let Some(cwd) = &self.cwd
            && !cwd.is_absolute()
        {
            return Err(AgentError::invalid_params(format!(
                "agent cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        Ok(())
    }
}

/// Bounded stderr capture for one agent process.
#[derive(Debug, Default)]
pub struct StderrCapture {
    lines: VecDeque<String>,
    total_lines: usize,
}

impl StderrCapture {
    /// Creates an empty capture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one line, enforcing the line/byte caps.
    pub fn push(&mut self, line: String) {
        let mut line = line;
        if line.len() > STDERR_MAX_LINE_BYTES {
            // Leave room for the ellipsis so the retained line stays within
            // the byte cap.
            let mut boundary = STDERR_MAX_LINE_BYTES.saturating_sub(4);
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            line.truncate(boundary);
            line.push('…');
        }
        self.lines.push_back(line);
        self.total_lines += 1;
        while self.lines.len() > STDERR_MAX_LINES {
            self.lines.pop_front();
        }
    }

    /// Number of lines ever received (before capping).
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.total_lines
    }

    /// The retained stderr lines, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// A running agent subprocess plus its stderr capture.
///
/// The connection takes the stdout/stderr pipes before building the SDK
/// transport; dropping this struct kills the child (`kill_on_drop`).
pub(crate) struct AgentProcess {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    stderr_state: Arc<Mutex<StderrCapture>>,
}

impl AgentProcess {
    /// Spawns the subprocess with piped stdio and `kill_on_drop` so a
    /// dropped connection never orphans the agent.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::SpawnFailed`] when the executable cannot be
    /// started.
    pub async fn spawn(config: &AgentProcessConfig) -> Result<Self, AgentError> {
        config.validate()?;
        let mut command = tokio::process::Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|error| AgentError::SpawnFailed {
            agent_id: config.command.clone(),
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| AgentError::SpawnFailed {
            agent_id: config.command.clone(),
            message: "agent stdin was not piped".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| AgentError::SpawnFailed {
            agent_id: config.command.clone(),
            message: "agent stdout was not piped".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| AgentError::SpawnFailed {
            agent_id: config.command.clone(),
            message: "agent stderr was not piped".into(),
        })?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            stderr_state: Arc::new(Mutex::new(StderrCapture::new())),
        })
    }

    /// Takes the stdin pipe (the host writes ACP JSON-RPC into it).
    pub fn take_stdin(&mut self) -> tokio::process::ChildStdin {
        self.stdin.take().expect("agent stdin taken exactly once")
    }

    /// Takes the stdout pipe (ACP JSON-RPC) for the transport.
    pub fn take_stdout(&mut self) -> tokio::process::ChildStdout {
        self.stdout.take().expect("agent stdout taken exactly once")
    }

    /// Takes the stderr pipe for the diagnostics reader.
    pub fn take_stderr(&mut self) -> tokio::process::ChildStderr {
        self.stderr.take().expect("agent stderr taken exactly once")
    }

    /// Shared stderr capture state for the diagnostics reader.
    pub fn stderr_state(&self) -> Arc<Mutex<StderrCapture>> {
        self.stderr_state.clone()
    }

    /// The retained stderr diagnostics.
    pub fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_state.lock().expect("stderr capture poisoned").snapshot()
    }

    /// Kills the process (best effort) and waits briefly for it to exit.
    pub async fn kill(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, self.child.wait()).await;
    }

    /// The child process handle, for status queries and `wait`.
    pub fn child(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }
}

/// Spawns the stderr diagnostics reader: captures into the bounded buffer
/// and forwards every line as an [`AgentEvent::StderrLine`].  The reader
/// exits when the pipe closes (process exit or kill).
pub(crate) fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    state: Arc<Mutex<StderrCapture>>,
    agent_id: String,
    events: UnboundedSender<AgentEvent>,
) {
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        loop {
            let line = match reader.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(agent_id, ?error, "agent stderr read failed");
                    break;
                }
            };
            state.lock().expect("stderr capture poisoned").push(line.clone());
            let _ = events.send(AgentEvent::StderrLine { agent_id: agent_id.clone(), line });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_bounded_by_lines_and_bytes() {
        let mut capture = StderrCapture::new();
        for i in 0..(STDERR_MAX_LINES + 50) {
            capture.push(format!("line {i}"));
        }
        let snapshot = capture.snapshot();
        assert_eq!(snapshot.len(), STDERR_MAX_LINES);
        assert_eq!(snapshot.first().map(String::as_str), Some("line 50"));
        assert_eq!(snapshot.last().map(String::as_str), Some("line 305"));
        assert_eq!(capture.total_lines(), STDERR_MAX_LINES + 50);
    }

    #[test]
    fn capture_truncates_long_lines_at_char_boundaries() {
        let mut capture = StderrCapture::new();
        let long = "é".repeat(STDERR_MAX_LINE_BYTES * 2);
        capture.push(long);
        let snapshot = capture.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].len() <= STDERR_MAX_LINE_BYTES);
        assert!(snapshot[0].ends_with('…'));
    }

    #[test]
    fn process_config_validates_command_and_cwd() {
        assert!(AgentProcessConfig::new("agent").validate().is_ok());
        assert!(AgentProcessConfig::new("  ").validate().is_err());
        assert!(AgentProcessConfig::new("a").validate().is_ok());

        let mut relative = AgentProcessConfig::new("agent");
        relative.cwd = Some(PathBuf::from("relative/dir"));
        assert!(relative.validate().is_err());

        let mut absolute = AgentProcessConfig::new("agent");
        absolute.cwd = Some(PathBuf::from("/tmp/work"));
        assert!(absolute.validate().is_ok());
    }
}
