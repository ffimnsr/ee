//! `/harness` runs the recursive harness improvement loop for the last task.
//!
//! The provider adapter owns the session, not the rollout host. This command
//! delegates to the checked-in CLI (`ee do agent harness improve`) with a
//! bounded child process, fixed arguments, no shell, and a wall-clock timeout.
//! The child reuses the same configured agent host and the same driver the
//! manual CLI path uses, so there is one implementation of the loop.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_agent_acp_server::{PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{HARNESS_COMMAND_NAME, SessionId, StopReason};
use tokio::sync::watch;

use super::{ProviderTelemetryTurn, finish_provider_telemetry};
use crate::events::OrchestratorEvent;
use crate::model::ModelMessage;
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::session_log::{HarnessLedgerReport, SessionLog};

/// Wall-clock bound for one `/harness` child process.
const HARNESS_TIMEOUT: Duration = Duration::from_secs(3_600);
/// Bound retained from each child stream.
const MAX_CHILD_OUTPUT_BYTES: usize = 32 * 1024;
/// Maximum recursion depth accepted from the command line.
const MAX_K: usize = 16;
/// Maximum cross-domain workers accepted from the command line.
const MAX_WORKERS: usize = 16;

/// Parsed `/harness` instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HarnessArgs {
    pub(super) k: usize,
    pub(super) mode: String,
    pub(super) workers: usize,
    pub(super) dry_run: bool,
}

impl Default for HarnessArgs {
    fn default() -> Self {
        Self { k: 1, mode: String::from("parallel"), workers: 2, dry_run: false }
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
    let task = super::reflect_command::last_task(&ctx.history).ok_or_else(|| {
        ProviderError::InvalidRequest(String::from(
            "`/harness` needs a completed task in this session; run the task first",
        ))
    })?;
    let domain = super::reflect_command::resolve_domain(None, ctx.workspace)?;
    let executable = std::env::current_exe().map_err(|error| {
        ProviderError::BackendFailure(format!("cannot resolve the ee executable: {error}"))
    })?;
    let argv = child_args(&executable, &args, ctx.workspace, &task, &domain);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut cancel = ctx.cancel.clone();
    if *cancel.borrow() {
        finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Cancelled);
        return Ok(PromptResult::new(StopReason::Cancelled));
    }
    let output = tokio::select! {
        changed = cancel.changed() => {
            let _ = changed;
            finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Cancelled);
            return Ok(PromptResult::new(StopReason::Cancelled));
        }
        result = tokio::time::timeout(HARNESS_TIMEOUT, command.output()) => result,
    };
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Failed);
            return Err(ProviderError::BackendFailure(format!(
                "`/harness` could not start the loop: {error}"
            )));
        }
        Err(_) => {
            finish_harness_telemetry(&ctx, &[], TelemetryTurnOutcome::Failed);
            return Err(ProviderError::BackendFailure(format!(
                "`/harness` exceeded {} seconds and was killed",
                HARNESS_TIMEOUT.as_secs()
            )));
        }
    };
    let events = ctx.runtime.event_snapshot();
    let events = if ctx.telemetry_event_start <= events.len() {
        &events[ctx.telemetry_event_start..]
    } else {
        &[]
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if let Some(log) = &ctx.session_log
        && let Some(report) = parse_child_report(&stdout)
        && let Err(error) = log.record_harness_ledger(HarnessLedgerReport {
            task_id: report.task_id,
            run_root: None,
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
    let report = format!(
        "`/{HARNESS_COMMAND_NAME}` session={} k={} mode={} workers={}{}\n\
         exit: {}\n\nstdout:\n{}\nstderr:\n{}\n",
        ctx.session_id,
        args.k,
        args.mode,
        args.workers,
        if args.dry_run { " dry-run" } else { "" },
        output.status,
        truncate_child(&stdout),
        truncate_child(&stderr)
    );
    let _ = ctx.sink.agent_message_chunk("harness", report);
    let outcome = if output.status.success() {
        TelemetryTurnOutcome::Succeeded
    } else {
        TelemetryTurnOutcome::Failed
    };
    finish_harness_telemetry(&ctx, events, outcome);
    if output.status.success() {
        Ok(PromptResult::new(StopReason::EndTurn))
    } else {
        Err(ProviderError::BackendFailure(format!(
            "`/harness` exited with {}; see the bounded output above",
            output.status
        )))
    }
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
                "unknown `/harness` argument {token:?}; usage: k=N mode=parallel|sequential workers=N dry-run"
            ));
        }
    }
    Ok(args)
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
    argv
}

#[derive(Debug, serde::Deserialize)]
struct ChildReport {
    task_id: String,
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

fn truncate_child(text: &str) -> String {
    if text.len() <= MAX_CHILD_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_CHILD_OUTPUT_BYTES.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]\n", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_arguments_parse_and_reject_unknown_tokens() {
        let args = parse_harness_args(Some("k=2 mode=sequential workers=4 dry-run")).expect("args");
        assert_eq!(args.k, 2);
        assert_eq!(args.mode, "sequential");
        assert_eq!(args.workers, 4);
        assert!(args.dry_run);
        assert_eq!(parse_harness_args(None).expect("defaults"), HarnessArgs::default());
        assert!(parse_harness_args(Some("k=0")).is_err());
        assert!(parse_harness_args(Some("k=99")).is_err());
        assert!(parse_harness_args(Some("mode=fast")).is_err());
        assert!(parse_harness_args(Some("workers=0")).is_err());
        assert!(parse_harness_args(Some("verbose")).is_err());
    }

    #[test]
    fn child_report_lines_parse_into_ledger_totals() {
        let stdout = "noise\nee: harness-report {\"task_id\":\"t\",\"rollouts\":8,\"sessions\":6,\"selection_sessions\":1,\"retry_sessions\":0,\"reflection_sessions\":1,\"dead_rollouts\":2,\"base_failed\":false}\nmore";
        let report = parse_child_report(stdout).expect("report");
        assert_eq!(report.task_id, "t");
        assert_eq!(report.rollouts, 8);
        assert_eq!(report.sessions, 6);
        assert_eq!(report.dead_rollouts, 2);
        assert!(!report.base_failed);
        assert!(parse_child_report("no report").is_none());
    }

    #[test]
    fn child_arguments_are_fixed_and_shell_free() {
        let executable = Path::new("/usr/bin/ee");
        let args = HarnessArgs { k: 3, mode: String::from("parallel"), workers: 2, dry_run: true };
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
            ]
        );
    }
}
