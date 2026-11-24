//! `/adopt` applies the last harness run's chosen tree to the active `.agents`
//! surface after an explicit ACP confirmation.
//!
//! The command shells out to the checked-in CLI with fixed arguments: the run
//! root comes from the session log, never from model text. A dry-run diff is
//! shown first; only an Accept elicitation runs the apply with `--yes`.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_agent_acp_server::{ClientBridge, PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{
    ADOPT_COMMAND_NAME, CreateElicitationRequest, ElicitationAction, ElicitationFormMode,
    ElicitationMode, ElicitationSchema, ElicitationScope, ElicitationSessionScope, SessionId,
    StopReason,
};
use tokio::sync::watch;

use super::{ProviderTelemetryTurn, finish_provider_telemetry};
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::session_log::SessionLog;

/// Wall-clock bound for one adopt child process.
const ADOPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Bound retained from each child stream.
const MAX_CHILD_OUTPUT_BYTES: usize = 32 * 1024;

/// Inputs for one `/adopt` invocation.
pub(super) struct AdoptCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) client: ClientBridge,
    pub(super) cancel: watch::Receiver<bool>,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) workspace: &'a Path,
    /// Test seam: an explicit `ee` executable; production resolves from the
    /// environment/sibling/PATH.
    pub(super) executable: Option<&'a Path>,
    pub(super) session_log: Option<Arc<SessionLog>>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Executes `/adopt [--include-memories]`.
pub(super) async fn handle_adopt_command(
    ctx: AdoptCommand<'_>,
) -> Result<PromptResult, ProviderError> {
    let include_memories =
        parse_adopt_args(ctx.instructions).map_err(ProviderError::InvalidRequest)?;
    let run_root = ctx
        .session_log
        .as_ref()
        .and_then(|log| log.last_harness_run_root().ok().flatten())
        .ok_or_else(|| {
            ProviderError::InvalidRequest(String::from(
                "`/adopt` needs a completed `/harness` run in this session; run `/harness` first",
            ))
        })?;
    let executable = match ctx.executable {
        Some(executable) => executable.to_path_buf(),
        None => super::harness_command::resolve_ee_executable()
            .map_err(ProviderError::BackendFailure)?,
    };
    let dry = match run_adopt(
        &executable,
        Path::new(&run_root),
        ctx.workspace,
        true,
        include_memories,
        &ctx.cancel,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            finish_telemetry(&ctx, TelemetryTurnOutcome::Failed);
            return Err(error);
        }
    };
    let Some(dry) = dry else {
        return finish_cancelled(&ctx);
    };
    let stdout = bounded(&dry.stdout);
    let stderr = bounded(&dry.stderr);
    let diff = format!(
        "`/{ADOPT_COMMAND_NAME}` dry-run for run root {run_root}\nexit: {}\n\nstdout:\n{stdout}\nstderr:\n{stderr}\n",
        dry.status
    );
    let _ = ctx.sink.agent_message_chunk("adopt", diff);
    if !dry.status.success() {
        finish_telemetry(&ctx, TelemetryTurnOutcome::Failed);
        return Err(ProviderError::BackendFailure(format!(
            "`/adopt` dry-run exited with {}; nothing was applied",
            dry.status
        )));
    }
    let message = format!(
        "`/adopt` will replace the managed `.agents` surface in {} with the harness from {}. Apply?",
        ctx.workspace.display(),
        run_root
    );
    let approved = match confirm(&ctx.client, ctx.session_id, message).await {
        Ok(approved) => approved,
        Err(error) => {
            finish_telemetry(&ctx, TelemetryTurnOutcome::Failed);
            return Err(error);
        }
    };
    if !approved {
        let _ = ctx.sink.agent_message_chunk("adopt", "adoption declined; no files written");
        finish_telemetry(&ctx, TelemetryTurnOutcome::Succeeded);
        return Ok(PromptResult::new(StopReason::EndTurn));
    }
    let applied = match run_adopt(
        &executable,
        Path::new(&run_root),
        ctx.workspace,
        false,
        include_memories,
        &ctx.cancel,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            finish_telemetry(&ctx, TelemetryTurnOutcome::Failed);
            return Err(error);
        }
    };
    let Some(applied) = applied else {
        return finish_cancelled(&ctx);
    };
    let stdout = bounded(&applied.stdout);
    let stderr = bounded(&applied.stderr);
    let result = format!(
        "`/{ADOPT_COMMAND_NAME}` applied run root {run_root}\nexit: {}\n\nstdout:\n{stdout}\nstderr:\n{stderr}\n",
        applied.status
    );
    let _ = ctx.sink.agent_message_chunk("adopt", result);
    if applied.status.success() {
        finish_telemetry(&ctx, TelemetryTurnOutcome::Succeeded);
        Ok(PromptResult::new(StopReason::EndTurn))
    } else {
        finish_telemetry(&ctx, TelemetryTurnOutcome::Failed);
        Err(ProviderError::BackendFailure(format!(
            "`/adopt` exited with {}; see the bounded output above",
            applied.status
        )))
    }
}

fn finish_cancelled(ctx: &AdoptCommand<'_>) -> Result<PromptResult, ProviderError> {
    finish_telemetry(ctx, TelemetryTurnOutcome::Cancelled);
    Ok(PromptResult::new(StopReason::Cancelled))
}

fn finish_telemetry(ctx: &AdoptCommand<'_>, outcome: TelemetryTurnOutcome) {
    let events = ctx.runtime.event_snapshot();
    let events = if ctx.telemetry_event_start <= events.len() {
        &events[ctx.telemetry_event_start..]
    } else {
        &[]
    };
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

/// Parses the optional `--include-memories` token.
pub(super) fn parse_adopt_args(instructions: Option<&str>) -> Result<bool, String> {
    let mut include_memories = false;
    for token in instructions.unwrap_or_default().split_whitespace() {
        if token == "--include-memories" {
            include_memories = true;
        } else {
            return Err(format!(
                "unknown `/adopt` argument {token:?}; usage: /adopt [--include-memories]"
            ));
        }
    }
    Ok(include_memories)
}

async fn run_adopt(
    executable: &Path,
    run_root: &Path,
    workspace: &Path,
    dry_run: bool,
    include_memories: bool,
    cancel: &watch::Receiver<bool>,
) -> Result<Option<std::process::Output>, ProviderError> {
    let argv = child_args(executable, run_root, workspace, dry_run, include_memories);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut cancel = cancel.clone();
    if *cancel.borrow() {
        return Ok(None);
    }
    let output = tokio::select! {
        changed = cancel.changed() => {
            let _ = changed;
            return Ok(None);
        }
        result = tokio::time::timeout(ADOPT_TIMEOUT, command.output()) => result,
    };
    match output {
        Ok(Ok(output)) => Ok(Some(output)),
        Ok(Err(error)) => Err(ProviderError::BackendFailure(format!(
            "`/adopt` could not start the apply: {error}"
        ))),
        Err(_) => Err(ProviderError::BackendFailure(format!(
            "`/adopt` exceeded {} seconds and was killed",
            ADOPT_TIMEOUT.as_secs()
        ))),
    }
}

fn child_args(
    executable: &Path,
    run_root: &Path,
    workspace: &Path,
    dry_run: bool,
    include_memories: bool,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = [
        executable.as_os_str(),
        OsStr::new("do"),
        OsStr::new("agent"),
        OsStr::new("harness"),
        OsStr::new("adopt"),
        OsStr::new("--run-root"),
        run_root.as_os_str(),
        OsStr::new("--workspace"),
        workspace.as_os_str(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if include_memories {
        argv.push(OsString::from("--include-memories"));
    }
    argv.push(OsString::from(if dry_run { "--dry-run" } else { "--yes" }));
    argv
}

async fn confirm(
    client: &ClientBridge,
    session_id: &SessionId,
    message: String,
) -> Result<bool, ProviderError> {
    let scope = ElicitationScope::Session(ElicitationSessionScope::new(session_id.clone()));
    let mode = ElicitationMode::Form(ElicitationFormMode::new(scope, ElicitationSchema::new()));
    let response = client.create_elicitation(CreateElicitationRequest::new(mode, message)).await?;
    Ok(matches!(response.action, ElicitationAction::Accept(_)))
}

fn bounded(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
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
    use std::path::PathBuf;

    #[test]
    fn adopt_arguments_accept_only_the_memory_flag() {
        assert!(!parse_adopt_args(None).expect("defaults"));
        assert!(!parse_adopt_args(Some("")).expect("empty"));
        assert!(parse_adopt_args(Some("--include-memories")).expect("flag"));
        assert!(parse_adopt_args(Some("--force")).is_err());
        assert!(parse_adopt_args(Some("yes")).is_err());
    }

    #[test]
    fn child_arguments_are_fixed_and_carry_no_model_text() {
        let argv = child_args(
            Path::new("/usr/bin/ee"),
            Path::new("/state/run"),
            Path::new("/work"),
            true,
            true,
        );
        let rendered: Vec<String> =
            argv.iter().map(|value| value.to_string_lossy().to_string()).collect();
        assert_eq!(
            rendered,
            vec![
                "/usr/bin/ee",
                "do",
                "agent",
                "harness",
                "adopt",
                "--run-root",
                "/state/run",
                "--workspace",
                "/work",
                "--include-memories",
                "--dry-run",
            ]
        );
        let apply = child_args(
            Path::new("/usr/bin/ee"),
            Path::new("/state/run"),
            Path::new("/work"),
            false,
            false,
        );
        let rendered: Vec<String> =
            apply.iter().map(|value| value.to_string_lossy().to_string()).collect();
        assert!(rendered.iter().any(|value| value == "--yes"));
        assert!(!rendered.iter().any(|value| value == "--include-memories"));
    }

    #[cfg(unix)]
    fn fake_ee(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("ee");
        std::fs::write(
            &path,
            "#!/bin/sh\nworkspace=\"\"\nprevious=\"\"\nfor arg in \"$@\"; do\n  if [ \"$previous\" = \"--workspace\" ]; then workspace=\"$arg\"; fi\n  previous=\"$arg\"\ndone\ncase \" $* \" in\n  *\" --dry-run \"*) echo \"ee: adopt + guardrails.md\"; exit 0 ;;\n  *\" --yes \"*) mkdir -p \"$workspace/.agents\"; printf applied > \"$workspace/.agents/applied\"; echo \"ee: harness-adopt applied\"; exit 0 ;;\nesac\nexit 2\n",
        )
        .expect("fake ee");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fake ee executable");
        path
    }

    #[cfg(unix)]
    struct AdoptFixture {
        workspace: PathBuf,
        log: Arc<SessionLog>,
        runtime: Arc<OrchestratorRuntime>,
        sink: UpdateSink,
        sink_rx: tokio::sync::mpsc::UnboundedReceiver<ee_agent_acp_server::server::OutboundEvent>,
        client: ClientBridge,
        responder: tokio::task::JoinHandle<()>,
    }

    #[cfg(unix)]
    fn adopt_fixture(root: &Path, action: &'static str) -> AdoptFixture {
        use ee_agent_acp_server::server::OutboundEvent;
        use ee_agent_protocol::{RawJsonRpcMessage, Response};
        use tokio::sync::mpsc;

        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join(".agents")).expect("workspace");
        std::fs::write(workspace.join(".agents/guardrails.md"), "old\n").expect("old rule");
        let run_root = root.join("run");
        let adopted = run_root.join("adopted/.agents");
        std::fs::create_dir_all(&adopted).expect("adopted");
        std::fs::write(adopted.join("guardrails.md"), "new\n").expect("new rule");

        let log = Arc::new(SessionLog::in_memory("session-1"));
        log.record_harness_ledger(crate::session_log::HarnessLedgerReport {
            task_id: String::from("task"),
            run_root: Some(run_root.display().to_string()),
            ..crate::session_log::HarnessLedgerReport::default()
        })
        .expect("ledger");

        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let client = ClientBridge::new_for_test(Duration::from_secs(5), client_tx);
        let responder_bridge = client.clone();
        let responder = tokio::spawn(async move {
            while let Some(event) = client_rx.recv().await {
                if let OutboundEvent::ClientRequest { frame } = event
                    && let RawJsonRpcMessage::Request(request) = frame
                {
                    responder_bridge.handle_response(Response::new(
                        request.id,
                        Ok(serde_json::json!({ "action": action })),
                    ));
                }
            }
        });

        let (sink_tx, sink_rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), sink_tx);
        let runtime = Arc::new(OrchestratorRuntime::new(
            crate::config::OrchestratorConfig::default(),
            Arc::new(crate::test_support::FakeModel::new(Vec::new()))
                as Arc<dyn crate::model::ModelAdapter>,
        ));
        AdoptFixture { workspace, log, runtime, sink, sink_rx, client, responder }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accept_applies_after_the_dry_run_diff() {
        use ee_agent_acp_server::server::OutboundEvent;

        let root = tempfile::tempdir().expect("root");
        let executable = fake_ee(root.path());
        let fixture = adopt_fixture(root.path(), "accept");
        let (_cancel_tx, cancel) = watch::channel(false);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let session_id = SessionId::new("session-1");
        let result = handle_adopt_command(AdoptCommand {
            runtime: &fixture.runtime,
            sink: &fixture.sink,
            client: fixture.client.clone(),
            cancel,
            session_id: &session_id,
            instructions: None,
            workspace: &fixture.workspace,
            executable: Some(&executable),
            session_log: Some(fixture.log.clone()),
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .await
        .expect("adopt");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join(".agents/applied")).expect("applied"),
            "applied"
        );
        let mut text = String::new();
        let mut sink_rx = fixture.sink_rx;
        while let Ok(event) = sink_rx.try_recv() {
            if let OutboundEvent::Update { update, .. } = event {
                text.push_str(&format!("{update:?}"));
            }
        }
        assert!(text.contains("dry-run"), "{text}");
        assert!(text.contains("harness-adopt applied"), "{text}");
        fixture.responder.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn decline_writes_nothing() {
        use ee_agent_acp_server::server::OutboundEvent;

        let root = tempfile::tempdir().expect("root");
        let executable = fake_ee(root.path());
        let fixture = adopt_fixture(root.path(), "decline");
        let (_cancel_tx, cancel) = watch::channel(false);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let session_id = SessionId::new("session-1");
        let result = handle_adopt_command(AdoptCommand {
            runtime: &fixture.runtime,
            sink: &fixture.sink,
            client: fixture.client.clone(),
            cancel,
            session_id: &session_id,
            instructions: None,
            workspace: &fixture.workspace,
            executable: Some(&executable),
            session_log: Some(fixture.log.clone()),
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .await
        .expect("adopt");
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(!fixture.workspace.join(".agents/applied").exists(), "decline writes nothing");
        let mut text = String::new();
        let mut sink_rx = fixture.sink_rx;
        while let Ok(event) = sink_rx.try_recv() {
            if let OutboundEvent::Update { update, .. } = event {
                text.push_str(&format!("{update:?}"));
            }
        }
        assert!(text.contains("declined"), "{text}");
        fixture.responder.abort();
    }

    #[tokio::test]
    async fn missing_run_root_is_a_clear_error() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let log = Arc::new(SessionLog::in_memory("session-1"));
        let (client_tx, _client_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = ClientBridge::new_for_test(Duration::from_secs(5), client_tx);
        let (sink_tx, _sink_rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new("session-1"), sink_tx);
        let runtime = Arc::new(OrchestratorRuntime::new(
            crate::config::OrchestratorConfig::default(),
            Arc::new(crate::test_support::FakeModel::new(Vec::new()))
                as Arc<dyn crate::model::ModelAdapter>,
        ));
        let (_cancel_tx, cancel) = watch::channel(false);
        let telemetry = Arc::new(Mutex::new(TelemetryRecorder::default()));
        let session_id = SessionId::new("session-1");
        let error = handle_adopt_command(AdoptCommand {
            runtime: &runtime,
            sink: &sink,
            client,
            cancel,
            session_id: &session_id,
            instructions: None,
            workspace: &workspace,
            executable: None,
            session_log: Some(log),
            telemetry: &telemetry,
            telemetry_turn: "turn-1",
            telemetry_started_at: Instant::now(),
            telemetry_event_start: 0,
        })
        .await
        .expect_err("missing run root");
        assert!(error.to_string().contains("completed `/harness` run"), "{error}");
    }
}
