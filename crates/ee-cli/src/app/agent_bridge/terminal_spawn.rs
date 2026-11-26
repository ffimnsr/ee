//! Terminal process spawning: plain and OS-sandboxed.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use ee_agent_host::AgentError;
use ee_agent_protocol::CreateTerminalRequest;
use ee_sandbox::{ProcessLimits, ProcessTreeMonitor, SandboxSpec};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use super::terminal::{
    BoundedOutput, TERMINAL_READER_CHUNK, TerminalOutputStream, terminal_child_env,
};

/// One per-command process plus its bounded output and reader threads.
pub(super) type TerminalProcessSpawn =
    (Arc<Mutex<BoundedOutput>>, Vec<thread::JoinHandle<()>>, Child);

pub(super) fn spawn_terminal_process(
    request: &CreateTerminalRequest,
    command_line: &str,
    cap: usize,
) -> Result<TerminalProcessSpawn, AgentError> {
    let cwd = request.cwd.as_deref().unwrap_or_else(|| Path::new("."));
    let mut command = crate::terminal::shell_command(command_line, cwd);
    command.envs(terminal_child_env(request));
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| AgentError::Io(format!("terminal spawn failed: {error}")))?;
    let output = Arc::new(Mutex::new(BoundedOutput::new(cap)));
    let readers = [
        spawn_output_reader(child.stdout.take(), Arc::clone(&output), TerminalOutputStream::Stdout),
        spawn_output_reader(child.stderr.take(), Arc::clone(&output), TerminalOutputStream::Stderr),
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok((output, readers, child))
}

pub(super) fn spawn_sandboxed_terminal_process(
    request: &CreateTerminalRequest,
    command_line: &str,
    cap: usize,
    spec: &SandboxSpec,
) -> Result<TerminalProcessSpawn, AgentError> {
    let probe = ee_sandbox::cached_probe();
    if !probe.available {
        return Err(AgentError::Io(format!("terminal sandbox unavailable: {}", probe.summary())));
    }
    let program = probe
        .program
        .clone()
        .ok_or_else(|| AgentError::Io(String::from("terminal sandbox backend path is missing")))?;
    let env = terminal_child_env(request);

    #[cfg(target_os = "linux")]
    let sandbox = ee_sandbox::bwrap_command(&program, spec, command_line, &env)
        .map_err(|error| AgentError::Io(format!("terminal sandbox setup failed: {error}")))?;
    #[cfg(target_os = "macos")]
    let sandbox = {
        let temp = sandbox_temp_dir()?;
        ee_sandbox::seatbelt_command(&program, spec, command_line, &env, &temp)
            .map_err(|error| AgentError::Io(format!("terminal sandbox setup failed: {error}")))?
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let sandbox = return Err(AgentError::Io(String::from(
        "terminal sandbox is supported only on Linux and macOS",
    )));

    let cwd = request.cwd.as_deref().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(&sandbox.program);
    command.args(&sandbox.args);
    command.env_clear();
    if !sandbox.env.is_empty() {
        command.envs(sandbox.env.iter().map(|(name, value)| (name, value)));
    }
    command.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let limits = spec.limits;
    apply_process_limits(&mut command, limits);
    let mut child = command
        .spawn()
        .map_err(|error| AgentError::Io(format!("sandboxed terminal spawn failed: {error}")))?;
    let output = Arc::new(Mutex::new(BoundedOutput::new(cap)));
    let mut readers = [
        spawn_output_reader(child.stdout.take(), Arc::clone(&output), TerminalOutputStream::Stdout),
        spawn_output_reader(child.stderr.take(), Arc::clone(&output), TerminalOutputStream::Stderr),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if limits.is_enabled() {
        readers.push(spawn_limit_monitor(child.id(), limits));
    }
    Ok((output, readers, child))
}

#[cfg(unix)]
fn apply_process_limits(command: &mut Command, limits: ProcessLimits) {
    if !limits.is_enabled() {
        return;
    }
    command.process_group(0);
    unsafe {
        let _ = command.pre_exec(move || {
            let set = |resource: libc::c_int, value: u64| {
                let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
                // Explicitly configured limits are best-effort at exec time; a
                // kernel refusal surfaces as a normal spawn failure.
                let _ = libc::setrlimit(resource as _, &limit);
            };
            if let Some(value) = limits.max_cpu_seconds {
                set(libc::RLIMIT_CPU as _, value);
            }
            if let Some(value) = limits.max_rss_bytes {
                set(libc::RLIMIT_AS as _, value);
            }
            if let Some(value) = limits.max_open_files {
                set(libc::RLIMIT_NOFILE as _, value);
            }
            if let Some(value) = limits.max_file_bytes {
                set(libc::RLIMIT_FSIZE as _, value);
            }
            if let Some(value) = limits.max_processes {
                set(libc::RLIMIT_NPROC as _, value);
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_process_limits(_command: &mut Command, _limits: ProcessLimits) {}

fn spawn_limit_monitor(root_pid: u32, limits: ProcessLimits) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name(String::from("ee-agent-terminal-limits"))
        .spawn(move || {
            let monitor = ProcessTreeMonitor::started_now(limits);
            let hard_deadline = limits.wall_seconds.map(|seconds| seconds.saturating_add(60));
            let started = std::time::Instant::now();
            loop {
                if let Some(breach) = monitor.observe(root_pid) {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(root_pid as i32), libc::SIGKILL);
                    }
                    tracing::warn!(pid = root_pid, %breach, "terminal resource limit breached");
                    return;
                }
                if !process_alive(root_pid) {
                    return;
                }
                if hard_deadline.is_some_and(|seconds| started.elapsed().as_secs() > seconds) {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(root_pid as i32), libc::SIGKILL);
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .expect("spawn terminal limit monitor")
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 probes existence without sending a signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(target_os = "macos")]
fn sandbox_temp_dir() -> Result<PathBuf, AgentError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let path = std::env::temp_dir().join(format!(
        "ee-sandbox-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).map_err(|error| {
        AgentError::Io(format!("sandbox temp dir {} failed: {error}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(path)
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
