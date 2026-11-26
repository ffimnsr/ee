//! `ee do agent harness`: the recursive harness improvement driver.
//!
//! Rollouts and improver sessions run through the configured headless agent
//! host. Each session gets an isolated workspace under the run root; the user's
//! workspace harness is never mounted read-write. Improver sessions copy the
//! curated evidence dir in and the edited `.agents` tree is harvested back after
//! the turn. Child sessions run in `write` mode so they can edit `.agents` and
//! use `context/` as scratch instead of being silently read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_harnessing::{
    Channel, ChannelFuture, Deliverable, DeliverableAsset, ExperienceEntry, HarnessConfig,
    HarnessError, HarnessMode, HarnessRun, HarnessTask, HarnessTree, ImproverModel, ImproverReply,
    ImproverRequest, LibraryLocation, ReflectionSource, ReflectionToolRecord, ReflectionWriter,
    RolloutDiffFile, RolloutToolRecord, RolloutTurn, RolloutView, RunRecord, RunRequest,
    materialize, run_task, save_entry_at,
};
use ee_agent_host::AgentEvent;
use ee_agent_protocol::{ContentBlock, SessionUpdate, ToolCallStatus};
use ee_agent_remote::protocol::{Method, PermissionDecision, SessionNewResult};
use ee_agent_remote::{RemoteHost, RemotePermissionMode};
use tokio::sync::Notify;

use crate::args::{HarnessCommands, HarnessModeArg, ReflectionChoiceArg};

mod workspace;

const MAX_SNAPSHOT_DEPTH: usize = 64;
const MAX_ASSET_BYTES: usize = 64 * 1024;
const MAX_ASSETS: usize = 32;
const MAX_OUTPUT_CHARS: usize = 256 * 1024;
const MAX_SELECTOR_ASSET_TOTAL_BYTES: usize = 256 * 1024;
/// Harness child sessions are isolated under the run root, so they get the
/// write-capable mode. `ask` mode denies writes before approval and made every
/// improver session read-only.
const HARNESS_SESSION_MODE: &str = "write";

/// Runs one `ee do agent harness` subcommand.
pub(crate) fn run(command: HarnessCommands) -> io::Result<()> {
    match command {
        HarnessCommands::Improve(options) => {
            let options = *options;
            let prompt = resolve_prompt(options.prompt, options.prompt_file, options.dry_run)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            runtime.block_on(improve(ImproveOptions {
                workspace: options.workspace,
                prompt,
                domain: options.domain,
                k: options.k,
                mode: options.mode.into(),
                workers: options.workers,
                dry_run: options.dry_run,
                run_root: options.run_root,
                library_root: options.library_root,
                improver_model: options.improver_model,
                improver_effort: options.improver_effort,
                improver_steps: options.improver_steps,
                improver_timeout: options.improver_timeout,
                reflect: !options.no_reflect,
                reflection_write: options.reflection == ReflectionChoiceArg::Accept,
            }))
        }
        HarnessCommands::Adopt(options) => crate::harness_adopt_cmd::run(*options),
        HarnessCommands::Sweep(options) => crate::harness_sweep_cmd::run(*options),
        HarnessCommands::Demo { root } => {
            let root = root.unwrap_or_else(default_demo_root);
            std::fs::create_dir_all(&root)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            let run =
                runtime.block_on(ee_agent_harnessing::demo::run_demo(&root)).map_err(to_io)?;
            print_run(&run, &root.join("run"));
            println!("ee: harness demo root {}", root.display());
            Ok(())
        }
    }
}

impl From<HarnessModeArg> for HarnessMode {
    fn from(value: HarnessModeArg) -> Self {
        match value {
            HarnessModeArg::Parallel => Self::Parallel,
            HarnessModeArg::Sequential => Self::Sequential,
        }
    }
}

struct ImproveOptions {
    workspace: PathBuf,
    prompt: String,
    domain: Option<String>,
    k: usize,
    mode: HarnessMode,
    workers: usize,
    dry_run: bool,
    run_root: Option<PathBuf>,
    library_root: Option<PathBuf>,
    improver_model: Option<String>,
    improver_effort: Option<String>,
    improver_steps: usize,
    improver_timeout: u64,
    reflect: bool,
    reflection_write: bool,
}

async fn improve(options: ImproveOptions) -> io::Result<()> {
    let workspace = std::fs::canonicalize(&options.workspace).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("invalid --workspace {}: {error}", options.workspace.display()),
        )
    })?;
    if !workspace.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--workspace is not a directory"));
    }
    let domain = options.domain.clone().unwrap_or_else(|| derive_domain(&workspace));
    let task_id = task_id(&domain, &options.prompt);
    let library = LibraryLocation::resolve(
        LibraryLocation::workspace(&workspace),
        options.library_root.as_deref(),
    );
    std::fs::create_dir_all(library.root())?;
    let task = HarnessTask::new(task_id.clone(), &options.prompt, &domain, &workspace)
        .with_library_root(library.root());
    let config = HarnessConfig {
        k: options.k,
        mode: options.mode,
        dry_run: options.dry_run,
        reflect: options.reflect,
        reflection_source: if !options.reflect {
            ReflectionSource::None
        } else if options.reflection_write {
            ReflectionSource::Invocation
        } else {
            ReflectionSource::Flag
        },
        improver: ImproverModel {
            model_id: options.improver_model.clone(),
            effort: options.improver_effort.clone(),
            max_steps: options.improver_steps,
            timeout_secs: options.improver_timeout,
        },
        max_workers: options.workers,
        resume: true,
        progress: true,
    };
    config.validate().map_err(to_io)?;
    if options.dry_run {
        let plan = ee_agent_harnessing::driver::dry_run_plan(&task, &config);
        print!("{}", plan.render());
        return Ok(());
    }
    let run_root = options.run_root.clone().unwrap_or_else(|| default_run_root(&task_id));
    std::fs::create_dir_all(&run_root)?;
    let (host, handler, channel) = start_host_channel(
        &workspace,
        &run_root,
        Duration::from_secs(options.improver_timeout.max(60)),
    )?;
    let writer = CliWriter { library_root: library.root().to_path_buf() };
    let writer: Option<&dyn ReflectionWriter> =
        if options.reflection_write { Some(&writer) } else { None };
    let result = run_task(&channel, &task, &config, &run_root, writer).await;
    host.shutdown().await;
    drop(handler);
    let run = result.map_err(to_io)?;
    print_run(&run, &run_root);
    println!("ee: run root {}", run_root.display());
    Ok(())
}

/// Starts the isolated host and returns a channel bound to its event stream.
pub(crate) fn start_host_channel(
    workspace: &Path,
    run_root: &Path,
    timeout: Duration,
) -> io::Result<(RemoteHost, Arc<ee_agent_remote::HeadlessHandler>, HostChannel)> {
    let (host, handler, agent_id) = start_host(workspace, run_root)?;
    let channel = bind_host_channel(&host, agent_id, timeout)?;
    Ok((host, handler, channel))
}

/// Binds a rollout channel to a host's pump-backed event tap. Kept separate so
/// the single-owner stream contract is testable without configured agents.
fn bind_host_channel(
    host: &RemoteHost,
    agent_id: String,
    timeout: Duration,
) -> io::Result<HostChannel> {
    let events = claim_event_stream(host)?;
    let collector = Arc::new(EventCollector::default());
    spawn_collector(events, host.clone(), collector.clone());
    Ok(HostChannel { host: host.clone(), collector, agent_id, timeout })
}

/// Claims the pump-backed event tap. The pump stays the sole owner of the
/// host receiver, so approval registration, session history, and bus
/// notifications keep working while the collector observes the same events.
fn claim_event_stream(
    host: &RemoteHost,
) -> io::Result<tokio::sync::mpsc::UnboundedReceiver<AgentEvent>> {
    host.start_with_events()
        .map_err(|error| io::Error::other(format!("agent host: {error}")))?
        .ok_or_else(|| {
            io::Error::other("agent host did not expose its event stream; cannot watch rollouts")
        })
}

fn start_host(
    workspace: &Path,
    run_root: &Path,
) -> io::Result<(RemoteHost, Arc<ee_agent_remote::HeadlessHandler>, String)> {
    // Isolation boundary: every rollout/improver session runs under the run
    // root, and the candidate `.agents` tree is mounted in its own copied
    // workspace there. The user's workspace contributes trust/secret policy
    // only; child sessions are never granted it as an allowed root.
    let (mut config, options) = crate::serve_cmd::build_host_inputs(
        vec![workspace.to_path_buf(), run_root.to_path_buf()],
        Some(RemotePermissionMode::Ask.as_str().to_string()),
        Some(run_root.join("agent-audit.log")),
        Some(run_root.join("agent-sessions")),
    )
    .map_err(|error| io::Error::new(error.kind(), format!("agent host: {error}")))?;
    let isolated_root = std::fs::canonicalize(run_root).unwrap_or_else(|_| run_root.to_path_buf());
    config.allowed_roots.retain(|root| root == &isolated_root);
    if config.allowed_roots.is_empty() {
        config.allowed_roots.push(isolated_root);
    }
    let agent_id = config
        .default_agent
        .clone()
        .or_else(|| config.agents.keys().next().cloned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no configured agents"))?;
    let (host, handler) = RemoteHost::headless_with_options(config, options)
        .map_err(|error| io::Error::other(format!("agent host: {error}")))?;
    // Do not call `host.start()` here: `start_host_channel` starts the pump and
    // claims its event tap. Starting early would consume the receiver and the
    // harness collector would never see rollout events.
    Ok((host, handler, agent_id))
}

fn print_run(run: &HarnessRun, run_root: &Path) {
    let chosen = run.selection.chosen.as_deref().unwrap_or("none");
    println!(
        "ee: harness task {} k={} mode={} chosen={chosen}{}{}",
        run.task_id,
        run.k,
        run.mode.as_str(),
        if run.selection.fallback { " (fallback)" } else { "" },
        if run.resumed { " (resumed)" } else { "" }
    );
    println!(
        "ee: harness ledger rollouts={} sessions={} selection={} retries={} reflection={} dead={}",
        run.ledger.rollouts,
        run.ledger.sessions,
        run.ledger.selection_sessions,
        run.ledger.retry_sessions,
        run.ledger.reflection_sessions,
        run.ledger.dead_rollouts.len()
    );
    for dead in &run.ledger.dead_rollouts {
        println!("ee: harness dead rollout {}: {}", dead.label, dead.reason);
    }
    println!(
        "ee: harness library {} reflection={} source={}",
        run.library_root.display(),
        run.reflection_write.as_str(),
        run.reflection_source.as_str()
    );
    let adopted_path = run.adopted_path.as_ref().map(|path| path.display().to_string());
    let reflected_path = run.reflected_path.as_ref().map(|path| path.display().to_string());
    match serde_json::to_string(&serde_json::json!({
        "task_id": run.task_id,
        "selected_label": run.selection.chosen,
        "run_root": run_root.display().to_string(),
        "library_root": run.library_root.display().to_string(),
        "reflection_write": run.reflection_write.as_str(),
        "reflection_source": run.reflection_source.as_str(),
        "rollouts": run.ledger.rollouts,
        "sessions": run.ledger.sessions,
        "selection_sessions": run.ledger.selection_sessions,
        "retry_sessions": run.ledger.retry_sessions,
        "reflection_sessions": run.ledger.reflection_sessions,
        "dead_rollouts": run.ledger.dead_rollouts.len(),
        "base_failed": run.ledger.base_failed,
        "adopted_path": adopted_path,
        "reflected_path": reflected_path,
    })) {
        Ok(report) => println!("ee: harness-report {report}"),
        Err(error) => tracing::warn!(%error, "harness report could not be serialized"),
    }
    if let Some(path) = &run.adopted_path {
        println!("ee: adopted harness {}", path.display());
    }
    if let Some(path) = &run.reflected_path {
        println!("ee: experience entry {}", path.display());
    }
}

fn resolve_prompt(
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
    dry_run: bool,
) -> io::Result<String> {
    match (prompt, prompt_file) {
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use either --prompt or --prompt-file, not both",
        )),
        (Some(prompt), None) => Ok(prompt),
        (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("cannot read {}: {error}", path.display()))
        }),
        (None, None) if dry_run => Ok(String::from("dry-run task")),
        (None, None) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--prompt or --prompt-file is required (except with --dry-run)",
        )),
    }
}

fn derive_domain(workspace: &Path) -> String {
    let name = workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace");
    let domain: String =
        name.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' }
            })
            .collect();
    let domain = domain.trim_matches('-').to_string();
    if domain.is_empty() { String::from("workspace") } else { domain }
}

fn task_id(domain: &str, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().take(6).map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("cli-{domain}-{hex}")
}

fn default_run_root(task_id: &str) -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("ee")
        .join("harness-runs")
        .join(task_id)
}

fn default_demo_root() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("ee")
        .join("harness-demo")
}

fn to_io(error: HarnessError) -> io::Error {
    io::Error::other(error.to_string())
}

// ── Host channel ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct HostChannel {
    host: RemoteHost,
    collector: Arc<EventCollector>,
    agent_id: String,
    timeout: Duration,
}

impl Channel for HostChannel {
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
        let this = self.clone();
        Box::pin(async move {
            let workspace = prepare_rollout_workspace(&request)?;
            let before = snapshot_files(&workspace);
            let outcome = this.run_session(&workspace, &request.task.prompt, None).await?;
            let added = added_files(&workspace, &before);
            let tools = tool_states(&outcome.updates);
            let delivered = delivery(&outcome.text, &added, &workspace);
            let rollout = RolloutView {
                session_id: outcome.session_id.clone(),
                turns: vec![RolloutTurn {
                    turn: 1,
                    stop_reason: Some(outcome.stop_reason.clone()),
                    tools: tools
                        .iter()
                        .enumerate()
                        .map(|(index, tool)| RolloutToolRecord {
                            turn: 1,
                            iteration: index + 1,
                            tool: tool.name.clone(),
                            success: tool.success,
                            error_kind: (!tool.success).then(|| String::from("tool")),
                            duration_ms: 0,
                        })
                        .collect(),
                }],
                diff: added
                    .iter()
                    .map(|path| RolloutDiffFile {
                        path: path.display().to_string(),
                        change: "added",
                    })
                    .collect(),
                final_deliverable: Some(outcome.text.clone()),
            };
            let tool_records = tools
                .iter()
                .enumerate()
                .map(|(index, tool)| ReflectionToolRecord {
                    turn: 1,
                    iteration: (index + 1) as u64,
                    tool: tool.name.clone(),
                    success: tool.success,
                    summary: tool.title.clone(),
                })
                .collect();
            let workspace_diff = if added.is_empty() {
                None
            } else {
                Some(
                    added
                        .iter()
                        .map(|path| format!("- added {}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            };
            Ok(RunRecord {
                label: request.label,
                delivered,
                rollout,
                tool_records,
                workspace_diff,
                sessions: 1,
                rollouts: 1,
                usage_tokens: None,
            })
        })
    }

    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
        let this = self.clone();
        Box::pin(async move {
            std::fs::create_dir_all(&request.candidate_dir)?;
            if !request.candidate_dir.join(".agents").is_dir() {
                let base = HarnessTree::load_components(&request.base_harness.join(".agents"))
                    .map_err(HarnessError::Validation)?;
                materialize(&base, &request.candidate_dir.join(".agents"))
                    .map_err(HarnessError::Validation)?;
            }
            copy_tree(&request.context_dir, &request.candidate_dir.join("context"))?;
            let prompt = improver_prompt(&request);
            let outcome =
                this.run_session(&request.candidate_dir, &prompt, Some(&request.model)).await?;
            let tree = HarnessTree::load_components(&request.candidate_dir.join(".agents"))
                .map_err(HarnessError::Validation)?;
            Ok(ImproverReply { tree, notes: outcome.text })
        })
    }
}

impl HostChannel {
    async fn run_session(
        &self,
        cwd: &Path,
        prompt: &str,
        model: Option<&ImproverModel>,
    ) -> Result<SessionOutcome, HarnessError> {
        let session: SessionNewResult = serde_json::from_value(
            self.host
                .dispatch(
                    Method::SessionNew,
                    Some(serde_json::json!({
                        "agentId": self.agent_id,
                        "cwd": cwd.display().to_string(),
                    })),
                )
                .await
                .map_err(|error| HarnessError::Channel(error.to_string()))?,
        )
        .map_err(|error| HarnessError::Channel(format!("session/new reply: {error}")))?;
        if let Err(error) = self.apply_session_mode(&session.session_id).await {
            self.close_session(&session.session_id).await;
            return Err(error);
        }
        // Effort and step budgets are advisories the host cannot enforce; the
        // model id is applied through the agent's `session/set_model`.
        if let Some(model_id) = model.and_then(|model| model.model_id.as_deref())
            && let Err(error) = self
                .host
                .dispatch(
                    Method::SessionSetModel,
                    Some(serde_json::json!({
                        "sessionId": session.session_id,
                        "modelId": model_id,
                    })),
                )
                .await
        {
            self.close_session(&session.session_id).await;
            return Err(HarnessError::Channel(format!(
                "cannot apply improver model {model_id:?}: {error}"
            )));
        }
        self.collector.active.lock().expect("active sessions").insert(session.session_id.clone());
        let started = self
            .host
            .dispatch(
                Method::TurnStart,
                Some(serde_json::json!({
                    "sessionId": session.session_id,
                    "text": prompt,
                })),
            )
            .await;
        let result = match started {
            Ok(_) => self.wait_for_terminal(&session.session_id).await,
            Err(error) => Err(HarnessError::Channel(error.to_string())),
        };
        let stop_reason = match &result {
            Ok(stop) => stop.clone(),
            Err(_) => String::from("failed"),
        };
        let updates = self.collector.take(&session.session_id);
        self.close_session(&session.session_id).await;
        self.collector.active.lock().expect("active sessions").remove(&session.session_id);
        result?;
        let text = assistant_text(&updates);
        Ok(SessionOutcome { session_id: session.session_id, stop_reason, text, updates })
    }

    async fn apply_session_mode(&self, session: &str) -> Result<(), HarnessError> {
        self.host
            .dispatch(
                Method::SessionSetMode,
                Some(serde_json::json!({
                    "sessionId": session,
                    "modeId": HARNESS_SESSION_MODE,
                })),
            )
            .await
            .map_err(|error| {
                HarnessError::Channel(format!(
                    "cannot apply harness session mode {HARNESS_SESSION_MODE:?}: {error}"
                ))
            })?;
        Ok(())
    }

    async fn close_session(&self, session: &str) {
        let _ = self
            .host
            .dispatch(Method::SessionClose, Some(serde_json::json!({ "sessionId": session })))
            .await;
    }

    async fn wait_for_terminal(&self, session: &str) -> Result<String, HarnessError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let notified = self.collector.notify.notified();
            if let Some(error) = self.collector.failed.lock().expect("failed").get(session) {
                return Err(HarnessError::Channel(error.clone()));
            }
            if let Some(reason) = self.collector.done.lock().expect("done").get(session) {
                return Ok(reason.clone());
            }
            match tokio::time::timeout_at(deadline, notified).await {
                Ok(()) => {}
                Err(_) => {
                    let _ = self
                        .host
                        .dispatch(
                            Method::TurnInterrupt,
                            Some(serde_json::json!({ "sessionId": session })),
                        )
                        .await;
                    return Err(HarnessError::Channel(format!(
                        "agent turn exceeded {:?}",
                        self.timeout
                    )));
                }
            }
        }
    }
}

struct SessionOutcome {
    session_id: String,
    stop_reason: String,
    text: String,
    updates: Vec<SessionUpdate>,
}

fn improver_prompt(request: &ImproverRequest) -> String {
    format!(
        "{}\n\nThis workspace is isolated to this session.\n\
         - Read `context/INDEX.md` first; it lists every evidence file.\n\
         - `context/` is writable scratch for notes while you work.\n\
         - `.agents/` is the harness you may edit.\n\
         Edit `.agents/` and return a short summary of the changes.\n\
         Never read or write outside this workspace.\n",
        request.mission.trim()
    )
}

// ── Event collection ─────────────────────────────────────────────────────────

#[derive(Default)]
struct EventCollector {
    updates: Mutex<BTreeMap<String, Vec<SessionUpdate>>>,
    done: Mutex<BTreeMap<String, String>>,
    failed: Mutex<BTreeMap<String, String>>,
    active: Mutex<BTreeSet<String>>,
    notify: Notify,
}

impl EventCollector {
    fn take(&self, session: &str) -> Vec<SessionUpdate> {
        self.updates.lock().expect("updates").remove(session).unwrap_or_default()
    }
}

fn spawn_collector(
    mut events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    host: RemoteHost,
    collector: Arc<EventCollector>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::SessionUpdate { session_id, update } => {
                    collector
                        .updates
                        .lock()
                        .expect("updates")
                        .entry(session_id.to_string())
                        .or_default()
                        .push(*update);
                }
                AgentEvent::TurnCompleted { session_id, stop_reason, .. } => {
                    collector
                        .done
                        .lock()
                        .expect("done")
                        .insert(session_id.to_string(), format!("{stop_reason:?}"));
                    collector.notify.notify_waiters();
                }
                AgentEvent::TurnFailed { session_id, error, .. } => {
                    collector
                        .failed
                        .lock()
                        .expect("failed")
                        .insert(session_id.to_string(), error.to_string());
                    collector.notify.notify_waiters();
                }
                AgentEvent::TurnCancelled { session_id, .. } => {
                    collector
                        .failed
                        .lock()
                        .expect("failed")
                        .insert(session_id.to_string(), String::from("turn cancelled"));
                    collector.notify.notify_waiters();
                }
                AgentEvent::PermissionRequested { session_id, request, .. } => {
                    // Rollouts and improver sessions run against isolated copies
                    // under the run root and the run is explicitly invoked, so
                    // this host answers its own child sessions' approvals.
                    if collector.active.lock().expect("active").contains(&session_id.to_string())
                        && let Err(error) = host.approvals().respond(
                            request.request_id.0,
                            PermissionDecision::AllowAlways,
                            None,
                            "harness",
                        )
                    {
                        tracing::warn!(%error, "harness approval could not be answered");
                    }
                }
                _ => {}
            }
        }
    });
}

fn assistant_text(updates: &[SessionUpdate]) -> String {
    let mut text = String::new();
    for update in updates {
        if let SessionUpdate::AgentMessageChunk(chunk) = update
            && let ContentBlock::Text(content) = &chunk.content
        {
            text.push_str(&content.text);
        }
    }
    truncate(&text, MAX_OUTPUT_CHARS)
}

struct ToolState {
    name: String,
    title: String,
    success: bool,
}

fn tool_states(updates: &[SessionUpdate]) -> Vec<ToolState> {
    let mut tools: BTreeMap<String, ToolState> = BTreeMap::new();
    for update in updates {
        match update {
            SessionUpdate::ToolCall(call) => {
                tools.insert(
                    call.tool_call_id.to_string(),
                    ToolState {
                        name: call.name.clone().unwrap_or_else(|| call.title.clone()),
                        title: call.title.clone(),
                        success: call.status == ToolCallStatus::Completed,
                    },
                );
            }
            SessionUpdate::ToolCallUpdate(patch) => {
                let entry =
                    tools.entry(patch.tool_call_id.to_string()).or_insert_with(|| ToolState {
                        name: patch
                            .fields
                            .name
                            .clone()
                            .or_else(|| patch.fields.title.clone())
                            .unwrap_or_else(|| String::from("tool")),
                        title: patch.fields.title.clone().unwrap_or_default(),
                        success: false,
                    });
                if let Some(name) = &patch.fields.name {
                    entry.name = name.clone();
                }
                if let Some(title) = &patch.fields.title {
                    entry.title = title.clone();
                }
                if let Some(status) = &patch.fields.status {
                    entry.success = *status == ToolCallStatus::Completed;
                }
            }
            _ => {}
        }
    }
    tools.into_values().collect()
}

// ── Rollout workspace preparation ────────────────────────────────────────────

fn prepare_rollout_workspace(request: &RunRequest) -> Result<PathBuf, HarnessError> {
    let workspace = request.scratch_root.join("workspace");
    if workspace.exists() {
        std::fs::remove_dir_all(&workspace)?;
    }
    std::fs::create_dir_all(&workspace)?;
    workspace::copy_workspace(&request.task.workspace, &workspace)?;
    let agents = workspace.join(".agents");
    if agents.exists() {
        std::fs::remove_dir_all(&agents)?;
    }
    let tree = HarnessTree::load_components(&request.harness_root.join(".agents"))
        .map_err(HarnessError::Validation)?;
    materialize(&tree, &agents).map_err(HarnessError::Validation)?;
    Ok(workspace)
}

fn snapshot_files(workspace: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    collect_files(workspace, workspace, 0, &mut files);
    files
}

fn collect_files(root: &Path, directory: &Path, depth: usize, files: &mut BTreeSet<PathBuf>) {
    if depth > MAX_SNAPSHOT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if name == ".agents" {
                continue;
            }
            collect_files(root, &path, depth + 1, files);
        } else if file_type.is_file() {
            files.insert(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
}

fn added_files(workspace: &Path, before: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    snapshot_files(workspace)
        .into_iter()
        .filter(|path| !before.contains(path))
        .take(MAX_ASSETS)
        .collect()
}

fn delivery(text: &str, added: &[PathBuf], workspace: &Path) -> Option<Deliverable> {
    if text.trim().is_empty() && added.is_empty() {
        return None;
    }
    // Selector evidence must show rendered artifacts, not just their names.
    // Each asset is bounded and the total is capped so one run cannot flood
    // the selection context.
    let mut remaining = MAX_SELECTOR_ASSET_TOTAL_BYTES;
    let assets = added
        .iter()
        .map(|path| {
            let full = workspace.join(path);
            let bytes = std::fs::symlink_metadata(&full)
                .map(|metadata| metadata.len() as usize)
                .unwrap_or(0);
            let content = read_asset_text(&full, MAX_ASSET_BYTES.min(remaining));
            if let Some(text) = &content {
                remaining = remaining.saturating_sub(text.len());
            }
            DeliverableAsset { name: path.display().to_string(), bytes, content }
        })
        .collect();
    Some(Deliverable { text: text.to_string(), assets })
}

fn read_asset_text(path: &Path, limit: usize) -> Option<String> {
    if limit == 0 {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

// ── Reflection persistence ────────────────────────────────────────────────

pub(crate) struct CliWriter {
    pub(crate) library_root: PathBuf,
}

impl ReflectionWriter for CliWriter {
    fn write(
        &self,
        entry: &ExperienceEntry,
        harness: &HarnessTree,
    ) -> Result<PathBuf, HarnessError> {
        save_entry_at(&self.library_root, entry, Some(harness)).map_err(HarnessError::State)
    }
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), HarnessError> {
    if !source.is_dir() {
        return Err(HarnessError::State(format!(
            "context directory is missing: {}",
            source.display()
        )));
    }
    let mut files = Vec::new();
    collect_copy_files(source, source, 0, &mut files)?;
    for relative in files {
        let from = source.join(&relative);
        let to = target.join(&relative);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to)?;
    }
    Ok(())
}

fn collect_copy_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), HarnessError> {
    if depth > MAX_SNAPSHOT_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory)?;
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_copy_files(root, &path, depth + 1, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]", &text[..cut])
}

#[cfg(test)]
mod tests;
