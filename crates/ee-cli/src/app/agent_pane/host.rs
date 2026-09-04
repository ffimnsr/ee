//! Host worker bridge: `AgentHostBridge`, `HostCommand`, worker loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use ee_agent_host::{
    AgentError, AgentEvent, AgentManager, AgentThread, CriticAgentBroker, CriticRevisionObserver,
    ExternalCriticConfig, ExternalCritiqueOutcome, ExternalCritiqueRequest,
};
use ee_agent_protocol::{
    ContentBlock, McpServer, McpServerStdio, SessionConfigOptionValue, SessionModeId,
};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::{mpsc as tokio_mpsc, watch};

use super::constants::{AGENT_LIFECYCLE_CONCURRENCY, DEFAULT_AGENT_MODE_ID};

// ── Host bridge ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(super) struct FixedCriticRevision(String);

impl CriticRevisionObserver for FixedCriticRevision {
    fn current_revision(&self, _worktree_roots: &[PathBuf]) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

/// Commands the UI enqueues for the host worker thread.
pub(super) enum HostCommand {
    NewSession {
        agent_id: String,
        roots: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        /// Stdio `ee --mcp-proxy` fallback entry (proxy mode on); the host
        /// swaps it for an ACP-native entry when the agent supports
        /// MCP-over-ACP (Phase 6b).
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
        reply: std_mpsc::Sender<Result<AgentThread, String>>,
    },
    ReconnectSession {
        agent_id: String,
        session_id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        reply: std_mpsc::Sender<Result<AgentThread, String>>,
    },
    SendPrompt {
        thread: AgentThread,
        blocks: Vec<ContentBlock>,
    },
    ResumePrompt {
        thread: AgentThread,
        blocks: Vec<ContentBlock>,
    },
    SetMode {
        thread: AgentThread,
        mode_id: SessionModeId,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    SetConfigOption {
        thread: AgentThread,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Cancel {
        thread: AgentThread,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    ExternalCritique {
        config: ExternalCriticConfig,
        timeout: Duration,
        output_limit: usize,
        request: ExternalCritiqueRequest,
        cancel: watch::Receiver<bool>,
        reply: std_mpsc::Sender<ExternalCritiqueOutcome>,
    },
    Shutdown,
}

pub(super) async fn ensure_default_agent_mode(
    result: Result<AgentThread, AgentError>,
) -> Result<AgentThread, AgentError> {
    let thread = result?;
    thread.ensure_mode(SessionModeId::new(DEFAULT_AGENT_MODE_ID)).await?;
    Ok(thread)
}

/// Runs async host operations on a dedicated worker thread.
///
/// The whole loop runs inside `block_on` and awaits commands over a tokio
/// channel, so the single-threaded runtime keeps driving spawned tasks
/// (connection driver, permission responders, elicitation handler futures)
/// even while no command is queued. Lifecycle operations run in bounded tasks:
/// connection drivers preserve protocol ordering while independent sessions
/// can overlap without wedging later control commands.
pub(super) fn host_worker(
    runtime: tokio::runtime::Runtime,
    manager: AgentManager,
    mut rx: tokio_mpsc::UnboundedReceiver<HostCommand>,
) {
    runtime.block_on(async move {
        let lifecycle_slots = Arc::new(tokio::sync::Semaphore::new(AGENT_LIFECYCLE_CONCURRENCY));
        let mut lifecycle_tasks = tokio::task::JoinSet::new();
        while let Some(command) = rx.recv().await {
            match command {
                HostCommand::NewSession {
                    agent_id,
                    roots,
                    mcp_servers,
                    ee_proxy_stdio_fallback,
                    reply,
                } => {
                    let manager = manager.clone();
                    let lifecycle_slots = lifecycle_slots.clone();
                    lifecycle_tasks.spawn(async move {
                        let Ok(_slot) = lifecycle_slots.acquire_owned().await else {
                            let _ =
                                reply.send(Err(String::from("agent lifecycle scheduler stopped")));
                            return;
                        };
                        let result = manager
                            .new_session(&agent_id, roots, mcp_servers, ee_proxy_stdio_fallback)
                            .await;
                        let result = ensure_default_agent_mode(result).await;
                        let _ = reply.send(result.map_err(|error| error.to_string()));
                    });
                }
                HostCommand::ReconnectSession {
                    agent_id,
                    session_id,
                    cwd,
                    additional_directories,
                    mcp_servers,
                    reply,
                } => {
                    let manager = manager.clone();
                    let lifecycle_slots = lifecycle_slots.clone();
                    lifecycle_tasks.spawn(async move {
                        let Ok(_slot) = lifecycle_slots.acquire_owned().await else {
                            let _ =
                                reply.send(Err(String::from("agent lifecycle scheduler stopped")));
                            return;
                        };
                        // Prefer `session/load`: it replays the conversation into
                        // the client. Fall back to `session/resume` only when load
                        // is not advertised.
                        let session_id = ee_agent_protocol::SessionId::new(session_id);
                        let result = match manager
                            .load_session(
                                &agent_id,
                                session_id.clone(),
                                cwd.clone(),
                                additional_directories.clone(),
                                mcp_servers.clone(),
                            )
                            .await
                        {
                            Ok(thread) => Ok(thread),
                            Err(AgentError::CapabilityUnsupported { method })
                                if method == "session/load" =>
                            {
                                manager
                                    .resume_session(
                                        &agent_id,
                                        session_id,
                                        cwd,
                                        additional_directories,
                                        mcp_servers,
                                    )
                                    .await
                            }
                            Err(error) => Err(error),
                        };
                        let result = ensure_default_agent_mode(result).await;
                        let _ = reply.send(result.map_err(|error| error.to_string()));
                    });
                }
                HostCommand::SendPrompt { thread, blocks } => {
                    // Detached: the turn streams through host events, and a
                    // later `Cancel` command must be able to run while the
                    // prompt is still in flight (sequential execution would
                    // deadlock against an agent that waits for the cancel
                    // before answering the prompt).  Terminal turn events
                    // (completed/cancelled/failed) arrive through the event
                    // stream; the host's `send_prompt` owns them.
                    std::mem::drop(tokio::spawn(async move { thread.send_prompt(blocks).await }));
                }
                HostCommand::ResumePrompt { thread, blocks } => {
                    std::mem::drop(tokio::spawn(async move { thread.resume_prompt(blocks).await }));
                }
                HostCommand::SetMode { thread, mode_id, reply } => {
                    let message = format!("mode set: {}", mode_id.0);
                    let result = thread.set_mode(mode_id).await.map(|()| message);
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::SetConfigOption { thread, config_id, value, reply } => {
                    let message = format!("config set: {}", config_id.0);
                    let result = thread.set_config_option(config_id, value).await.map(|()| message);
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::Cancel { thread, reply } => {
                    let result = thread.cancel().await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::ExternalCritique {
                    config,
                    timeout,
                    output_limit,
                    request,
                    cancel,
                    reply,
                } => {
                    let manager = manager.clone();
                    lifecycle_tasks.spawn(async move {
                        let observer = Arc::new(FixedCriticRevision(request.revision.clone()));
                        let outcome = match CriticAgentBroker::with_timeout(
                            &manager, config, timeout, observer,
                        )
                        .and_then(|broker| broker.with_output_limit(output_limit))
                        {
                            Ok(broker) => broker.critique(request, cancel).await,
                            Err(error) => ExternalCritiqueOutcome::Failed {
                                reason: error.to_string().chars().take(512).collect(),
                            },
                        };
                        let _ = reply.send(outcome);
                    });
                }
                HostCommand::Shutdown => break,
            }
        }
        lifecycle_tasks.shutdown().await;
        let _ = manager.shutdown().await;
    });
}

/// Owns the lazy host: manager, event receiver, and worker thread.
pub(crate) struct AgentHostBridge {
    pub(crate) manager: AgentManager,
    pub(crate) events: tokio_mpsc::UnboundedReceiver<AgentEvent>,
    commands: tokio_mpsc::UnboundedSender<HostCommand>,
}

impl AgentHostBridge {
    pub(super) fn new(
        manager: AgentManager,
        events: tokio_mpsc::UnboundedReceiver<AgentEvent>,
    ) -> Self {
        let (commands_tx, commands_rx) = tokio_mpsc::unbounded_channel();
        let runtime =
            TokioBuilder::new_current_thread().enable_all().build().expect("agents host runtime");
        let worker_manager = manager.clone();
        std::thread::Builder::new()
            .name(String::from("ee-agent-host"))
            .spawn(move || host_worker(runtime, worker_manager, commands_rx))
            .expect("spawn agents host worker");
        Self { manager, events, commands: commands_tx }
    }

    /// Enqueues a new-session request.
    pub(super) fn request_new_session(
        &self,
        agent_id: String,
        roots: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
    ) -> std_mpsc::Receiver<Result<AgentThread, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::NewSession {
            agent_id,
            roots,
            mcp_servers,
            ee_proxy_stdio_fallback,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a reconnect request (load, falling back to resume).
    pub(super) fn request_reconnect(
        &self,
        agent_id: String,
        session_id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> std_mpsc::Receiver<Result<AgentThread, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::ReconnectSession {
            agent_id,
            session_id,
            cwd,
            additional_directories,
            mcp_servers,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a prompt turn (fire-and-forget; events carry the outcome).
    pub(super) fn send_prompt(&self, thread: AgentThread, blocks: Vec<ContentBlock>) {
        let _ = self.commands.send(HostCommand::SendPrompt { thread, blocks });
    }

    /// Enqueues a recoverable-turn resume without allocating a new host evidence turn.
    pub(super) fn resume_prompt(&self, thread: AgentThread, blocks: Vec<ContentBlock>) {
        let _ = self.commands.send(HostCommand::ResumePrompt { thread, blocks });
    }

    /// Enqueues a mode change.
    pub(super) fn set_mode(
        &self,
        thread: AgentThread,
        mode_id: SessionModeId,
    ) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::SetMode { thread, mode_id, reply: reply_tx });
        reply_rx
    }

    /// Enqueues a session config option change.
    pub(super) fn set_config_option(
        &self,
        thread: AgentThread,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::SetConfigOption {
            thread,
            config_id,
            value,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a turn cancellation.
    pub(super) fn cancel(&self, thread: AgentThread) -> std_mpsc::Receiver<Result<(), String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::Cancel { thread, reply: reply_tx });
        reply_rx
    }

    /// Enqueues one host-isolated external critique without starting it during discovery.
    pub(super) fn request_external_critique(
        &self,
        config: ExternalCriticConfig,
        timeout: Duration,
        output_limit: usize,
        request: ExternalCritiqueRequest,
        cancel: watch::Receiver<bool>,
    ) -> std_mpsc::Receiver<ExternalCritiqueOutcome> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::ExternalCritique {
            config,
            timeout,
            output_limit,
            request,
            cancel,
            reply: reply_tx,
        });
        reply_rx
    }
}

impl std::fmt::Debug for AgentHostBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHostBridge").field("manager", &self.manager).finish_non_exhaustive()
    }
}

impl Drop for AgentHostBridge {
    fn drop(&mut self) {
        // Ask the worker to shut the manager down; the worker exits after
        // the current command resolves (all commands are internally bounded).
        let _ = self.commands.send(HostCommand::Shutdown);
    }
}
