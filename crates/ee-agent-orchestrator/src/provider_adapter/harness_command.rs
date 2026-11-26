//! `/harness` runs the recursive harness improvement loop for the last task.
//!
//! The provider adapter owns the session, not the rollout host. This command
//! delegates to the checked-in CLI (`ee do agent harness improve`) with a
//! bounded child process, fixed arguments, no shell, and a wall-clock timeout.
//! The child reuses the same configured agent host and the same driver the
//! manual CLI path uses, so there is one implementation of the loop.
//!
//! The provider runs as its own binary (`ee-agent-opencode`), so
//! `current_exe` is not the CLI. Resolution order is `EE_CLI_BIN`, the `ee`
//! sibling of the running provider, then `ee` on `PATH`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{ADOPT_COMMAND_NAME, HARNESS_COMMAND_NAME, SessionId, StopReason};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use super::{ProviderTelemetryTurn, finish_provider_telemetry};
use crate::events::OrchestratorEvent;
use crate::model::ModelMessage;
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::session_log::{HarnessLedgerReport, SessionLog};

/// Wall-clock bound for one `/harness` child process.
const HARNESS_TIMEOUT: Duration = Duration::from_secs(3_600);
/// Bound retained from a failure diagnostic.
const MAX_ERROR_BYTES: usize = 640;
/// Bound retained from the child streams for the final report.
const MAX_CHILD_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum recursion depth accepted from the command line.
const MAX_K: usize = 16;
/// Maximum cross-domain workers accepted from the command line.
const MAX_WORKERS: usize = 16;
/// Environment override for the `ee` CLI used by `/harness`.
const CLI_BIN_ENV: &str = "EE_CLI_BIN";

/// Parsed `/harness` instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HarnessArgs {
    pub(super) k: usize,
    pub(super) mode: String,
    pub(super) workers: usize,
    pub(super) dry_run: bool,
    pub(super) no_reflect: bool,
}

impl Default for HarnessArgs {
    fn default() -> Self {
        Self { k: 1, mode: String::from("parallel"), workers: 2, dry_run: false, no_reflect: false }
    }
}

/// Inputs for one `/harness` invocation.
pub(super) struct HarnessCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) cancel: watch::Receiver<bool>,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) workspace: &'a Path,
    pub(super) history: Vec<ModelMessage>,
    pub(super) session_log: Option<Arc<SessionLog>>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/harness [k=N] [mode=parallel|sequential] [workers=N] [dry-run]`.
pub(super) async fn handle_harness_command(
    ctx: HarnessCommand<'_>,
) -> Result<PromptResult, ProviderError> {
    let args = parse_harness_args(ctx.instructions).map_err(ProviderError::InvalidRequest)?;
    let task = super::reflect_command::last_task(&ctx.history)
        .or_else(|| last_logged_task(ctx.runtime, &ctx.session_id.to_string()))
        .ok_or_else(|| {
            ProviderError::InvalidRequest(String::from(
                "`/harness` needs a completed task in this session; run the task first",
            ))
        })?;
    let domain = super::reflect_command::resolve_domain(None, ctx.workspace)?;
    let executable = resolve_ee_executable().map_err(ProviderError::BackendFailure)?;
    let argv = child_args(&executable, &args, ctx.workspace, &task, &domain);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let cancel = ctx.cancel.clone();
    if *cancel.borrow() {
        finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Cancelled);
        return Ok(PromptResult::new(StopReason::Cancelled));
    }
    let (status, stdout, stderr) = match run_child_streaming(&mut command, ctx.sink, &cancel).await
    {
        Ok(ChildOutcome::Finished { status, stdout, stderr }) => (status, stdout, stderr),
        Ok(ChildOutcome::Cancelled) => {
            finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Cancelled);
            return Ok(PromptResult::new(StopReason::Cancelled));
        }
        Err(error) => {
            finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Failed);
            return Err(error);
        }
    };
    let events = ctx.runtime.event_snapshot();
    let events = if ctx.telemetry_event_start <= events.len() {
        &events[ctx.telemetry_event_start..]
    } else {
        &[]
    };
    let parsed = parse_child_report(&stdout);
    if let Some(log) = &ctx.session_log
        && let Some(report) = parsed.as_ref()
        && let Err(error) = log.record_harness_ledger(HarnessLedgerReport {
            task_id: report.task_id.clone(),
            run_root: report.run_root.clone(),
            rollouts: report.rollouts,
            sessions: report.sessions,
            selection_sessions: report.selection_sessions,
            retry_sessions: report.retry_sessions,
            reflection_sessions: report.reflection_sessions,
            dead_rollouts: report.dead_rollouts,
            base_failed: report.base_failed,
        })
    {
        tracing::warn!(%error, "harness ledger report could not be recorded");
    }
    let report = status_report(
        &ctx.session_id.to_string(),
        &args,
        status.success(),
        &status.to_string(),
        parsed.as_ref(),
        &stderr,
    );
    let _ = ctx.sink.agent_message_chunk("harness", report);
    let outcome = if status.success() {
        TelemetryTurnOutcome::Succeeded
    } else {
        TelemetryTurnOutcome::Failed
    };
    finish_harness_telemetry(&ctx, events, outcome);
    if status.success() {
        Ok(PromptResult::new(StopReason::EndTurn))
    } else {
        Err(ProviderError::BackendFailure(format!(
            "`/harness` exited with {status}; see status above"
        )))
    }
}

enum ChildOutcome {
    Finished { status: std::process::ExitStatus, stdout: String, stderr: String },
    Cancelled,
}

async fn run_child_streaming(
    command: &mut tokio::process::Command,
    sink: &UpdateSink,
    cancel: &watch::Receiver<bool>,
) -> Result<ChildOutcome, ProviderError> {
    let mut child = command.spawn().map_err(|error| {
        ProviderError::BackendFailure(format!("`/harness` could not start the loop: {error}"))
    })?;
    let mut stdout_lines = BufReader::new(child.stdout.take().ok_or_else(|| {
        ProviderError::BackendFailure(String::from("`/harness` child stdout is not piped"))
    })?)
    .lines();
    let mut stderr_lines = BufReader::new(child.stderr.take().ok_or_else(|| {
        ProviderError::BackendFailure(String::from("`/harness` child stderr is not piped"))
    })?)
    .lines();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut cancel = cancel.clone();
    let mut cancel_open = true;
    let deadline = tokio::time::Instant::now() + HARNESS_TIMEOUT;
    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            changed = cancel.changed(), if cancel_open => {
                match changed {
                    Ok(()) if *cancel.borrow() => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Ok(ChildOutcome::Cancelled);
                    }
                    Ok(()) => {}
                    Err(_) => cancel_open = false,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ProviderError::BackendFailure(format!(
                    "`/harness` exceeded {} seconds and was killed",
                    HARNESS_TIMEOUT.as_secs()
                )));
            }
            line = stdout_lines.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        append_capture(&mut stdout, &line);
                        if let Some(update) = progress_update(&line) {
                            let _ = sink.agent_message_chunk(HARNESS_COMMAND_NAME, update);
                        }
                    }
                    Ok(None) => stdout_done = true,
                    Err(error) => {
                        append_capture(&mut stdout, &format!("stdout read error: {error}"));
                        stdout_done = true;
                    }
                }
            }
            line = stderr_lines.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => append_capture(&mut stderr, &line),
                    Ok(None) => stderr_done = true,
                    Err(error) => {
                        append_capture(&mut stderr, &format!("stderr read error: {error}"));
                        stderr_done = true;
                    }
                }
            }
        }
    }
    let status = child.wait().await.map_err(|error| {
        ProviderError::BackendFailure(format!("`/harness` child wait failed: {error}"))
    })?;
    Ok(ChildOutcome::Finished { status, stdout, stderr })
}

fn append_capture(buffer: &mut String, line: &str) {
    if buffer.len() >= MAX_CHILD_CAPTURE_BYTES {
        return;
    }
    let remaining = MAX_CHILD_CAPTURE_BYTES - buffer.len();
    let mut end = line.len().min(remaining);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    buffer.push_str(&line[..end]);
    buffer.push('\n');
}

#[derive(Debug, serde::Deserialize)]
struct ChildProgress {
    stage: String,
    state: String,
    rollouts: u64,
    sessions: u64,
    selection_sessions: u64,
    retry_sessions: u64,
    reflection_sessions: u64,
    dead_rollouts: usize,
    base_failed: bool,
}

fn progress_update(line: &str) -> Option<String> {
    let json = line.strip_prefix("ee: harness-progress ")?;
    let progress: ChildProgress = serde_json::from_str(json).ok()?;
    Some(format!(
        "`/{HARNESS_COMMAND_NAME}` {} stage={} rollouts={} sessions={} selection={} retries={} reflection={} dead={} base_failed={}",
        progress.state,
        progress.stage,
        progress.rollouts,
        progress.sessions,
        progress.selection_sessions,
        progress.retry_sessions,
        progress.reflection_sessions,
        progress.dead_rollouts,
        progress.base_failed
    ))
}

fn finish_harness_telemetry(
    ctx: &HarnessCommand<'_>,
    events: &[OrchestratorEvent],
    outcome: TelemetryTurnOutcome,
) {
    finish_provider_telemetry(
        ctx.telemetry,
        ProviderTelemetryTurn {
            turn_id: ctx.telemetry_turn,
            started_at: ctx.telemetry_started_at,
            events,
            outcome,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
}

/// Parses the slash-command instructions into driver options.
pub(super) fn parse_harness_args(instructions: Option<&str>) -> Result<HarnessArgs, String> {
    let mut args = HarnessArgs::default();
    for token in instructions.unwrap_or_default().split_whitespace() {
        if token == "dry-run" {
            args.dry_run = true;
        } else if token == "no-reflect" {
            args.no_reflect = true;
        } else if let Some(value) = token.strip_prefix("k=") {
            args.k = value
                .parse::<usize>()
                .ok()
                .filter(|k| (1..=MAX_K).contains(k))
                .ok_or_else(|| format!("`k` must be 1-{MAX_K}; got {value:?}"))?;
        } else if let Some(value) = token.strip_prefix("mode=") {
            if !matches!(value, "parallel" | "sequential") {
                return Err(format!("`mode` must be parallel or sequential; got {value:?}"));
            }
            args.mode = value.to_string();
        } else if let Some(value) = token.strip_prefix("workers=") {
            args.workers = value
                .parse::<usize>()
                .ok()
                .filter(|workers| (1..=MAX_WORKERS).contains(workers))
                .ok_or_else(|| format!("`workers` must be 1-{MAX_WORKERS}; got {value:?}"))?;
        } else {
            return Err(format!(
                "unknown `/harness` argument {token:?}; usage: k=N mode=parallel|sequential workers=N dry-run no-reflect"
            ));
        }
    }
    Ok(args)
}

fn last_logged_task(runtime: &OrchestratorRuntime, session_id: &str) -> Option<String> {
    let records = runtime.replay_session_messages(session_id).ok()?;
    last_task_from_logged(&records)
}

fn last_task_from_logged(records: &[(String, ModelMessage)]) -> Option<String> {
    let messages: Vec<ModelMessage> = records.iter().map(|(_, message)| message.clone()).collect();
    super::reflect_command::last_task(&messages)
}

/// Resolves the `ee` CLI that hosts the recursive improvement loop.
pub(super) fn resolve_ee_executable() -> Result<PathBuf, String> {
    let override_path =
        std::env::var_os(CLI_BIN_ENV).filter(|value| !value.is_empty()).map(PathBuf::from);
    let current_exe = std::env::current_exe().ok();
    let path_var = std::env::var_os("PATH");
    resolve_ee_executable_from(
        override_path.as_deref(),
        current_exe.as_deref(),
        path_var.as_deref(),
    )
}

fn resolve_ee_executable_from(
    override_path: Option<&Path>,
    current_exe: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        if is_executable_file(path) {
            return Ok(path.to_path_buf());
        }
        return Err(format!(
            "{CLI_BIN_ENV} does not point to an executable file: {}",
            path.display()
        ));
    }
    if let Some(executable) = current_exe {
        if executable.file_name() == Some(OsStr::new(ee_binary_file_name()))
            && is_executable_file(executable)
        {
            return Ok(executable.to_path_buf());
        }
        if let Some(parent) = executable.parent() {
            let sibling = parent.join(ee_binary_file_name());
            if is_executable_file(&sibling) {
                return Ok(sibling);
            }
        }
    }
    if let Some(path_var) = path_var {
        for directory in std::env::split_paths(path_var) {
            // An empty PATH component means the current directory; never
            // auto-run a workspace-controlled `ee` from there.
            if directory.as_os_str().is_empty() {
                continue;
            }
            let candidate = directory.join(ee_binary_file_name());
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(format!("cannot locate the `ee` CLI executable; set {CLI_BIN_ENV} or add `ee` to PATH"))
}

fn ee_binary_file_name() -> &'static str {
    if cfg!(windows) { "ee.exe" } else { "ee" }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn child_args(
    executable: &Path,
    args: &HarnessArgs,
    workspace: &Path,
    prompt: &str,
    domain: &str,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = [
        executable.as_os_str(),
        std::ffi::OsStr::new("do"),
        std::ffi::OsStr::new("agent"),
        std::ffi::OsStr::new("harness"),
        std::ffi::OsStr::new("improve"),
        std::ffi::OsStr::new("--workspace"),
        workspace.as_os_str(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for (flag, value) in [
        ("--prompt", prompt.to_string()),
        ("--domain", domain.to_string()),
        ("--k", args.k.to_string()),
        ("--mode", args.mode.clone()),
        ("--workers", args.workers.to_string()),
    ] {
        argv.push(OsString::from(flag));
        argv.push(OsString::from(value));
    }
    if args.dry_run {
        argv.push(OsString::from("--dry-run"));
    }
    argv.push(OsString::from("--reflection"));
    argv.push(OsString::from("accept"));
    if args.no_reflect {
        argv.push(OsString::from("--no-reflect"));
    }
    argv
}

#[derive(Debug, serde::Deserialize)]
struct ChildReport {
    task_id: String,
    #[serde(default)]
    selected_label: Option<String>,
    #[serde(default)]
    run_root: Option<String>,
    #[serde(default)]
    library_root: Option<String>,
    #[serde(default)]
    reflection_write: Option<String>,
    #[serde(default)]
    reflection_source: Option<String>,
    #[serde(default)]
    adopted_path: Option<String>,
    #[serde(default)]
    reflected_path: Option<String>,
    rollouts: u64,
    sessions: u64,
    selection_sessions: u64,
    retry_sessions: u64,
    reflection_sessions: u64,
    dead_rollouts: u64,
    base_failed: bool,
}

fn parse_child_report(stdout: &str) -> Option<ChildReport> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("ee: harness-report "))
        .and_then(|json| serde_json::from_str(json).ok())
}

fn status_report(
    session: &str,
    args: &HarnessArgs,
    success: bool,
    exit: &str,
    report: Option<&ChildReport>,
    stderr: &str,
) -> String {
    let base_failed = report.is_some_and(|report| report.base_failed);
    let failed = !success || base_failed;
    let state = if failed { "failed" } else { "completed" };
    let mut lines = vec![
        format!("`/{HARNESS_COMMAND_NAME}` {state}"),
        format!(
            "session={session} k={} mode={} workers={}{}",
            args.k,
            args.mode,
            args.workers,
            if args.dry_run { " dry-run" } else { "" }
        ),
    ];
    if let Some(report) = report {
        lines.push(format!(
            "task={} chosen={}",
            report.task_id,
            report.selected_label.as_deref().unwrap_or("none")
        ));
        if let Some(run_root) = &report.run_root {
            lines.push(format!("run root: {run_root}"));
        }
        if let Some(library_root) = &report.library_root {
            lines.push(format!("library: {library_root}"));
        }
        lines.push(format!(
            "rollouts={} sessions={} selection={} retries={} reflection={} dead={} base_failed={}",
            report.rollouts,
            report.sessions,
            report.selection_sessions,
            report.retry_sessions,
            report.reflection_sessions,
            report.dead_rollouts,
            report.base_failed
        ));
        if let Some(write) = &report.reflection_write {
            let source = report.reflection_source.as_deref().unwrap_or("unknown");
            lines.push(format!("experience: {write} (source: {source})"));
        }
        if let Some(path) = &report.adopted_path {
            lines.push(format!("adopted tree: {path}"));
        }
        if let Some(path) = &report.reflected_path {
            lines.push(format!("experience entry: {path}"));
        }
    } else {
        lines.push(format!("exit: {exit}"));
        if success {
            lines.push(String::from("report: missing"));
        }
    }
    if failed {
        let reason = if success {
            String::from("base harness delivered no reference run")
        } else {
            error_excerpt(stderr)
        };
        lines.push(format!("reason: {reason}"));
        lines.push(format!(
            "next: fix the failure, then rerun `/{HARNESS_COMMAND_NAME}`; do not `/{ADOPT_COMMAND_NAME}` this run"
        ));
    } else if args.dry_run {
        lines.push(format!(
            "next: rerun `/{HARNESS_COMMAND_NAME}` without dry-run to produce a candidate"
        ));
    } else if report.is_none() {
        lines.push(String::from("next: inspect the run root; report missing, nothing was adopted"));
    } else if report.is_some_and(|report| report.selected_label.is_some()) {
        lines.push(format!(
            "next: run `/{ADOPT_COMMAND_NAME}` to apply the chosen harness; current `.agents` stays active"
        ));
    } else {
        lines.push(String::from("next: keep the current harness; no selected tree to adopt"));
    }
    lines.join("\n")
}

fn error_excerpt(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return String::from("no diagnostic output");
    }
    let mut start = text.len().saturating_sub(MAX_ERROR_BYTES);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    if start == 0 { text.to_string() } else { format!("…{}", &text[start..]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelRole;

    #[test]
    fn harness_arguments_parse_and_reject_unknown_tokens() {
        let args = parse_harness_args(Some("k=2 mode=sequential workers=4 dry-run no-reflect"))
            .expect("args");
        assert_eq!(args.k, 2);
        assert_eq!(args.mode, "sequential");
        assert_eq!(args.workers, 4);
        assert!(args.dry_run);
        assert!(args.no_reflect);
        assert_eq!(parse_harness_args(None).expect("defaults"), HarnessArgs::default());
        assert!(parse_harness_args(Some("k=0")).is_err());
        assert!(parse_harness_args(Some("k=99")).is_err());
        assert!(parse_harness_args(Some("mode=fast")).is_err());
        assert!(parse_harness_args(Some("workers=0")).is_err());
        assert!(parse_harness_args(Some("verbose")).is_err());
    }

    fn sample_report() -> ChildReport {
        ChildReport {
            task_id: String::from("t"),
            selected_label: Some(String::from("composed-0")),
            run_root: Some(String::from("/tmp/run")),
            library_root: Some(String::from("/tmp/library")),
            reflection_write: Some(String::from("written")),
            reflection_source: Some(String::from("invocation")),
            adopted_path: Some(String::from("/tmp/run/adopted")),
            reflected_path: Some(String::from("/tmp/library/entry")),
            rollouts: 4,
            sessions: 3,
            selection_sessions: 0,
            retry_sessions: 1,
            reflection_sessions: 1,
            dead_rollouts: 0,
            base_failed: false,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_stream_posts_progress_and_keeps_final_report() {
        use std::process::Stdio;
        use tokio::sync::mpsc;

        let (sink_tx, mut sink_rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), sink_tx);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s\\n' 'ee: harness-progress {\"stage\":\"reference\",\"state\":\"completed\",\"rollouts\":1,\"sessions\":0,\"selection_sessions\":0,\"retry_sessions\":0,\"reflection_sessions\":0,\"dead_rollouts\":0,\"base_failed\":false}' 'ee: harness-report {\"task_id\":\"t\",\"rollouts\":1,\"sessions\":0,\"selection_sessions\":0,\"retry_sessions\":0,\"reflection_sessions\":0,\"dead_rollouts\":0,\"base_failed\":false}'; printf 'warning\\n' >&2",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let outcome = run_child_streaming(&mut command, &sink, &cancel_rx).await.expect("child");
        let ChildOutcome::Finished { status, stdout, stderr } = outcome else {
            panic!("child must finish");
        };
        assert!(status.success());
        assert!(stdout.contains("ee: harness-progress"), "{stdout}");
        assert!(stdout.contains("ee: harness-report"), "{stdout}");
        assert_eq!(stderr.trim(), "warning");
        assert!(sink_rx.try_recv().is_ok(), "progress must reach the chat sink");
    }

    #[test]
    fn progress_lines_become_compact_chat_updates() {
        let line = "ee: harness-progress {\"stage\":\"step-0-probe\",\"state\":\"completed\",\"rollouts\":3,\"sessions\":3,\"selection_sessions\":0,\"retry_sessions\":0,\"reflection_sessions\":0,\"dead_rollouts\":0,\"base_failed\":false}";
        let update = progress_update(line).expect("progress");
        assert!(update.contains("stage=step-0-probe"), "{update}");
        assert!(update.contains("rollouts=3"), "{update}");
        assert!(!update.contains("stdout"), "{update}");
        assert!(progress_update("ordinary output").is_none());
    }

    #[test]
    fn success_status_is_compact_and_ends_with_adopt_hint() {
        let text = status_report(
            "session-1",
            &HarnessArgs::default(),
            true,
            "exit status: 0",
            Some(&sample_report()),
            "noise that must not be posted",
        );
        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("task=t chosen=composed-0"), "{text}");
        assert!(text.contains("rollouts=4"), "{text}");
        assert!(text.contains("next: run `/adopt`"), "{text}");
        assert!(!text.contains("stdout"), "{text}");
        assert!(!text.contains("noise that must not be posted"), "{text}");
    }

    #[test]
    fn failure_status_is_compact_and_says_not_to_adopt() {
        let text = status_report(
            "session-1",
            &HarnessArgs::default(),
            false,
            "exit status: 1",
            None,
            "fatal: child failed",
        );
        assert!(text.contains("failed"), "{text}");
        assert!(text.contains("reason: fatal: child failed"), "{text}");
        assert!(text.contains("do not `/adopt`"), "{text}");
        assert!(!text.contains("stdout"), "{text}");
    }

    #[test]
    fn child_report_lines_parse_into_ledger_totals() {
        let stdout = "noise\nee: harness-report {\"task_id\":\"t\",\"run_root\":\"/tmp/run\",\"rollouts\":8,\"sessions\":6,\"selection_sessions\":1,\"retry_sessions\":0,\"reflection_sessions\":1,\"dead_rollouts\":2,\"base_failed\":false}\nmore";
        let report = parse_child_report(stdout).expect("report");
        assert_eq!(report.task_id, "t");
        assert_eq!(report.run_root.as_deref(), Some("/tmp/run"));
        assert_eq!(report.rollouts, 8);
        assert_eq!(report.sessions, 6);
        assert_eq!(report.dead_rollouts, 2);
        assert!(!report.base_failed);
        assert!(parse_child_report("no report").is_none());
    }

    #[test]
    fn logged_session_messages_supply_a_task_when_the_transcript_is_empty() {
        let records = vec![
            (String::from("1"), ModelMessage::text(ModelRole::User, "first task")),
            (String::from("2"), ModelMessage::text(ModelRole::Assistant, "done")),
            (String::from("3"), ModelMessage::text(ModelRole::User, "/harness")),
            (String::from("4"), ModelMessage::text(ModelRole::User, "last task")),
        ];
        assert_eq!(last_task_from_logged(&records).as_deref(), Some("last task"));
        assert!(last_task_from_logged(&[]).is_none());
    }

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, b"#!/bin/sh\n").expect("write fake executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("mark fake executable");
    }

    #[cfg(unix)]
    #[test]
    fn harness_executable_prefers_the_provider_sibling_over_path() {
        let provider_root = tempfile::tempdir().expect("provider dir");
        let provider = provider_root.path().join("ee-agent-opencode");
        let sibling = provider_root.path().join("ee");
        write_executable(&provider);
        write_executable(&sibling);
        let path_root = tempfile::tempdir().expect("path dir");
        write_executable(&path_root.path().join("ee"));
        let path_var = std::env::join_paths([path_root.path()]).expect("path value");

        let resolved = resolve_ee_executable_from(None, Some(&provider), Some(&path_var))
            .expect("sibling resolves");
        assert_eq!(resolved, sibling);
    }

    #[cfg(unix)]
    #[test]
    fn harness_executable_uses_the_environment_override_first() {
        let override_root = tempfile::tempdir().expect("override dir");
        let override_path = override_root.path().join("custom-ee");
        write_executable(&override_path);
        let provider_root = tempfile::tempdir().expect("provider dir");
        let provider = provider_root.path().join("ee-agent-opencode");
        write_executable(&provider);
        write_executable(&provider_root.path().join("ee"));

        let resolved = resolve_ee_executable_from(Some(&override_path), Some(&provider), None)
            .expect("override resolves");
        assert_eq!(resolved, override_path);
    }

    #[cfg(unix)]
    #[test]
    fn harness_executable_falls_back_to_path_when_no_sibling_exists() {
        let provider_root = tempfile::tempdir().expect("provider dir");
        let provider = provider_root.path().join("ee-agent-opencode");
        write_executable(&provider);
        let path_root = tempfile::tempdir().expect("path dir");
        let path_ee = path_root.path().join("ee");
        write_executable(&path_ee);
        let path_var = std::env::join_paths([path_root.path()]).expect("path value");

        let resolved = resolve_ee_executable_from(None, Some(&provider), Some(&path_var))
            .expect("path resolves");
        assert_eq!(resolved, path_ee);
    }

    #[test]
    fn harness_executable_reports_a_missing_override() {
        let missing = Path::new("/nonexistent/ee-cli-for-harness-test");
        let error = resolve_ee_executable_from(Some(missing), None, None)
            .expect_err("missing override rejected");
        assert!(error.contains(CLI_BIN_ENV), "unexpected error: {error}");
    }

    #[test]
    fn harness_executable_reports_when_nothing_can_be_resolved() {
        let empty_path = std::env::join_paths(std::iter::empty::<&Path>()).expect("empty path");
        let error = resolve_ee_executable_from(None, None, Some(&empty_path))
            .expect_err("no executable rejected");
        assert!(error.contains("cannot locate the `ee` CLI"), "unexpected error: {error}");
    }

    #[test]
    fn child_arguments_are_fixed_and_shell_free() {
        let executable = Path::new("/usr/bin/ee");
        let args = HarnessArgs {
            k: 3,
            mode: String::from("parallel"),
            workers: 2,
            dry_run: true,
            no_reflect: false,
        };
        let argv = child_args(executable, &args, Path::new("/work"), "do the task", "rust");
        let rendered: Vec<String> =
            argv.iter().map(|value| value.to_string_lossy().to_string()).collect();
        assert_eq!(
            rendered,
            vec![
                "/usr/bin/ee",
                "do",
                "agent",
                "harness",
                "improve",
                "--workspace",
                "/work",
                "--prompt",
                "do the task",
                "--domain",
                "rust",
                "--k",
                "3",
                "--mode",
                "parallel",
                "--workers",
                "2",
                "--dry-run",
                "--reflection",
                "accept",
            ]
        );
    }

    #[test]
    fn child_arguments_can_disable_reflection() {
        let executable = Path::new("/usr/bin/ee");
        let args = HarnessArgs {
            k: 1,
            mode: String::from("parallel"),
            workers: 2,
            dry_run: false,
            no_reflect: true,
        };
        let argv = child_args(executable, &args, Path::new("/work"), "task", "rust");
        let rendered: Vec<String> =
            argv.iter().map(|value| value.to_string_lossy().to_string()).collect();
        assert!(rendered.windows(2).any(|pair| pair == ["--reflection", "accept"]));
        assert!(rendered.iter().any(|value| value == "--no-reflect"));
        assert!(!rendered.iter().any(|value| value == "--yes"));
    }
}
