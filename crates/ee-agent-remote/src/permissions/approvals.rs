//! Remote approval registry.
//!
//! Two sources feed the same registry:
//!
//! - ACP `session/request_permission` events, answered through the session
//!   thread's permission broker;
//! - headless write/terminal gates, answered through a oneshot.
//!
//! Rules are evaluated by the host broker before either path: an `allow` never
//! reaches this registry, and a `deny` always wins there. The remote modes
//! below control only what happens to requests that survived that preflight.
//!
//! Every resolution appends one audit line. Expiry, client errors, malformed
//! responses, and dropped senders all end in deny.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ee_agent_host::{AgentThread, PermissionRequestId, PermissionRequestInfo};
use ee_agent_protocol::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::config::RemotePermissionMode;
use crate::error::RemoteError;
use crate::events::EventBus;
use crate::permissions::audit::{AuditEntry, AuditLog};
use crate::permissions::gate::{ApprovalGate, GateDecision, GateKind, GateRequest};
use crate::protocol::{
    PERMISSION_REQUEST_APPROVAL, PendingApproval, PermissionApprovalRequest, PermissionDecision,
};
use crate::redact::Redactor;

/// Offset applied to remote approval ids when they carry server-request ids.
pub const SERVER_REQUEST_ID_BASE: u64 = 1_000_000_000;
/// Maximum characters retained in a bounded approval summary.
pub const MAX_SUMMARY_CHARS: usize = 240;

static GATE_SEQ: AtomicU64 = AtomicU64::new(1);

enum Responder {
    Acp { thread: AgentThread, request_id: PermissionRequestId, host_request_id: u64 },
    Gate { sender: oneshot::Sender<GateDecision> },
}

struct PendingEntry {
    session_id: String,
    summary: String,
    tool_call: ToolCallUpdate,
    options: Vec<PermissionOption>,
    deadline: Instant,
    /// Whether a remote client may answer this entry. `rules` mode registers
    /// entries only so they can expire; opaque drivers must not widen it.
    answerable: bool,
    responder: Responder,
}

/// Who resolved a pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveSource {
    Client,
    Timeout,
    Shutdown,
}

#[derive(Default)]
struct ApprovalState {
    next_id: u64,
    pending: BTreeMap<u64, PendingEntry>,
    acp_index: BTreeMap<u64, u64>,
}

struct ApprovalsInner {
    mode: RemotePermissionMode,
    timeout: Duration,
    audit: AuditLog,
    bus: EventBus,
    redactor: Redactor,
    state: Mutex<ApprovalState>,
}

/// Shared remote approval registry.
#[derive(Clone)]
pub struct RemoteApprovals {
    inner: Arc<ApprovalsInner>,
}

impl std::fmt::Debug for RemoteApprovals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteApprovals")
            .field("mode", &self.inner.mode)
            .field("pending", &self.pending_count())
            .finish_non_exhaustive()
    }
}

impl RemoteApprovals {
    /// Creates a registry in the given mode with no configured-value seeds.
    #[must_use]
    pub fn new(
        mode: RemotePermissionMode,
        timeout: Duration,
        audit: AuditLog,
        bus: EventBus,
    ) -> Self {
        Self::with_redactor(mode, timeout, audit, bus, Redactor::default())
    }

    /// Creates a registry that redacts approval payloads with the host
    /// redactor (including configured secret values).
    #[must_use]
    pub fn with_redactor(
        mode: RemotePermissionMode,
        timeout: Duration,
        audit: AuditLog,
        bus: EventBus,
        redactor: Redactor,
    ) -> Self {
        Self {
            inner: Arc::new(ApprovalsInner {
                mode,
                timeout: timeout.max(Duration::from_millis(1)),
                audit,
                bus,
                redactor,
                state: Mutex::new(ApprovalState::default()),
            }),
        }
    }

    /// The approval posture.
    #[must_use]
    pub fn mode(&self) -> RemotePermissionMode {
        self.inner.mode
    }

    /// The audit log.
    #[must_use]
    pub fn audit(&self) -> &AuditLog {
        &self.inner.audit
    }

    /// Registers an ACP permission request.
    pub fn register_acp(&self, thread: AgentThread, info: PermissionRequestInfo) {
        let summary = summarize(&info.tool_call, &self.inner.redactor);
        let tool_call = redact_tool_call(info.tool_call.clone(), &self.inner.redactor);
        if self.inner.mode == RemotePermissionMode::Deny {
            let outcome = outcome_for(&info.options, PermissionDecision::Deny, None);
            let _ = thread.respond_permission(info.request_id, outcome);
            self.inner.record_decision(
                info.session_id.0.as_ref(),
                "rules",
                "deny",
                None,
                &summary,
                "mode=deny",
            );
            return;
        }
        let (id, deadline) = self.inner.insert(
            info.session_id.0.to_string(),
            summary.clone(),
            tool_call.clone(),
            info.options.clone(),
            self.inner.mode.surfaces_approvals(),
            Responder::Acp {
                thread,
                request_id: info.request_id,
                host_request_id: info.request_id.0,
            },
        );
        if self.inner.mode.surfaces_approvals() {
            self.inner.broadcast(
                id,
                info.session_id.0.as_ref(),
                &summary,
                &tool_call,
                &info.options,
                expires_in_ms(deadline),
            );
        }
        self.schedule_timeout(id);
    }

    /// Records an automatic decision made by declarative rules or trust
    /// profiles before any remote round-trip.
    pub fn record_auto(
        &self,
        session_id: &str,
        tool_call: &ToolCallUpdate,
        outcome: &RequestPermissionOutcome,
    ) {
        self.inner.record_decision(
            session_id,
            "rules",
            outcome_label(outcome),
            None,
            &summarize(tool_call, &self.inner.redactor),
            "declarative rules",
        );
    }

    /// Settles the remote entry when the host broker resolved it (turn cancel,
    /// connection close, or after a remote answer).
    pub fn settle_acp(&self, request_id: PermissionRequestId, outcome: &RequestPermissionOutcome) {
        let removed = {
            let mut state = self.inner.state.lock().expect("approvals poisoned");
            let Some(remote_id) = state.acp_index.remove(&request_id.0) else {
                return;
            };
            state.pending.remove(&remote_id).map(|entry| (remote_id, entry))
        };
        if let Some((remote_id, entry)) = removed {
            self.inner.record_decision(
                &entry.session_id,
                "agent",
                outcome_label(outcome),
                Some(remote_id),
                &entry.summary,
                "resolved by the agent session",
            );
        }
    }

    /// Pending approvals, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingApproval> {
        self.inner
            .state
            .lock()
            .expect("approvals poisoned")
            .pending
            .iter()
            .filter(|(_, entry)| entry.answerable)
            .map(|(id, entry)| PendingApproval {
                request_id: *id,
                session_id: entry.session_id.clone(),
                summary: entry.summary.clone(),
                tool_call: entry.tool_call.clone(),
                options: entry.options.clone(),
                expires_in_ms: expires_in_ms(entry.deadline),
            })
            .collect()
    }

    /// Answers a pending approval.
    ///
    /// # Errors
    ///
    /// `already_resolved` when another client answered first or the request
    /// expired.
    pub fn respond(
        &self,
        request_id: u64,
        decision: PermissionDecision,
        option_id: Option<&str>,
        actor: &str,
    ) -> Result<(), RemoteError> {
        self.inner.resolve(
            request_id,
            decision,
            option_id,
            actor,
            "client answer",
            ResolveSource::Client,
        )
    }

    /// Answers a pending ACP permission request by the agent's request id.
    ///
    /// The agent-side [`PermissionBroker`] numbers its own requests from zero,
    /// while this registry keys entries by a separate internal id exposed to
    /// remote clients in approval notifications. An answer that carries the
    /// ACP id must be translated through the `acp_index`; passing it straight
    /// to [`Self::respond`] misses the entry and lets it expire.
    ///
    /// [`PermissionBroker`]: ee_agent_host::PermissionBroker
    ///
    /// # Errors
    ///
    /// `already_resolved` when the request is unknown, already answered, or
    /// expired.
    pub fn respond_acp(
        &self,
        request_id: PermissionRequestId,
        decision: PermissionDecision,
        option_id: Option<&str>,
        actor: &str,
    ) -> Result<(), RemoteError> {
        let internal_id = {
            let state = self.inner.state.lock().expect("approvals poisoned");
            state.acp_index.get(&request_id.0).copied()
        };
        let internal_id = internal_id.ok_or_else(|| RemoteError::already_resolved(request_id.0))?;
        self.respond(internal_id, decision, option_id, actor)
    }

    /// Resolves a server→client approval response frame.
    ///
    /// `result` is `None` when the client answered with a JSON-RPC error. That
    /// fails closed with a typed error and leaves the entry pending so a
    /// well-behaved client can still answer before the timeout.
    ///
    /// # Errors
    ///
    /// `not_found` for unknown server request ids, `invalid_params` for
    /// malformed results, `already_resolved` for late answers.
    pub fn resolve_server_response(
        &self,
        server_request_id: u64,
        result: Option<&Value>,
        actor: &str,
    ) -> Result<(), RemoteError> {
        let Some(request_id) = server_request_id.checked_sub(SERVER_REQUEST_ID_BASE) else {
            return Err(RemoteError::not_found(format!("server request {server_request_id}")));
        };
        let Some(result) = result else {
            return Err(RemoteError::invalid_params(
                "approval response carried a JSON-RPC error; request stays pending until timeout",
            ));
        };
        let parsed: crate::protocol::PermissionApprovalResponse =
            serde_json::from_value(result.clone()).map_err(|error| {
                RemoteError::invalid_params(format!(
                    "malformed approval response; request stays pending until timeout: {error}"
                ))
            })?;
        self.respond(request_id, parsed.decision, parsed.option_id.as_deref(), actor)
    }

    /// Number of pending approvals.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.inner.state.lock().expect("approvals poisoned").pending.len()
    }

    /// Denies every pending approval with the `shutdown` actor.
    ///
    /// Called by host shutdown so waiting handlers, ACP brokers, and pending
    /// gates all resolve instead of leaking tasks.
    pub fn cancel_all(&self) {
        let ids: Vec<u64> =
            self.inner.state.lock().expect("approvals poisoned").pending.keys().copied().collect();
        for id in ids {
            let _ = self.inner.resolve(
                id,
                PermissionDecision::Deny,
                None,
                "shutdown",
                "host shutdown",
                ResolveSource::Shutdown,
            );
        }
    }

    /// Arms the deny-on-expiry task for one pending approval.
    fn schedule_timeout(&self, id: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to schedule on: fail closed immediately.
            let _ = self.inner.resolve(
                id,
                PermissionDecision::Deny,
                None,
                "timeout",
                "expired",
                ResolveSource::Timeout,
            );
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let timeout = self.inner.timeout;
        handle.spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(inner) = weak.upgrade() {
                let _ = inner.resolve(
                    id,
                    PermissionDecision::Deny,
                    None,
                    "timeout",
                    "expired",
                    ResolveSource::Timeout,
                );
            }
        });
    }

    async fn authorize_gate(&self, request: GateRequest) -> GateDecision {
        if self.inner.mode == RemotePermissionMode::Deny {
            self.inner.record_decision(
                request.session_id.0.as_ref(),
                "rules",
                "deny",
                None,
                &request.summary,
                "mode=deny",
            );
            return GateDecision::Deny;
        }
        let summary = bounded_summary(&self.inner.redactor.text(&request.summary));
        let tool_call = synthetic_tool_call(&request, &summary);
        let options = synthetic_options();
        let (sender, receiver) = oneshot::channel();
        let (id, deadline) = self.inner.insert(
            request.session_id.0.to_string(),
            summary.clone(),
            tool_call.clone(),
            options.clone(),
            self.inner.mode.surfaces_approvals(),
            Responder::Gate { sender },
        );
        if self.inner.mode.surfaces_approvals() {
            self.inner.broadcast(
                id,
                request.session_id.0.as_ref(),
                &summary,
                &tool_call,
                &options,
                expires_in_ms(deadline),
            );
        }
        self.schedule_timeout(id);
        // The timeout resolver answers this oneshot with a denial, so the gate
        // future is bounded by the same deadline.
        receiver.await.unwrap_or(GateDecision::Deny)
    }
}

impl ApprovalGate for RemoteApprovals {
    fn authorize(
        &self,
        request: GateRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = GateDecision> + Send + '_>> {
        let approvals = self.clone();
        Box::pin(async move { approvals.authorize_gate(request).await })
    }
}

impl ApprovalsInner {
    fn insert(
        &self,
        session_id: String,
        summary: String,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        answerable: bool,
        responder: Responder,
    ) -> (u64, Instant) {
        let deadline = Instant::now() + self.timeout;
        let mut state = self.state.lock().expect("approvals poisoned");
        state.next_id += 1;
        let id = state.next_id;
        if let Responder::Acp { host_request_id, .. } = &responder {
            state.acp_index.insert(*host_request_id, id);
        }
        state.pending.insert(
            id,
            PendingEntry {
                session_id,
                summary,
                tool_call,
                options,
                deadline,
                answerable,
                responder,
            },
        );
        (id, deadline)
    }

    fn broadcast(
        &self,
        id: u64,
        session_id: &str,
        summary: &str,
        tool_call: &ToolCallUpdate,
        options: &[PermissionOption],
        expires_in_ms: u64,
    ) {
        let request = PermissionApprovalRequest {
            request_id: id,
            session_id: session_id.to_string(),
            summary: summary.to_string(),
            tool_call: tool_call.clone(),
            options: options.to_vec(),
            expires_in_ms,
        };
        if let Ok(params) = serde_json::to_value(request) {
            self.bus.broadcast_frame(crate::protocol::server_request(
                SERVER_REQUEST_ID_BASE + id,
                PERMISSION_REQUEST_APPROVAL,
                params,
            ));
        }
    }

    fn resolve(
        &self,
        id: u64,
        decision: PermissionDecision,
        option_id: Option<&str>,
        actor: &str,
        reason: &str,
        source: ResolveSource,
    ) -> Result<(), RemoteError> {
        let entry = {
            let mut state = self.state.lock().expect("approvals poisoned");
            let answerable = state
                .pending
                .get(&id)
                .map(|entry| entry.answerable)
                .ok_or_else(|| RemoteError::already_resolved(id))?;
            if source == ResolveSource::Client && !answerable {
                return Err(RemoteError::denied(
                    "approval is not answerable in the configured remote permission mode",
                ));
            }
            let entry =
                state.pending.remove(&id).ok_or_else(|| RemoteError::already_resolved(id))?;
            if let Responder::Acp { host_request_id, .. } = &entry.responder {
                state.acp_index.remove(host_request_id);
            }
            entry
        };
        let outcome = outcome_for(&entry.options, decision, option_id);
        let label = decision_label(decision);
        match entry.responder {
            Responder::Acp { thread, request_id, .. } => {
                let _ = thread.respond_permission(request_id, outcome);
            }
            Responder::Gate { sender } => {
                let answer = match decision {
                    PermissionDecision::Allow | PermissionDecision::AllowAlways => {
                        GateDecision::Allow
                    }
                    PermissionDecision::Deny => GateDecision::Deny,
                };
                let _ = sender.send(answer);
            }
        }
        self.record_decision(&entry.session_id, actor, label, Some(id), &entry.summary, reason);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_decision(
        &self,
        session_id: &str,
        actor: &str,
        decision: &str,
        request_id: Option<u64>,
        summary: &str,
        reason: &str,
    ) {
        let mut entry = AuditEntry::new("permission_decision", actor, "session/request_permission")
            .decision(decision)
            .session(session_id)
            .mode(self.mode.as_str())
            .detail(format!("{summary} ({reason})"));
        if let Some(request_id) = request_id {
            entry = entry.request_id(request_id);
        }
        self.audit.record(&entry);
    }
}

fn expires_in_ms(deadline: Instant) -> u64 {
    deadline.saturating_duration_since(Instant::now()).as_millis() as u64
}

fn bounded_summary(summary: &str) -> String {
    let mut chars = summary.chars();
    let bounded: String = chars.by_ref().take(MAX_SUMMARY_CHARS).collect();
    if chars.next().is_some() { format!("{bounded}…") } else { bounded }
}

fn summarize(tool_call: &ToolCallUpdate, redactor: &Redactor) -> String {
    let kind = tool_call
        .fields
        .kind
        .map(|kind| format!("{kind:?}").to_ascii_lowercase())
        .unwrap_or_else(|| String::from("tool"));
    let title = tool_call.fields.title.as_deref().unwrap_or_default();
    let name = tool_call.fields.name.as_deref().unwrap_or_default();
    let text = if name.is_empty() {
        format!("{kind}: {title}")
    } else if title.is_empty() {
        format!("{kind}: {name}")
    } else {
        format!("{kind}: {name} — {title}")
    };
    bounded_summary(&redactor.text(&text))
}

fn redact_tool_call(mut tool_call: ToolCallUpdate, redactor: &Redactor) -> ToolCallUpdate {
    tool_call.fields.title = tool_call.fields.title.map(|title| redactor.text(&title));
    tool_call.fields.name = tool_call.fields.name.map(|name| redactor.text(&name));
    tool_call.fields.content = None;
    tool_call.fields.raw_input = tool_call.fields.raw_input.map(|value| redactor.value(&value));
    tool_call
}

fn synthetic_tool_call(request: &GateRequest, summary: &str) -> ToolCallUpdate {
    let kind = match request.kind {
        GateKind::WriteFile { .. } => ToolKind::Edit,
        GateKind::CreateTerminal { .. } => ToolKind::Execute,
    };
    let id = GATE_SEQ.fetch_add(1, Ordering::Relaxed);
    let fields = ToolCallUpdateFields::new().title(summary.to_string()).kind(kind).name("headless");
    ToolCallUpdate::new(format!("gate-{}-{id}", std::process::id()), fields)
}

fn synthetic_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
    ]
}

fn outcome_for(
    options: &[PermissionOption],
    decision: PermissionDecision,
    option_id: Option<&str>,
) -> RequestPermissionOutcome {
    if let Some(option_id) = option_id
        && let Some(option) = options.iter().find(|option| option.option_id.0.as_ref() == option_id)
    {
        let allowed = matches!(
            option.kind,
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
        );
        let wants_allow = decision != PermissionDecision::Deny;
        if allowed == wants_allow {
            return RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ));
        }
        return RequestPermissionOutcome::Cancelled;
    }
    let kinds: &[PermissionOptionKind] = match decision {
        PermissionDecision::Allow => {
            &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
        }
        PermissionDecision::AllowAlways => {
            &[PermissionOptionKind::AllowAlways, PermissionOptionKind::AllowOnce]
        }
        PermissionDecision::Deny => {
            &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
        }
    };
    options
        .iter()
        .find(|option| kinds.contains(&option.kind))
        .map(|option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
        .unwrap_or(RequestPermissionOutcome::Cancelled)
}

fn decision_label(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::AllowAlways => "allow_always",
        PermissionDecision::Deny => "deny",
    }
}

fn outcome_label(outcome: &RequestPermissionOutcome) -> &'static str {
    match outcome {
        RequestPermissionOutcome::Selected(_) => "allow",
        RequestPermissionOutcome::Cancelled => "deny",
        _ => "deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call() -> ToolCallUpdate {
        ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new()
                .title("Write src/main.rs")
                .name("bash")
                .kind(ToolKind::Edit),
        )
    }

    #[test]
    fn outcome_selection_prefers_the_requested_kind() {
        let options = synthetic_options();
        assert!(matches!(
            outcome_for(&options, PermissionDecision::Allow, None),
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == "allow_once"
        ));
        assert!(matches!(
            outcome_for(&options, PermissionDecision::Deny, None),
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.0.as_ref() == "reject_once"
        ));
        assert!(matches!(
            outcome_for(&options, PermissionDecision::Allow, Some("reject_once")),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn redaction_strips_secret_raw_input_and_content() {
        let mut tool_call = tool_call();
        tool_call.fields.raw_input = Some(serde_json::json!({"api_key": "secret"}));
        let redacted = redact_tool_call(tool_call, &Redactor::default());
        assert_eq!(redacted.fields.raw_input, Some(serde_json::json!({"api_key": "***"})));
        assert!(redacted.fields.content.is_none());
    }

    #[test]
    fn configured_secret_values_are_stripped_from_approval_payloads() {
        let redactor = Redactor::new(vec![String::from("env-secret-value")]);
        let mut tool_call = tool_call();
        tool_call.fields.raw_input = Some(serde_json::json!({"command": "echo env-secret-value"}));
        let redacted = redact_tool_call(tool_call, &redactor);
        let text = serde_json::to_string(&redacted.fields.raw_input).expect("serialize");
        assert!(!text.contains("env-secret-value"), "leaked configured secret: {text}");
    }

    #[tokio::test]
    async fn rules_mode_gates_expire_without_remote_visibility() {
        let bus = EventBus::new(8, 8);
        let mut subscription = bus.subscribe();
        let approvals = RemoteApprovals::new(
            RemotePermissionMode::Rules,
            Duration::from_millis(30),
            AuditLog::disabled(),
            bus,
        );
        let decision = approvals
            .authorize(GateRequest {
                session_id: ee_agent_protocol::SessionId::new("s-1"),
                summary: String::from("write /tmp/x"),
                kind: GateKind::WriteFile { path: std::path::PathBuf::from("/tmp/x"), bytes: 4 },
            })
            .await;
        assert_eq!(decision, GateDecision::Deny);
        assert!(approvals.pending().is_empty());
        assert!(
            subscription.receiver.try_recv().is_err(),
            "rules mode must not emit remote approval requests"
        );
    }

    #[test]
    fn summaries_are_bounded_and_never_empty() {
        let summary = summarize(&tool_call(), &Redactor::default());
        assert!(summary.contains("edit"));
        assert!(!summary.is_empty());
        let tool_call = ToolCallUpdate::new("c", ToolCallUpdateFields::new());
        assert!(!summarize(&tool_call, &Redactor::default()).is_empty());
    }
}
