//! Bounded, non-interactive git command execution for grammar staging.
//!
//! Grammar installs clone and fetch remote repositories. A stalled network,
//! credential prompt, or unreachable host must fail the operation instead of
//! blocking `mk install` forever, so every git command runs with a deadline and
//! without terminal prompts.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::helpers::redact_git_command_args;
use super::types::RuntimeOperationError;

/// Hard ceiling for one git invocation, including clone and fetch transfers.
/// Large grammar repositories can take minutes on a slow link; this is a
/// backstop against a truly wedged process, not a transfer-speed limit.
const DEFAULT_GIT_COMMAND_TIMEOUT_SECS: u64 = 600;
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

pub fn git_rev_parse(
    source_dir: &Path,
    revision: &str,
    display_ref: &str,
    source_label: &str,
) -> Result<String, RuntimeOperationError> {
    let args = vec![
        String::from("-C"),
        source_dir.display().to_string(),
        String::from("rev-parse"),
        String::from("--verify"),
        revision.to_string(),
    ];
    let output = run_git_capture(args)?;
    if !output.success {
        let stderr = output.stderr.trim();
        return Err(RuntimeOperationError::grammar_source(format!(
            "{source_label}: missing {display_ref} in {}{}",
            source_dir.display(),
            if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
        )));
    }
    Ok(output.stdout.trim().to_string())
}

pub fn run_git<I, S>(current_dir: Option<&Path>, args: I) -> Result<(), RuntimeOperationError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args.into_iter().map(|value| value.as_ref().to_string()).collect::<Vec<_>>();
    let display_args = redact_git_command_args(&args);
    let output = run_git_capture_with_dir(current_dir, args)?;
    if output.success {
        return Ok(());
    }

    let stderr = output.stderr.trim();
    Err(RuntimeOperationError::grammar_source(format!(
        "`git {display_args}` failed{}",
        if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
    )))
}

fn run_git_capture(args: Vec<String>) -> Result<GitOutput, RuntimeOperationError> {
    run_git_capture_with_dir(None, args)
}

fn run_git_capture_with_dir(
    current_dir: Option<&Path>,
    args: Vec<String>,
) -> Result<GitOutput, RuntimeOperationError> {
    let display_args = redact_git_command_args(&args);
    let mut command = Command::new("git");
    // Abort stalled transfers well before the hard deadline. Local commands
    // ignore these settings.
    command
        .args(["-c", "http.lowSpeedLimit=100", "-c", "http.lowSpeedTime=30"])
        .args(&args)
        // A background install has no terminal to answer credential prompts.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }

    let mut child = command.spawn().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `git {display_args}`: {error}"
        ))
    })?;

    // Drain both pipes on their own threads so a chatty command cannot fill a
    // pipe buffer and block before `try_wait` observes its exit.
    let stdout_reader = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = pipe.read_to_string(&mut buffer);
            buffer
        })
    });
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = pipe.read_to_string(&mut buffer);
            buffer
        })
    });

    let timeout = git_command_timeout();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Do not join the pipe readers: a surviving git child can
                    // keep the pipes open, and the deadline must still return.
                    drop(stdout_reader);
                    drop(stderr_reader);
                    return Err(RuntimeOperationError::grammar_source(format!(
                        "`git {display_args}` timed out after {}s",
                        timeout.as_secs()
                    )));
                }
                thread::sleep(GIT_POLL_INTERVAL);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(RuntimeOperationError::grammar_source(format!(
                    "failed waiting for `git {display_args}`: {error}"
                )));
            }
        }
    };

    Ok(GitOutput {
        success: status.success(),
        stdout: join_pipe(stdout_reader).unwrap_or_default(),
        stderr: join_pipe(stderr_reader).unwrap_or_default(),
    })
}

fn join_pipe(handle: Option<thread::JoinHandle<String>>) -> Option<String> {
    handle.and_then(|handle| handle.join().ok())
}

fn git_command_timeout() -> Duration {
    std::env::var("EE_RUNTIME_GIT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_GIT_COMMAND_TIMEOUT_SECS))
}
