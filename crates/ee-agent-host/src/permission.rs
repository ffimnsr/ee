//! Permission broker: the single gate every agent-initiated approval goes
//! through before any file write or terminal execution.
//!
//! Agent-to-client `session/request_permission` requests register here; the
//! UI learns about them through [`crate::AgentEvent::PermissionRequested`]
//! and answers through [`crate::session::AgentThread::respond_permission`].
//! The broker is session-scoped and resolves outstanding requests when a
//! turn is cancelled or the connection drops, so no approval can hang a
//! shutdown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ee_agent_protocol::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
    SessionId, ToolCallUpdate,
};
use tokio::sync::oneshot;

use crate::events::{PermissionCommand, PermissionRequestId, PermissionRequestInfo};
use crate::permission_rules::{PermissionEffect, PermissionRules, PermissionTarget};

/// Outcome channel for one pending permission request.
pub(crate) type PermissionResponse = oneshot::Sender<RequestPermissionOutcome>;

#[derive(Debug)]
pub(crate) struct PendingPermission {
    pub session_id: SessionId,
    pub respond: PermissionResponse,
    target: PermissionTarget,
    options: Vec<PermissionOption>,
}

#[derive(Debug, Default)]
struct BrokerState {
    next_id: u64,
    pending: HashMap<PermissionRequestId, PendingPermission>,
}

/// Thread-safe registry of pending permission requests.
///
/// Cloning shares the same registry; the broker is cheap to pass into
/// handler closures and thread handles.
#[derive(Debug, Clone, Default)]
pub struct PermissionBroker {
    state: Arc<Mutex<BrokerState>>,
    rules: Arc<Mutex<Arc<PermissionRules>>>,
}

impl PermissionBroker {
    /// Creates an empty broker (every request asks).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a broker that settles `allow`/`deny` rules before notifying the
    /// client and remembers `allow_always` answers.
    #[must_use]
    pub fn with_rules(rules: Arc<PermissionRules>) -> Self {
        Self { state: Arc::default(), rules: Arc::new(Mutex::new(rules)) }
    }

    /// Replaces the active ruleset. Safe to call while requests are pending;
    /// only requests evaluated afterwards use the new rules.
    pub fn set_rules(&self, rules: Arc<PermissionRules>) {
        *self.rules.lock().expect("permission rules poisoned") = rules;
    }

    /// Snapshot of the active rules (configured rules plus saved approvals).
    #[must_use]
    pub fn rules(&self) -> Arc<PermissionRules> {
        self.rules.lock().expect("permission rules poisoned").clone()
    }

    /// Evaluates the configured rules for `tool_call` without asking the
    /// client. `None` means the request must follow the normal approval path.
    #[must_use]
    pub fn preflight(
        &self,
        tool_call: &ToolCallUpdate,
        options: &[PermissionOption],
    ) -> Option<RequestPermissionOutcome> {
        let target = PermissionTarget::from_tool_call(tool_call);
        let rules = self.rules.lock().expect("permission rules poisoned").clone();
        match rules.decide(&target) {
            PermissionEffect::Allow => select_option(
                options,
                &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways],
            ),
            // A deny with no reject option still fails closed as cancelled;
            // never fabricate an option id the agent did not offer.
            PermissionEffect::Deny => select_option(
                options,
                &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways],
            )
            .or(Some(RequestPermissionOutcome::Cancelled)),
            PermissionEffect::Ask => None,
        }
    }

    /// Registers a pending permission request and returns its id, a
    /// UI-safe view of it, and the outcome receiver the connection task
    /// awaits.
    pub fn request(
        &self,
        session_id: SessionId,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) -> (PermissionRequestId, PermissionRequestInfo, oneshot::Receiver<RequestPermissionOutcome>)
    {
        self.request_with_command(session_id, tool_call, options, None)
    }

    /// Like [`Self::request`], carrying an ACP v2 command subject when the
    /// agent requested approval for an agent-owned command.
    pub(crate) fn request_with_command(
        &self,
        session_id: SessionId,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        command: Option<PermissionCommand>,
    ) -> (PermissionRequestId, PermissionRequestInfo, oneshot::Receiver<RequestPermissionOutcome>)
    {
        let (respond_tx, respond_rx) = oneshot::channel::<RequestPermissionOutcome>();
        let target = PermissionTarget::from_tool_call(&tool_call);
        let mut state = self.state.lock().expect("permission broker poisoned");
        let request_id = PermissionRequestId(state.next_id);
        state.next_id += 1;
        state.pending.insert(
            request_id,
            PendingPermission {
                session_id: session_id.clone(),
                respond: respond_tx,
                target,
                options: options.clone(),
            },
        );
        let info = PermissionRequestInfo { request_id, session_id, tool_call, options, command };
        (request_id, info, respond_rx)
    }

    /// Resolves the pending request with `outcome`.
    ///
    /// Returns `false` when the request is unknown or already resolved
    /// (duplicate responses are ignored), `true` when a decision was
    /// delivered.
    pub fn respond(
        &self,
        request_id: PermissionRequestId,
        outcome: RequestPermissionOutcome,
    ) -> bool {
        let pending = {
            let mut state = self.state.lock().expect("permission broker poisoned");
            state.pending.remove(&request_id)
        };
        let Some(pending) = pending else {
            return false;
        };
        if selected_option_kind(&outcome, &pending.options)
            == Some(PermissionOptionKind::AllowAlways)
        {
            let mut rules = self.rules.lock().expect("permission rules poisoned");
            Arc::make_mut(&mut *rules).remember_allow(&pending.target);
        }
        pending.respond.send(outcome).is_ok()
    }

    /// The session a pending request belongs to, if any.
    #[must_use]
    pub fn session_of(&self, request_id: PermissionRequestId) -> Option<SessionId> {
        self.state
            .lock()
            .expect("permission broker poisoned")
            .pending
            .get(&request_id)
            .map(|pending| pending.session_id.clone())
    }

    /// Cancels every pending request for `session_id` with
    /// [`RequestPermissionOutcome::Cancelled`]; returns how many were
    /// resolved.
    pub fn cancel_session(&self, session_id: &SessionId) -> usize {
        let pending = {
            let mut state = self.state.lock().expect("permission broker poisoned");
            let ids: Vec<PermissionRequestId> = state
                .pending
                .iter()
                .filter(|(_, pending)| &pending.session_id == session_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| state.pending.remove(&id)).collect::<Vec<_>>()
        };
        let mut cancelled = 0;
        for pending in pending {
            if pending.respond.send(RequestPermissionOutcome::Cancelled).is_ok() {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Cancels every pending request on the connection; returns how many
    /// were resolved. Used on connection close so no approval outlives its
    /// agent process.
    pub fn cancel_all(&self) -> usize {
        let pending = {
            let mut state = self.state.lock().expect("permission broker poisoned");
            std::mem::take(&mut state.pending)
        };
        let mut cancelled = 0;
        for (_, pending) in pending {
            if pending.respond.send(RequestPermissionOutcome::Cancelled).is_ok() {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Number of pending approvals (for tests and status lines).
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.state.lock().expect("permission broker poisoned").pending.len()
    }
}

fn select_option(
    options: &[PermissionOption],
    kinds: &[PermissionOptionKind],
) -> Option<RequestPermissionOutcome> {
    options.iter().find(|option| kinds.contains(&option.kind)).map(|option| {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
    })
}

fn selected_option_kind(
    outcome: &RequestPermissionOutcome,
    options: &[PermissionOption],
) -> Option<PermissionOptionKind> {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => options
            .iter()
            .find(|option| option.option_id == selected.option_id)
            .map(|option| option.kind),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_rules::{PermissionRule, PermissionRuleset};
    use ee_agent_protocol::{PermissionOptionKind, ToolCallUpdateFields};

    fn tool_call_update() -> ToolCallUpdate {
        ToolCallUpdate::new("call_1", ToolCallUpdateFields::new().title("Run tests"))
    }

    fn option() -> PermissionOption {
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce)
    }

    #[test]
    fn request_then_respond_resolves_exactly_once() {
        let broker = PermissionBroker::new();
        let (id, info, _rx) =
            broker.request(SessionId::new("s1"), tool_call_update(), vec![option()]);
        assert_eq!(broker.pending_count(), 1);
        assert_eq!(info.session_id, SessionId::new("s1"));
        assert_eq!(info.tool_call.fields.title.as_deref(), Some("Run tests"));
        assert_eq!(info.options.len(), 1);

        let outcome = RequestPermissionOutcome::Selected(
            ee_agent_protocol::SelectedPermissionOutcome::new("allow_once"),
        );
        assert!(broker.respond(id, outcome.clone()));
        assert_eq!(broker.pending_count(), 0);

        // Duplicate response is ignored.
        assert!(!broker.respond(id, outcome.clone()));
        // Unknown ids are ignored too.
        assert!(!broker.respond(PermissionRequestId(999), outcome));
    }

    #[tokio::test]
    async fn request_receiver_resolves_with_the_recorded_outcome() {
        let broker = PermissionBroker::new();
        let (id, _info, rx) =
            broker.request(SessionId::new("s1"), tool_call_update(), vec![option()]);
        let outcome = RequestPermissionOutcome::Selected(
            ee_agent_protocol::SelectedPermissionOutcome::new("allow_once"),
        );
        assert!(broker.respond(id, outcome.clone()));
        assert_eq!(rx.await, Ok(outcome));
    }

    #[tokio::test]
    async fn dropping_pending_senders_resolves_receivers_as_cancelled() {
        // `cancel_all` resolves pending requests with `Cancelled`; an
        // awaiting task observes that outcome, never a hang.
        let broker = PermissionBroker::new();
        let (_id, _info, rx) =
            broker.request(SessionId::new("s1"), tool_call_update(), vec![option()]);
        assert_eq!(broker.cancel_all(), 1);
        assert_eq!(rx.await, Ok(RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn cancel_session_resolves_only_that_sessions_requests() {
        let broker = PermissionBroker::new();
        let (id_a, _info, _rx) =
            broker.request(SessionId::new("s1"), tool_call_update(), vec![option()]);
        let (id_b, _info, _rx) =
            broker.request(SessionId::new("s2"), tool_call_update(), vec![option()]);

        assert_eq!(broker.cancel_session(&SessionId::new("s1")), 1);
        assert_eq!(broker.pending_count(), 1);
        assert!(broker.session_of(id_a).is_none());
        assert_eq!(broker.session_of(id_b), Some(SessionId::new("s2")));
    }

    #[test]
    fn rules_settle_allow_and_deny_before_request() {
        let rules = Arc::new(PermissionRules::new(PermissionRuleset::new(vec![
            PermissionRule::new("bash", "*", PermissionEffect::Allow),
            PermissionRule::new("bash", "rm *", PermissionEffect::Deny),
        ])));
        let broker = PermissionBroker::with_rules(rules);
        let options = vec![
            PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
        ];

        let allow = ToolCallUpdate::new(
            "call-allow",
            ToolCallUpdateFields::new()
                .name("bash")
                .kind(ee_agent_protocol::ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": "cargo test" })),
        );
        assert_eq!(
            broker.preflight(&allow, &options),
            Some(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once")))
        );
        assert_eq!(broker.pending_count(), 0, "allow never registers a pending request");

        let deny = ToolCallUpdate::new(
            "call-deny",
            ToolCallUpdateFields::new()
                .name("bash")
                .kind(ee_agent_protocol::ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": "rm -rf /" })),
        );
        assert_eq!(
            broker.preflight(&deny, &options),
            Some(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("reject_once")))
        );
        assert_eq!(broker.pending_count(), 0, "deny never registers a pending request");

        let ask = ToolCallUpdate::new(
            "call-ask",
            ToolCallUpdateFields::new().kind(ee_agent_protocol::ToolKind::Edit),
        );
        assert_eq!(broker.preflight(&ask, &options), None);
    }

    #[test]
    fn deny_without_a_reject_option_fails_closed_as_cancelled() {
        let rules = Arc::new(PermissionRules::new(PermissionRuleset::deny_all()));
        let broker = PermissionBroker::with_rules(rules);
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new().name("bash").kind(ee_agent_protocol::ToolKind::Execute),
        );
        let only_allow = vec![PermissionOption::new(
            "allow_once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )];
        assert_eq!(
            broker.preflight(&tool_call, &only_allow),
            Some(RequestPermissionOutcome::Cancelled)
        );
    }

    #[test]
    fn allow_always_records_a_saved_approval() {
        let broker = PermissionBroker::with_rules(Arc::new(PermissionRules::new(
            PermissionRuleset::empty(),
        )));
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new()
                .name("bash")
                .kind(ee_agent_protocol::ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": "cargo test" })),
        );
        let options = vec![
            PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("allow_always", "Always", PermissionOptionKind::AllowAlways),
        ];
        let (id, _info, _rx) = broker.request(SessionId::new("s1"), tool_call, options);
        assert!(broker.respond(
            id,
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_always")),
        ));

        let target = PermissionTarget::from_tool_call(&ToolCallUpdate::new(
            "call-2",
            ToolCallUpdateFields::new()
                .name("bash")
                .kind(ee_agent_protocol::ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": "cargo test" })),
        ));
        assert_eq!(broker.rules().decide(&target), PermissionEffect::Allow);
    }

    #[test]
    fn cancel_all_resolves_everything() {
        let broker = PermissionBroker::new();
        let (_id_a, _info_a, _rx_a) =
            broker.request(SessionId::new("s1"), tool_call_update(), vec![option()]);
        let (_id_b, _info_b, _rx_b) =
            broker.request(SessionId::new("s2"), tool_call_update(), vec![option()]);
        assert_eq!(broker.cancel_all(), 2);
        assert_eq!(broker.pending_count(), 0);
    }
}
