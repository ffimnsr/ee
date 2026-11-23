//! `ee do agent harness`: the recursive harness improvement driver.
//!
//! Rollouts and improver sessions run through the configured headless agent
//! host. Each session gets an isolated, bounded workspace under the run root;
//! the user's workspace harness is never mounted read-write. Improver sessions
//! copy the curated evidence dir in and the edited `.agents` tree is harvested
//! back after the turn.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_harnessing::{
    Channel, ChannelFuture, Deliverable, DeliverableAsset, ExperienceEntry, HarnessConfig,
    HarnessError, HarnessMode, HarnessRun, HarnessTask, HarnessTree, ImproverModel, ImproverReply,
    ImproverRequest, ReflectionToolRecord, ReflectionWriter, RolloutDiffFile, RolloutToolRecord,
    RolloutTurn, RolloutView, RunRecord, RunRequest, materialize, run_task, save_entry,
};
use ee_agent_host::AgentEvent;
use ee_agent_protocol::{ContentBlock, SessionUpdate, ToolCallStatus};
use ee_agent_remote::protocol::{Method, PermissionDecision, SessionNewResult};
use ee_agent_remote::{RemoteHost, RemotePermissionMode};
use tokio::sync::Notify;

use crate::args::{HarnessCommands, HarnessModeArg};

const MAX_WORKSPACE_FILES: usize = 512;
const MAX_WORKSPACE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WORKSPACE_DEPTH: usize = 4;
const MAX_ASSET_BYTES: usize = 64 * 1024;
const MAX_ASSETS: usize = 32;
const MAX_OUTPUT_CHARS: usize = 16 * 1024;

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
                approve_reflection: options.yes,
            }))
        }
        HarnessCommands::Demo { root } => {
            let root = root.unwrap_or_else(default_demo_root);
            std::fs::create_dir_all(&root)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            let run =
                runtime.block_on(ee_agent_harnessing::demo::run_demo(&root)).map_err(to_io)?;
            print_run(&run);
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
    approve_reflection: bool,
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
    let mut task = HarnessTask::new(task_id.clone(), &options.prompt, &domain, &workspace);
    if let Some(library_root) = &options.library_root {
        std::fs::create_dir_all(library_root)?;
        task = task.with_library_root(library_root);
    }
    let config = HarnessConfig {
        k: options.k,
        mode: options.mode,
        dry_run: options.dry_run,
        reflect: true,
        improver: ImproverModel {
            model_id: options.improver_model.clone(),
            effort: options.improver_effort.clone(),
            max_steps: options.improver_steps,
            timeout_secs: options.improver_timeout,
        },
        max_workers: options.workers,
        resume: true,
    };
    if options.dry_run {
        let plan = ee_agent_harnessing::driver::dry_run_plan(&task, &config);
        print!("{}", plan.render());
        return Ok(());
    }
    let run_root = options.run_root.clone().unwrap_or_else(|| default_run_root(&task_id));
    std::fs::create_dir_all(&run_root)?;
    let (host, handler, agent_id) = start_host(&workspace, &run_root)?;
    let events = host.take_events().ok_or_else(|| {
        io::Error::other("agent host did not expose its event stream; cannot watch rollouts")
    })?;
    let collector = Arc::new(EventCollector::default());
    spawn_collector(events, host.clone(), collector.clone());
    let channel = HostChannel {
        host: host.clone(),
        collector,
        agent_id,
        timeout: Duration::from_secs(options.improver_timeout.max(60)),
    };
    let writer = CliWriter {
        workspace: options.library_root.clone().unwrap_or_else(|| workspace.clone()),
        approve: options.approve_reflection,
    };
    let result = run_task(&channel, &task, &config, &run_root, Some(&writer)).await;
    host.shutdown().await;
    drop(handler);
    let run = result.map_err(to_io)?;
    print_run(&run);
    println!("ee: run root {}", run_root.display());
    Ok(())
}

fn start_host(
    workspace: &Path,
    run_root: &Path,
) -> io::Result<(RemoteHost, Arc<ee_agent_remote::HeadlessHandler>, String)> {
    // Isolation boundary: each rollout/improver session gets its own bounded
    // workspace under the run root, and the candidate `.agents` tree is mounted
    // there. The configured agent process itself keeps its normal environment
    // and OS permissions; deploy harness runs inside an extra sandbox when the
    // task's inputs are untrusted.
    let (config, options) = crate::serve_cmd::build_host_inputs(
        vec![workspace.to_path_buf(), run_root.to_path_buf()],
        Some(RemotePermissionMode::Ask.as_str().to_string()),
        None,
        None,
    )
    .map_err(|error| io::Error::new(error.kind(), format!("agent host: {error}")))?;
    let agent_id = config
        .default_agent
        .clone()
        .or_else(|| config.agents.keys().next().cloned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no configured agents"))?;
    let (host, handler) = RemoteHost::headless_with_options(config, options)
        .map_err(|error| io::Error::other(format!("agent host: {error}")))?;
    host.start().map_err(|error| io::Error::other(format!("agent host: {error}")))?;
    Ok((host, handler, agent_id))
}

fn print_run(run: &HarnessRun) {
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
    match serde_json::to_string(&serde_json::json!({
        "task_id": run.task_id,
        "rollouts": run.ledger.rollouts,
        "sessions": run.ledger.sessions,
        "selection_sessions": run.ledger.selection_sessions,
        "retry_sessions": run.ledger.retry_sessions,
        "reflection_sessions": run.ledger.reflection_sessions,
        "dead_rollouts": run.ledger.dead_rollouts.len(),
        "base_failed": run.ledger.base_failed,
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
struct HostChannel {
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
            let _ = self
                .host
                .dispatch(
                    Method::SessionClose,
                    Some(serde_json::json!({ "sessionId": session.session_id })),
                )
                .await;
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
        let _ = self
            .host
            .dispatch(
                Method::SessionClose,
                Some(serde_json::json!({ "sessionId": session.session_id })),
            )
            .await;
        self.collector.active.lock().expect("active sessions").remove(&session.session_id);
        result?;
        let text = assistant_text(&updates);
        Ok(SessionOutcome { session_id: session.session_id, stop_reason, text, updates })
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
         - `context/` contains the curated evidence for this stage.\n\
         - `.agents/` is the harness you may edit.\n\
         Edit files under `.agents/` only and return a short summary of the changes.\n\
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
    copy_workspace(&request.task.workspace, &workspace)?;
    let agents = workspace.join(".agents");
    if agents.exists() {
        std::fs::remove_dir_all(&agents)?;
    }
    let tree = HarnessTree::load_components(&request.harness_root.join(".agents"))
        .map_err(HarnessError::Validation)?;
    materialize(&tree, &agents).map_err(HarnessError::Validation)?;
    Ok(workspace)
}

fn copy_workspace(source: &Path, target: &Path) -> Result<(), HarnessError> {
    let mut budget = CopyBudget { files: 0, bytes: 0 };
    copy_dir(source, target, source, 0, &mut budget)
}

struct CopyBudget {
    files: usize,
    bytes: u64,
}

fn copy_dir(
    root: &Path,
    target: &Path,
    directory: &Path,
    depth: usize,
    budget: &mut CopyBudget,
) -> Result<(), HarnessError> {
    const IGNORED: &[&str] =
        &["target", "node_modules", ".git", ".atlas", "dist", "build", ".agents"];
    if depth > MAX_WORKSPACE_DEPTH || budget.files >= MAX_WORKSPACE_FILES {
        return Ok(());
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if budget.files >= MAX_WORKSPACE_FILES || budget.bytes >= MAX_WORKSPACE_BYTES {
            return Ok(());
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if IGNORED.contains(&name) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let destination = target.join(relative);
        if path.is_dir() {
            std::fs::create_dir_all(&destination)?;
            copy_dir(root, target, &path, depth + 1, budget)?;
        } else if path.is_file() {
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.len() > MAX_ASSET_BYTES as u64 {
                continue;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::copy(&path, &destination).is_ok() {
                budget.files += 1;
                budget.bytes += metadata.len();
            }
        }
    }
    Ok(())
}

fn snapshot_files(workspace: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    collect_files(workspace, workspace, 0, &mut files);
    files
}

fn collect_files(root: &Path, directory: &Path, depth: usize, files: &mut BTreeSet<PathBuf>) {
    if depth > MAX_WORKSPACE_DEPTH || files.len() >= MAX_WORKSPACE_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if path.is_dir() {
            if name == ".agents" {
                continue;
            }
            collect_files(root, &path, depth + 1, files);
        } else if path.is_file() {
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
    let assets = added
        .iter()
        .map(|path| {
            let bytes = std::fs::symlink_metadata(workspace.join(path))
                .map(|metadata| metadata.len() as usize)
                .unwrap_or(0);
            DeliverableAsset { name: path.display().to_string(), bytes, content: None }
        })
        .collect();
    Some(Deliverable { text: text.to_string(), assets })
}

// ── Reflection approval ──────────────────────────────────────────────────────

struct CliWriter {
    workspace: PathBuf,
    approve: bool,
}

impl ReflectionWriter for CliWriter {
    fn write(
        &self,
        entry: &ExperienceEntry,
        harness: &HarnessTree,
    ) -> Result<PathBuf, HarnessError> {
        let approved = self.approve || ask_approval(entry);
        if !approved {
            return Err(HarnessError::State(String::from("reflection write declined")));
        }
        save_entry(&self.workspace, entry, Some(harness)).map_err(HarnessError::State)
    }
}

fn ask_approval(entry: &ExperienceEntry) -> bool {
    if !io::stdin().is_terminal() {
        eprintln!("ee: harness reflection declined (no terminal for approval; pass --yes)");
        return false;
    }
    let lesson = entry.lesson.as_deref().unwrap_or("NO_ENTRY (edits only)");
    eprint!(
        "ee: write experience {}/{} with {} edit row(s) and lesson: {lesson}? [y/N] ",
        entry.domain,
        entry.seq,
        entry.edits.len()
    );
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
    if depth > 4 || files.len() > 256 {
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
mod tests {
    use super::*;

    #[test]
    fn delivery_combines_final_text_and_added_files() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("out.txt"), "done\n").expect("asset");
        let added = vec![PathBuf::from("out.txt")];
        let deliverable = delivery("wrote it", &added, root.path()).expect("delivered");
        assert_eq!(deliverable.text, "wrote it");
        assert_eq!(deliverable.assets.len(), 1);
        assert_eq!(deliverable.assets[0].name, "out.txt");
        assert_eq!(deliverable.assets[0].bytes, 5);
        assert!(delivery("", &[], root.path()).is_none());
        assert!(delivery("", &added, root.path()).is_some());
    }

    #[test]
    fn task_id_and_domain_are_stable() {
        let workspace = Path::new("/work/My Project");
        assert_eq!(derive_domain(workspace), "my-project");
        let first = task_id("my-project", "do the task");
        assert_eq!(first, task_id("my-project", "do the task"));
        assert_ne!(first, task_id("my-project", "a different task"));
        assert!(first.starts_with("cli-my-project-"));
    }

    #[test]
    fn rollout_workspace_mounts_the_candidate_and_copies_sources() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).expect("workspace");
        std::fs::write(workspace.join("src/lib.rs"), "pub fn one() {}\n").expect("source");
        let harness = root.path().join("harness");
        std::fs::create_dir_all(harness.join(".agents")).expect("harness");
        std::fs::write(harness.join(".agents/guardrails.md"), "candidate rule\n").expect("rule");
        let request = RunRequest {
            label: String::from("ref-0"),
            task: HarnessTask::new("t", "do work", "rust", &workspace),
            harness_root: harness,
            scratch_root: root.path().join("scratch"),
        };
        let prepared = prepare_rollout_workspace(&request).expect("prepared");
        assert_eq!(
            std::fs::read_to_string(prepared.join("src/lib.rs")).expect("source"),
            "pub fn one() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(prepared.join(".agents/guardrails.md")).expect("rule"),
            "candidate rule\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(".agents/guardrails.md")).unwrap_or_default(),
            "",
            "the task workspace harness is never mutated"
        );
    }
}
