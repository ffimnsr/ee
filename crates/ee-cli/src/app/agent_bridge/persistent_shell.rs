//! Persistent shell sessions for agent terminals (Unix).
//!
//! One shell is reused per `(session, cwd)` across `terminal/create` calls so
//! `cd`, `export`, and shell functions survive between commands. Each command
//! gets a unique end marker that carries its exit code; the reader routes all
//! output until the marker to that terminal's bounded buffer. Shells live until
//! the agent session ends; `terminal/release` only detaches a finished
//! terminal, and a still-running terminal is killed because a persistent shell
//! can only run one foreground command at a time.
//!
//! This module is Unix-only. Windows keeps the per-command process path in
//! `terminal.rs`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ee_agent_host::AgentError;
use ee_agent_protocol::{CreateTerminalRequest, TerminalExitStatus};

use super::terminal::{BoundedOutput, TerminalOutputStream};

/// Maximum number of reusable shells for one `(session, cwd)` key.
const MAX_SHELLS_PER_KEY: usize = 4;
/// Bytes each shell reader pulls per read.
const SHELL_READ_CHUNK: usize = 4096;
/// Marker prefix; the full marker includes a unique command id and nonce.
const MARKER_PREFIX: &str = "__EE_PTY_";

/// Shared bounded output buffer for one command.
type ShellOutput = Arc<Mutex<BoundedOutput>>;
/// One submitted command and its output buffer.
type SubmittedCommand = (PersistentCommandHandle, ShellOutput);

static NEXT_COMMAND_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

/// `(session, requested cwd)` identity for one persistent shell.
///
/// A request without an explicit cwd keeps its own bucket so shell `cd` state
/// survives between tool calls; an explicit cwd selects a separate shell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShellKey {
    session_id: String,
    cwd: Option<PathBuf>,
}

struct ActiveCommand {
    id: u64,
    marker: Vec<u8>,
    output: Arc<Mutex<BoundedOutput>>,
}

struct ShellCore {
    active: Option<ActiveCommand>,
    completed: HashMap<u64, TerminalExitStatus>,
    alive: bool,
    /// Bytes held back because they may be the start of the active marker.
    pending: Vec<u8>,
}

impl ShellCore {
    fn new() -> Self {
        Self { active: None, completed: HashMap::new(), alive: true, pending: Vec::new() }
    }
}

/// One reusable shell process.
pub(crate) struct PersistentShell {
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    core: Arc<Mutex<ShellCore>>,
    readers: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for PersistentShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentShell").field("pid", &self.pid).finish_non_exhaustive()
    }
}

impl PersistentShell {
    #[cfg(unix)]
    fn spawn(cwd: &Path, env: &[(String, String)]) -> Result<Arc<Self>, AgentError> {
        use std::os::unix::process::CommandExt;

        let shell = std::env::var("SHELL").ok().filter(|shell| {
            let path = Path::new(shell);
            !shell.is_empty() && (!path.is_absolute() || path.is_file())
        });
        let mut command = Command::new(shell.as_deref().unwrap_or("sh"));
        command
            .current_dir(cwd)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so kill stops the whole command tree.
            .process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| AgentError::Io(format!("terminal spawn failed: {error}")))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Io(String::from("persistent terminal stdin unavailable")))?;
        let core = Arc::new(Mutex::new(ShellCore::new()));
        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            let core = core.clone();
            readers.push(
                thread::Builder::new()
                    .name(String::from("ee-agent-shell-stdout"))
                    .spawn(move || read_stdout(stdout, core))
                    .map_err(|error| {
                        AgentError::Io(format!("terminal reader spawn failed: {error}"))
                    })?,
            );
        }
        if let Some(stderr) = stderr {
            let core = core.clone();
            readers.push(
                thread::Builder::new()
                    .name(String::from("ee-agent-shell-stderr"))
                    .spawn(move || read_stderr(stderr, core))
                    .map_err(|error| {
                        AgentError::Io(format!("terminal reader spawn failed: {error}"))
                    })?,
            );
        }
        Ok(Arc::new(Self {
            pid,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            core,
            readers: Mutex::new(readers),
        }))
    }

    fn is_idle(&self) -> bool {
        let core = self.core.lock().expect("persistent shell poisoned");
        core.alive && core.active.is_none()
    }

    fn is_alive(&self) -> bool {
        self.core.lock().expect("persistent shell poisoned").alive
    }

    /// Submits one command line and returns its id, output buffer, and exit
    /// polling handle.
    fn submit(
        self: &Arc<Self>,
        command_line: &str,
        cwd: Option<&Path>,
        env: &[(String, String)],
        cap: usize,
    ) -> Result<PersistentCommandHandle, AgentError> {
        let id = NEXT_COMMAND_ID.fetch_add(1, Ordering::Relaxed);
        let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
        // The leading newline belongs to the marker so it is stripped instead
        // of becoming a spurious trailing newline in the command output.
        let marker = format!("\n{MARKER_PREFIX}{id}_{nonce}__");
        let output = Arc::new(Mutex::new(BoundedOutput::new(cap)));
        {
            let mut core = self.core.lock().expect("persistent shell poisoned");
            if !core.alive {
                return Err(AgentError::Io(String::from("persistent terminal is closed")));
            }
            if core.active.is_some() {
                return Err(AgentError::invalid_params("persistent terminal is busy"));
            }
            core.pending.clear();
            core.active = Some(ActiveCommand {
                id,
                marker: marker.clone().into_bytes(),
                output: output.clone(),
            });
        }

        let mut script = String::new();
        for (name, value) in env {
            script.push_str("export ");
            script.push_str(name);
            script.push('=');
            script.push_str(&crate::terminal::shell_quote(value));
            script.push('\n');
        }
        if let Some(cwd) = cwd {
            script.push_str("cd ");
            script.push_str(&crate::terminal::shell_quote(cwd.to_string_lossy().as_ref()));
            script.push('\n');
        }
        script.push_str(command_line);
        script.push('\n');
        script.push_str("__ee_status=$?\n");
        script.push_str("printf '");
        script.push_str(&marker);
        script.push_str("%s\\n' \"$__ee_status\"\n");

        let write = self
            .stdin
            .lock()
            .expect("persistent shell stdin poisoned")
            .write_all(script.as_bytes());
        if let Err(error) = write {
            let mut core = self.core.lock().expect("persistent shell poisoned");
            core.active = None;
            core.alive = false;
            return Err(AgentError::Io(format!("terminal write failed: {error}")));
        }
        Ok(PersistentCommandHandle { shell: self.clone(), command_id: id, output })
    }

    fn exit_status(&self, command_id: u64) -> Option<TerminalExitStatus> {
        let core = self.core.lock().expect("persistent shell poisoned");
        if core.active.as_ref().is_some_and(|active| active.id == command_id) {
            return None;
        }
        core.completed.get(&command_id).cloned()
    }

    /// Kills the shell's whole process group. Any active command is reported
    /// as signalled.
    fn kill(&self) {
        #[cfg(unix)]
        unsafe {
            // Safe: the pid is this shell's process-group leader; failure only
            // means the group already exited.
            libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
        let mut child = self.child.lock().expect("persistent shell child poisoned");
        let _ = child.kill();
        let _ = child.wait();
        let mut core = self.core.lock().expect("persistent shell poisoned");
        core.alive = false;
        if let Some(active) = core.active.take() {
            core.completed
                .insert(active.id, TerminalExitStatus::new().signal(Some(String::from("9"))));
        }
    }

    fn join_readers(&self) {
        for reader in self.readers.lock().expect("shell readers poisoned").drain(..) {
            let _ = reader.join();
        }
    }
}

/// One submitted command bound to its shell.
#[derive(Debug, Clone)]
pub(crate) struct PersistentCommandHandle {
    shell: Arc<PersistentShell>,
    command_id: u64,
    output: Arc<Mutex<BoundedOutput>>,
}

impl PersistentCommandHandle {
    /// Exit status once the shell printed the end marker.
    #[must_use]
    pub(crate) fn exit_status(&self) -> Option<TerminalExitStatus> {
        self.shell.exit_status(self.command_id)
    }

    /// Whether the command is still running.
    #[must_use]
    pub(crate) fn is_running(&self) -> bool {
        self.exit_status().is_none()
    }

    /// The bounded output buffer for this command.
    #[must_use]
    pub(crate) fn output(&self) -> Arc<Mutex<BoundedOutput>> {
        self.output.clone()
    }

    /// Kills the shared shell (and therefore this command).
    pub(crate) fn kill(&self) {
        self.shell.kill();
    }
}

/// Pool of reusable shells keyed by `(session, cwd)`.
#[derive(Debug, Default)]
pub(crate) struct PersistentShellPool {
    shells: Mutex<HashMap<ShellKey, Vec<Arc<PersistentShell>>>>,
}

impl PersistentShellPool {
    /// Submits `request` to an idle shell for its `(session, cwd)` key,
    /// creating a shell when none is idle. Returns `None` when the key already
    /// has [`MAX_SHELLS_PER_KEY`] busy shells, letting the caller fall back to
    /// a per-command process.
    pub(crate) fn submit(
        &self,
        request: &CreateTerminalRequest,
        command_line: &str,
        cap: usize,
    ) -> Result<Option<SubmittedCommand>, AgentError> {
        let spawn_cwd = match request.cwd.clone() {
            Some(cwd) => cwd,
            None => std::env::current_dir().map_err(|error| {
                AgentError::Io(format!("terminal working directory unavailable: {error}"))
            })?,
        };
        let key =
            ShellKey { session_id: request.session_id.0.to_string(), cwd: request.cwd.clone() };
        let env: Vec<(String, String)> = std::env::vars()
            .filter(|(name, _)| !super::terminal::is_secret_env_key(name))
            .chain(
                request.env.iter().map(|variable| (variable.name.clone(), variable.value.clone())),
            )
            .collect();

        let mut shells = self.shells.lock().expect("persistent shell pool poisoned");
        let entries = shells.entry(key).or_default();
        // Drop dead shells and reuse an idle one.
        entries.retain(|shell| shell.is_alive());
        if let Some(shell) = entries.iter().find(|shell| shell.is_idle()) {
            return submit_to(shell, command_line, request.cwd.as_deref(), &env, cap);
        }
        if entries.len() >= MAX_SHELLS_PER_KEY {
            return Ok(None);
        }
        let shell = PersistentShell::spawn(&spawn_cwd, &env)?;
        entries.push(shell.clone());
        submit_to(&shell, command_line, request.cwd.as_deref(), &env, cap)
    }

    /// Kills every pooled shell (agent session shutdown).
    pub(crate) fn kill_all(&self) {
        let shells =
            std::mem::take(&mut *self.shells.lock().expect("persistent shell pool poisoned"));
        for shell in shells.into_values().flatten() {
            shell.kill();
            shell.join_readers();
        }
    }
}

fn submit_to(
    shell: &Arc<PersistentShell>,
    command_line: &str,
    cwd: Option<&Path>,
    env: &[(String, String)],
    cap: usize,
) -> Result<Option<SubmittedCommand>, AgentError> {
    let handle = shell.submit(command_line, cwd, env, cap)?;
    let output = handle.output();
    Ok(Some((handle, output)))
}

fn read_stdout(mut stream: impl Read, core: Arc<Mutex<ShellCore>>) {
    let mut buffer = [0u8; SHELL_READ_CHUNK];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => consume_stdout(&core, &buffer[..read]),
        }
    }
    let mut core = core.lock().expect("persistent shell poisoned");
    core.alive = false;
    if let Some(active) = core.active.take() {
        core.completed.insert(active.id, TerminalExitStatus::new().signal(Some(String::from("9"))));
    }
}

fn consume_stdout(core: &Arc<Mutex<ShellCore>>, bytes: &[u8]) {
    let mut core = core.lock().expect("persistent shell poisoned");
    core.pending.extend_from_slice(bytes);
    loop {
        let Some((id, marker, output)) = core
            .active
            .as_ref()
            .map(|active| (active.id, active.marker.clone(), active.output.clone()))
        else {
            core.pending.clear();
            return;
        };
        let Some(marker_start) = find_bytes(&core.pending, &marker) else {
            // Keep a possible partial marker prefix at the tail.
            let keep = longest_suffix_prefix(&core.pending, &marker);
            let flush = core.pending.len().saturating_sub(keep);
            if flush > 0 {
                let chunk: Vec<u8> = core.pending.drain(..flush).collect();
                output.lock().expect("output poisoned").push(TerminalOutputStream::Stdout, &chunk);
            }
            return;
        };
        if marker_start > 0 {
            let chunk: Vec<u8> = core.pending.drain(..marker_start).collect();
            output.lock().expect("output poisoned").push(TerminalOutputStream::Stdout, &chunk);
        }
        // Drop the marker bytes themselves.
        core.pending.drain(..marker.len());
        // Exit code digits followed by '\n', possibly split across reads.
        let Some(newline) = core.pending.iter().position(|byte| *byte == b'\n') else {
            return;
        };
        let digits: Vec<u8> = core.pending.drain(..newline).collect();
        core.pending.drain(..1);
        let code = std::str::from_utf8(&digits).ok().and_then(|text| text.parse::<u32>().ok());
        core.active = None;
        core.completed.insert(id, TerminalExitStatus::new().exit_code(code));
    }
}

fn read_stderr(mut stream: impl Read, core: Arc<Mutex<ShellCore>>) {
    let mut buffer = [0u8; SHELL_READ_CHUNK];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let core = core.lock().expect("persistent shell poisoned");
                if let Some(active) = core.active.as_ref() {
                    active
                        .output
                        .lock()
                        .expect("output poisoned")
                        .push(TerminalOutputStream::Stderr, &buffer[..read]);
                }
            }
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Length of the longest suffix of `bytes` that is a prefix of `marker`.
fn longest_suffix_prefix(bytes: &[u8], marker: &[u8]) -> usize {
    let max = bytes.len().min(marker.len().saturating_sub(1));
    (1..=max).rev().find(|&len| bytes[bytes.len() - len..] == marker[..len]).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{EnvVariable, SessionId};

    fn request(session: &str, cwd: Option<PathBuf>) -> CreateTerminalRequest {
        let mut request = CreateTerminalRequest::new(SessionId::new(session), "true");
        request.cwd = cwd;
        request.env = Vec::new();
        request
    }

    fn wait_for(handle: &PersistentCommandHandle) -> TerminalExitStatus {
        for _ in 0..200 {
            if let Some(status) = handle.exit_status() {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("command never completed");
    }

    #[test]
    fn shell_keeps_cwd_across_commands_without_an_explicit_cwd() {
        let pool = PersistentShellPool::default();
        let start = std::env::current_dir().expect("cwd");
        let parent = start.parent().expect("parent cwd").to_path_buf();
        let request = request("s1", None);
        let first = pool.submit(&request, "cd ..", 1024).expect("submit").expect("shell available");
        let _ = wait_for(&first.0);
        let second = pool.submit(&request, "pwd", 1024).expect("submit").expect("shell available");
        let status = wait_for(&second.0);
        assert_eq!(status.exit_code, Some(0));
        let output = second.1.lock().expect("output").as_string();
        assert_eq!(output.trim(), parent.display().to_string(), "cwd persisted: {output:?}");
    }

    #[test]
    fn shell_captures_exit_code_and_output() {
        let pool = PersistentShellPool::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let request = request("s2", Some(dir.path().to_path_buf()));
        let (handle, output) = pool
            .submit(&request, "printf marker-output; sh -c 'exit 7'", 1024)
            .expect("submit")
            .expect("shell available");
        let status = wait_for(&handle);
        assert_eq!(status.exit_code, Some(7));
        assert!(output.lock().expect("output").as_string().contains("marker-output"));
    }

    #[test]
    fn env_is_exported_for_later_commands() {
        let pool = PersistentShellPool::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut request = request("s3", Some(dir.path().to_path_buf()));
        request.env = vec![EnvVariable::new("EE_PTY_TEST", "yes")];
        let (handle, _) = pool
            .submit(&request, "test \"$EE_PTY_TEST\" = yes", 1024)
            .expect("submit")
            .expect("shell available");
        assert_eq!(wait_for(&handle).exit_code, Some(0));
    }

    #[test]
    fn kill_reports_signal_and_stops_command() {
        let pool = PersistentShellPool::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let request = request("s4", Some(dir.path().to_path_buf()));
        let (handle, _) =
            pool.submit(&request, "sleep 30", 1024).expect("submit").expect("shell available");
        handle.kill();
        let status = wait_for(&handle);
        assert_eq!(status.signal.as_deref(), Some("9"));
    }

    #[test]
    fn pool_is_bounded_per_key() {
        let pool = PersistentShellPool::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let request = request("s5", Some(dir.path().to_path_buf()));
        let mut handles = Vec::new();
        for _ in 0..MAX_SHELLS_PER_KEY {
            let (handle, _) =
                pool.submit(&request, "sleep 5", 1024).expect("submit").expect("shell available");
            handles.push(handle);
        }
        assert!(pool.submit(&request, "true", 1024).expect("submit").is_none());
        for handle in handles {
            handle.kill();
        }
    }
}
