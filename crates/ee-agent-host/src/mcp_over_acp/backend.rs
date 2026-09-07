//! The host-side proxy backend: struct + shared call plumbing.
use super::*;

pub(crate) struct HostProxyBackend {
    pub(super) jobs: mpsc::UnboundedSender<ProxyJob>,
    pub(super) process: Arc<Mutex<Option<AgentProcess>>>,
    pub(super) threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
    pub(super) agent_id: String,
    pub(super) scope: String,
    pub(super) supported_tools: Option<Vec<String>>,
    pub(super) workspace_memory: Arc<WorkspaceMemoryHost>,
    pub(super) shutdown: CancellationToken,
}

impl HostProxyBackend {
    pub(super) fn call_with_timeout(
        &self,
        request: ClientRequest,
        timeout: Duration,
    ) -> Result<Option<ClientRequestResponse>, ProxyToolError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let cancel = self.shutdown.child_token();
        if self.jobs.send(ProxyJob { request, cancel: cancel.clone(), reply: reply_tx }).is_err() {
            return Err(ProxyToolError {
                message: String::from("agent host is shutting down"),
                is_permission_denied: false,
            });
        }
        let started = std::time::Instant::now();
        loop {
            if self.shutdown.is_cancelled() {
                cancel.cancel();
                return Err(ProxyToolError {
                    message: "workspace_memory_approval_cancelled: operation cancelled".to_string(),
                    is_permission_denied: false,
                });
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                cancel.cancel();
                return Ok(None);
            };
            let wait = remaining.min(Duration::from_millis(20));
            match reply_rx.recv_timeout(wait) {
                Ok(Ok(response)) => return Ok(Some(response)),
                Ok(Err(error)) => {
                    return Err(ProxyToolError {
                        message: error.to_string(),
                        is_permission_denied: matches!(error, AgentError::PermissionDenied { .. }),
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProxyToolError {
                        message: String::from("agent host reply channel closed"),
                        is_permission_denied: false,
                    });
                }
            }
        }
    }

    pub(super) fn call(
        &self,
        request: ClientRequest,
    ) -> Result<ClientRequestResponse, ProxyToolError> {
        self.call_with_timeout(request, MCP_OVER_ACP_APPROVAL_TIMEOUT)?.ok_or_else(|| {
            ProxyToolError {
                message: format!("approval timed out after {MCP_OVER_ACP_APPROVAL_TIMEOUT:?}"),
                is_permission_denied: false,
            }
        })
    }

    pub(super) fn approve_workspace_memory_mutation(
        &self,
        operation: WorkspaceMemoryMutationOperation,
        key: String,
    ) -> Result<(), ProxyToolError> {
        match self.call(ClientRequest::ApproveWorkspaceMemoryMutation { operation, key })? {
            ClientRequestResponse::WorkspaceMemoryApproval { approved: true } => Ok(()),
            ClientRequestResponse::WorkspaceMemoryApproval { approved: false } => {
                Err(ProxyToolError {
                    message: "workspace_memory_approval_denied: workspace-memory mutation denied"
                        .to_string(),
                    is_permission_denied: true,
                })
            }
            _ => Err(ProxyToolError {
                message:
                    "workspace_memory_approval_invalid: invalid workspace-memory approval response"
                        .to_string(),
                is_permission_denied: false,
            }),
        }
    }

    pub(super) fn memory_source_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"ee.workspace-memory.mcp-source.v1\0");
        digest.update(self.agent_id.as_bytes());
        digest.update(b"\0");
        digest.update(self.scope.as_bytes());
        format!("mcp:{:x}", digest.finalize())
    }

    pub(super) fn evidence_unavailable(message: &'static str) -> ProxyToolError {
        ProxyToolError {
            message: format!("evidence_unavailable: {message}"),
            is_permission_denied: false,
        }
    }

    pub(super) fn resolve_evidence_thread(
        &self,
        session_id: Option<String>,
        turn_id: Option<u64>,
    ) -> Result<(Arc<ThreadShared>, u64), ProxyToolError> {
        if turn_id.is_some() && session_id.is_none() {
            return Err(Self::evidence_unavailable(
                "a specified turn requires a connection-owned session",
            ));
        }

        let (thread, turn_id) = match session_id {
            Some(session_id) => {
                let session = SessionId::new(session_id);
                let thread = self
                    .threads
                    .lock()
                    .expect("threads poisoned")
                    .get(&session)
                    .cloned()
                    .ok_or_else(|| {
                        Self::evidence_unavailable("session is not owned by this connection")
                    })?;
                if thread.agent_id != self.agent_id || thread.session_id != session {
                    return Err(Self::evidence_unavailable("session ownership validation failed"));
                }
                let turn_id = match turn_id {
                    Some(turn_id) => turn_id,
                    None => thread
                        .active_turn
                        .lock()
                        .expect("active turn poisoned")
                        .as_ref()
                        .map(|turn| turn.turn_id())
                        .ok_or_else(|| {
                            Self::evidence_unavailable("session has no current evidence turn")
                        })?,
                };
                (thread, turn_id)
            }
            None => {
                let candidates = self
                    .threads
                    .lock()
                    .expect("threads poisoned")
                    .values()
                    .filter_map(|thread| {
                        if thread.agent_id != self.agent_id {
                            return None;
                        }
                        thread
                            .active_turn
                            .lock()
                            .expect("active turn poisoned")
                            .as_ref()
                            .map(|turn| (thread.clone(), turn.turn_id()))
                    })
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [(thread, turn_id)] => (thread.clone(), *turn_id),
                    [] => return Err(Self::evidence_unavailable("no current evidence turn")),
                    _ => {
                        return Err(Self::evidence_unavailable(
                            "current evidence turn is ambiguous; specify session_id",
                        ));
                    }
                }
            }
        };
        Ok((thread, turn_id))
    }
}
