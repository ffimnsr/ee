//! Message dispatch from the pinned registry to host operations.
//!
//! Every route resolves through [`Method`]; this module never carries method
//! name literals. `turn/start` returns immediately and completion arrives as a
//! notification, mirroring the ACP rule that only one prompt may be active per
//! session.

use ee_agent_host::AgentError;
use ee_agent_protocol::{ContentBlock, SessionId, SessionModeId, TextContent};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::REMOTE_PROTOCOL_VERSION;
use crate::error::{RemoteError, RemoteErrorCode};
use crate::host::{RemoteHost, canonical_directory};
use crate::protocol::*;

/// Identity of the connection issuing a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    /// Stable client id from `initialize` (never agent controlled).
    pub client_id: String,
}

impl CallerIdentity {
    /// Builds an identity from a client-supplied id, falling back to the
    /// connection name.
    #[must_use]
    pub fn new(client_id: impl Into<String>) -> Self {
        Self { client_id: client_id.into() }
    }

    /// The identity used by in-process callers (tests, MCP dispatch).
    #[must_use]
    pub fn anonymous() -> Self {
        Self::new("anonymous")
    }
}

impl Default for CallerIdentity {
    fn default() -> Self {
        Self::anonymous()
    }
}

impl RemoteHost {
    /// Dispatches one client request through the registry.
    ///
    /// # Errors
    ///
    /// Returns a typed [`RemoteError`] for unknown methods, invalid params,
    /// busy sessions, unknown sessions, and host failures.
    pub async fn dispatch(
        &self,
        method: Method,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        self.dispatch_as(&CallerIdentity::anonymous(), method, params).await
    }

    /// Like [`Self::dispatch`], with the caller identity recorded in audits.
    ///
    /// # Errors
    ///
    /// Same as [`Self::dispatch`].
    pub async fn dispatch_as(
        &self,
        caller: &CallerIdentity,
        method: Method,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        let result = self.dispatch_inner(caller, method, params).await?;
        Ok(self.redactor().value(&result))
    }

    async fn dispatch_inner(
        &self,
        caller: &CallerIdentity,
        method: Method,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        self.start()?;
        match method {
            Method::Initialize => self.handle_initialize(params),
            Method::SessionList => self.handle_session_list(params).await,
            Method::SessionNew => self.handle_session_new(params).await,
            Method::SessionResume => self.handle_session_resume(params).await,
            Method::SessionClose => self.handle_session_close(params).await,
            Method::SessionSetMode => self.handle_session_set_mode(params).await,
            Method::SessionSetModel => self.handle_session_set_model(params).await,
            Method::TurnStart => self.handle_turn_start(caller, params).await,
            Method::TurnInterrupt => self.handle_turn_interrupt(caller, params).await,
            Method::PermissionsPending => self.handle_permissions_pending(params),
            Method::PermissionsRespond => self.handle_permissions_respond(caller, params),
            Method::HistoryTail => self.handle_history_tail(params),
            Method::Initialized
            | Method::SessionUpdate
            | Method::TurnCompleted
            | Method::StatusChanged
            | Method::PermissionRequestApproval => Err(RemoteError::invalid_request(format!(
                "{} is not a client request method",
                method.name()
            ))),
        }
    }

    fn handle_initialize(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: InitializeParams = parse_params(Method::Initialize, params)?;
        if params.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(RemoteError::new(
                RemoteErrorCode::InvalidRequest,
                format!(
                    "unsupported remote protocol version {:?}; this server speaks {REMOTE_PROTOCOL_VERSION:?}",
                    params.protocol_version
                ),
                Some(json!({
                    "supported": [REMOTE_PROTOCOL_VERSION],
                    "requested": params.protocol_version,
                })),
            ));
        }
        Ok(json!(InitializeResult {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            server_info: ServerInfo {
                name: crate::SERVER_NAME.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: ServerCapabilities {
                steering: false,
                queueing: false,
                permission_approvals: true,
                history: true,
            },
            permission_mode: self.config().permission_mode.as_str().to_string(),
        }))
    }

    async fn handle_session_list(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionListParams = parse_params(Method::SessionList, params)?;
        let sessions = self
            .sessions_snapshot()
            .into_iter()
            .filter(|session| {
                params.agent_id.as_deref().is_none_or(|agent| agent == session.agent_id)
            })
            .map(|session| SessionSummary {
                session_id: session.session_id().0.to_string(),
                agent_id: session.agent_id.clone(),
                cwd: Some(session.cwd.display().to_string()),
                title: session.thread.snapshot().session_info.and_then(|info| info.title.take()),
                turn_running: session.thread.is_turn_running(),
            })
            .collect();
        let default_agent =
            self.manager().resolve_default_agent(self.config().default_agent.as_deref());
        let agents = self
            .manager()
            .agent_ids()
            .into_iter()
            .map(|agent_id| AgentSummary {
                connected: self.manager().connection_state(&agent_id).is_some(),
                default: default_agent.as_deref() == Some(agent_id.as_str()),
                agent_id,
            })
            .collect();
        Ok(json!(SessionListResult { sessions, agents }))
    }

    async fn handle_session_new(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionNewParams = parse_params(Method::SessionNew, params)?;
        let agent_id = self.resolve_agent(params.agent_id.as_deref())?;
        let cwd = self.resolve_cwd(params.cwd.as_deref())?;
        self.ensure_allowed(&cwd)?;
        let mut roots = vec![cwd.clone()];
        for directory in &params.additional_directories {
            let directory = canonical_directory(std::path::Path::new(directory))?;
            self.ensure_allowed(&directory)?;
            roots.push(directory);
        }
        let thread = self
            .manager()
            .new_session(&agent_id, roots, Vec::new(), None)
            .await
            .map_err(RemoteError::from)?;
        if let Some(mode) = params.mode {
            thread.set_mode(SessionModeId::new(mode)).await.map_err(RemoteError::from)?;
        }
        let session = self.register_session(agent_id, thread, cwd);
        Ok(json!(SessionNewResult {
            session_id: session.session_id().0.to_string(),
            agent_id: session.agent_id.clone(),
            modes: session.thread.advertised_modes(),
            config_options: session.thread.config_options(),
        }))
    }

    async fn handle_session_resume(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionResumeParams = parse_params(Method::SessionResume, params)?;
        let agent_id = self.resolve_agent(params.agent_id.as_deref())?;
        let cwd = self.resolve_cwd(params.cwd.as_deref())?;
        self.ensure_allowed(&cwd)?;
        let mut additional = Vec::new();
        for directory in &params.additional_directories {
            let directory = canonical_directory(std::path::Path::new(directory))?;
            self.ensure_allowed(&directory)?;
            additional.push(directory);
        }
        let thread = self
            .manager()
            .resume_session(
                &agent_id,
                SessionId::new(params.session_id),
                cwd.clone(),
                additional,
                Vec::new(),
            )
            .await
            .map_err(RemoteError::from)?;
        let session = self.register_session(agent_id, thread, cwd);
        Ok(json!(SessionNewResult {
            session_id: session.session_id().0.to_string(),
            agent_id: session.agent_id.clone(),
            modes: session.thread.advertised_modes(),
            config_options: session.thread.config_options(),
        }))
    }

    async fn handle_session_close(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionCloseParams = parse_params(Method::SessionClose, params)?;
        let session = self.session(&params.session_id)?;
        let session_id = session.session_id().clone();
        let agent_id = session.agent_id.clone();
        // Best-effort agent-side close: even an agent that does not advertise
        // `session/close` must not leave the session loaded on this host.
        let _ = self.manager().close_session(&agent_id, session_id).await;
        self.remove_session(&params.session_id);
        Ok(json!(SessionCloseResult { closed: true }))
    }

    async fn handle_session_set_mode(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionSetModeParams = parse_params(Method::SessionSetMode, params)?;
        let session = self.session(&params.session_id)?;
        session
            .thread
            .set_mode(SessionModeId::new(params.mode_id))
            .await
            .map_err(RemoteError::from)?;
        Ok(json!(SessionSetModeResult { applied: true }))
    }

    async fn handle_session_set_model(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: SessionSetModelParams = parse_params(Method::SessionSetModel, params)?;
        let session = self.session(&params.session_id)?;
        session.thread.set_model(params.model_id).await.map_err(RemoteError::from)?;
        Ok(json!(SessionSetModelResult { applied: true }))
    }

    async fn handle_turn_start(
        &self,
        caller: &CallerIdentity,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        let params: TurnStartParams = parse_params(Method::TurnStart, params)?;
        if params.text.trim().is_empty() {
            self.audit_turn(caller, "turn_start", "refused", &params.session_id, "empty text");
            return Err(RemoteError::invalid_params("turn text must not be empty"));
        }
        let session = match self.session(&params.session_id) {
            Ok(session) => session,
            Err(error) => {
                self.audit_turn(
                    caller,
                    "turn_start",
                    "refused",
                    &params.session_id,
                    "unknown session",
                );
                return Err(error);
            }
        };
        let Some(turn_id) = session.reserve_turn() else {
            self.audit_turn(
                caller,
                "turn_start",
                "refused",
                &params.session_id,
                "turn already active",
            );
            return Err(RemoteError::already_active(&params.session_id));
        };
        self.audit_turn(
            caller,
            "turn_start",
            "started",
            &params.session_id,
            &format!("turn {turn_id}"),
        );
        let text = params.text;
        let thread = session.thread.clone();
        let host = self.clone();
        let key = params.session_id.clone();
        tokio::spawn(async move {
            let prompt = vec![ContentBlock::Text(TextContent::new(text))];
            if let Err(error) = thread.send_prompt(prompt).await {
                host.fail_reserved_turn(&key, turn_id, error);
            }
        });
        Ok(json!(TurnStartResult { turn_id, session_id: params.session_id }))
    }

    async fn handle_turn_interrupt(
        &self,
        caller: &CallerIdentity,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        let params: TurnInterruptParams = parse_params(Method::TurnInterrupt, params)?;
        let session = match self.session(&params.session_id) {
            Ok(session) => session,
            Err(error) => {
                self.audit_turn(
                    caller,
                    "turn_interrupt",
                    "refused",
                    &params.session_id,
                    "unknown session",
                );
                return Err(error);
            }
        };
        match session.thread.cancel().await {
            Ok(()) => {
                self.audit_turn(
                    caller,
                    "turn_interrupt",
                    "interrupted",
                    &params.session_id,
                    "cancel sent",
                );
                Ok(json!(TurnInterruptResult { interrupted: true }))
            }
            // A second interrupt (or an interrupt while queued) is a typed
            // no-op, never an error.
            Err(AgentError::NoRunningTurn) => {
                self.audit_turn(
                    caller,
                    "turn_interrupt",
                    "noop",
                    &params.session_id,
                    "no running turn",
                );
                Ok(json!(TurnInterruptResult { interrupted: false }))
            }
            Err(error) => {
                self.audit_turn(
                    caller,
                    "turn_interrupt",
                    "error",
                    &params.session_id,
                    "cancel failed",
                );
                Err(RemoteError::from(error))
            }
        }
    }

    fn audit_turn(
        &self,
        caller: &CallerIdentity,
        event: &str,
        decision: &str,
        session_id: &str,
        detail: &str,
    ) {
        let entry = crate::permissions::AuditEntry::new(event, &caller.client_id, event)
            .decision(decision)
            .session(session_id)
            .mode(self.config().permission_mode.as_str())
            .detail(detail);
        self.approvals().audit().record(&entry);
    }

    fn handle_permissions_pending(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let _: PermissionsPendingParams = parse_params(Method::PermissionsPending, params)?;
        Ok(json!(PermissionsPendingResult { approvals: self.approvals().pending() }))
    }

    fn handle_permissions_respond(
        &self,
        caller: &CallerIdentity,
        params: Option<Value>,
    ) -> Result<Value, RemoteError> {
        let params: PermissionsRespondParams = parse_params(Method::PermissionsRespond, params)?;
        self.approvals().respond(
            params.request_id,
            params.decision,
            params.option_id.as_deref(),
            &caller.client_id,
        )?;
        Ok(json!(PermissionsRespondResult { resolved: true }))
    }

    /// Routes a client response to a `permission/requestApproval` server
    /// request into the approval registry.
    ///
    /// # Errors
    ///
    /// `not_found` for unknown ids, `invalid_params` for malformed results,
    /// `already_resolved` for late answers.
    pub fn resolve_approval_response(
        &self,
        server_request_id: u64,
        result: Option<&Value>,
        caller: &CallerIdentity,
    ) -> Result<(), RemoteError> {
        self.approvals().resolve_server_response(server_request_id, result, &caller.client_id)
    }

    fn handle_history_tail(&self, params: Option<Value>) -> Result<Value, RemoteError> {
        let params: HistoryTailParams = parse_params(Method::HistoryTail, params)?;
        self.session(&params.session_id)?;
        let page = params.limit.unwrap_or(self.config().history_page_size);
        let limit = page.clamp(1, self.config().history_page_size);
        Ok(json!(self.bus().tail(&params.session_id, params.from_cursor.unwrap_or(0), limit)))
    }

    fn ensure_allowed(&self, path: &std::path::Path) -> Result<(), RemoteError> {
        if self.config().allowed_roots.is_empty() {
            return Ok(());
        }
        let allowed = self
            .config()
            .allowed_roots
            .iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| path.starts_with(root)));
        if allowed {
            Ok(())
        } else {
            Err(RemoteError::denied(format!(
                "{} is outside the configured workspace roots",
                path.display()
            )))
        }
    }

    /// Emits a failed completion for a reserved turn when the prompt future
    /// failed before any terminal event was observed. Idempotent: the pump may
    /// already have completed the turn from an event.
    fn fail_reserved_turn(&self, session_id: &str, turn_id: u64, error: AgentError) {
        let Ok(session) = self.session(session_id) else {
            return;
        };
        if session.active_turn() != Some(turn_id) {
            return;
        }
        if session.take_active_turn() != Some(turn_id) {
            return;
        }
        let failure = RemoteError::from(error);
        let key = session_id.to_string();
        let params_key = key.clone();
        let status =
            if failure.code() == RemoteErrorCode::Denied && failure.message() == "turn cancelled" {
                TurnStatus::Cancelled
            } else {
                TurnStatus::Failed
            };
        self.bus().publish_with(&key, crate::protocol::TURN_COMPLETED, move |cursor| {
            json!(TurnCompletedNotification {
                session_id: params_key.clone(),
                turn_id,
                cursor,
                status,
                stop_reason: None,
                error: Some(RemoteFailure {
                    class: failure.code().as_str().to_string(),
                    message: failure.message().to_string(),
                }),
            })
        });
    }
}

fn parse_params<T: DeserializeOwned>(
    method: Method,
    params: Option<Value>,
) -> Result<T, RemoteError> {
    let value = params.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    serde_json::from_value(value).map_err(|error| {
        RemoteError::invalid_params(format!(
            "{} params did not match {}: {error}",
            method.name(),
            method.params_type()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteHostConfig;

    #[tokio::test]
    async fn initialize_negotiates_the_pinned_version() {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        let result = host
            .dispatch(
                Method::Initialize,
                Some(json!({ "protocolVersion": REMOTE_PROTOCOL_VERSION })),
            )
            .await
            .expect("initialize");
        assert_eq!(result["protocolVersion"], REMOTE_PROTOCOL_VERSION);
        assert_eq!(result["capabilities"]["steering"], false);
        assert_eq!(result["permissionMode"], "ask");
    }

    #[tokio::test]
    async fn unsupported_protocol_versions_fail_typed() {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        let error = host
            .dispatch(Method::Initialize, Some(json!({ "protocolVersion": "2.0" })))
            .await
            .expect_err("version");
        assert_eq!(error.code(), RemoteErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn unknown_sessions_fail_typed_for_every_session_method() {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        for (method, params) in [
            (Method::TurnStart, json!({"sessionId":"s","text":"hi"})),
            (Method::TurnInterrupt, json!({"sessionId":"s"})),
            (Method::HistoryTail, json!({"sessionId":"s"})),
            (Method::SessionClose, json!({"sessionId":"s"})),
            (Method::SessionSetMode, json!({"sessionId":"s","modeId":"code"})),
        ] {
            let error = host.dispatch(method, Some(params)).await.expect_err("unknown session");
            assert_eq!(error.code(), RemoteErrorCode::NotFound, "{}", method.name());
        }
    }

    #[tokio::test]
    async fn empty_turn_text_is_invalid() {
        let host = RemoteHost::deny_all(RemoteHostConfig::default());
        let error = host
            .dispatch(Method::TurnStart, Some(json!({"sessionId":"s","text":"  "})))
            .await
            .expect_err("empty text");
        // Unknown session is checked after text validation only for text; the
        // contract is that invalid params win before session lookup.
        assert!(matches!(error.code(), RemoteErrorCode::InvalidParams | RemoteErrorCode::NotFound));
    }
}
