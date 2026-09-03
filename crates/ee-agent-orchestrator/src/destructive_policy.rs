//! Destructive side-effect gating.
//!
//! Tools that delete, move, overwrite, chmod, kill terminals, or touch the
//! network are denied by default even when their side-effect class is
//! allowed. Each destructive subclass must be explicitly allowed through
//! [`crate::policy::ToolPolicy::allow_side_effect_subclass`]. Terminal kills
//! additionally require session ownership; external network access requires
//! a host-approved read tool.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Destructive side-effect subclass, gated separately from the class.
///
/// Every variant is denied by the default policy; explicit allowance is
/// required before any destructive tool may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SideEffectSubclass {
    /// Removing files or directories.
    Delete,
    /// Moving/renaming files or directories.
    Move,
    /// Overwriting existing file content.
    Overwrite,
    /// Changing permissions/modes (chmod-like operations).
    Chmod,
    /// Killing a terminal session.
    TerminalKill,
    /// Making an external network request.
    ExternalNetwork,
}

impl SideEffectSubclass {
    /// Stable lowercase name for diagnostics and policy reasons.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Overwrite => "overwrite",
            Self::Chmod => "chmod",
            Self::TerminalKill => "terminal_kill",
            Self::ExternalNetwork => "external_network",
        }
    }

    /// Every destructive subclass, for documentation and exhaustive tests.
    pub const ALL: [SideEffectSubclass; 6] = [
        Self::Delete,
        Self::Move,
        Self::Overwrite,
        Self::Chmod,
        Self::TerminalKill,
        Self::ExternalNetwork,
    ];
}

impl fmt::Display for SideEffectSubclass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::{RequestId, Response, SessionId, SessionUpdate, ToolCallStatus};
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::config::OrchestratorConfig;
    use crate::events::EventRecorder;
    use crate::policy::{PolicyContext, PolicyEngine, ToolPolicy};
    use crate::tools::{ToolDefinition, ToolErrorKind, ToolExecutor, ToolIntent, ToolRegistry};

    fn tool_with(
        subclass: SideEffectSubclass,
        class: crate::tools::SideEffectClass,
    ) -> ToolDefinition {
        ToolDefinition::new("tool", "test").side_effect_class(class).side_effect_subclass(subclass)
    }

    #[test]
    fn every_destructive_subclass_is_denied_by_default() {
        let engine = PolicyEngine::default();
        for subclass in SideEffectSubclass::ALL {
            let decision = engine.check(
                &tool_with(subclass, crate::tools::SideEffectClass::Write),
                PolicyContext::default(),
            );
            assert!(!decision.allow, "{subclass} must fail closed");
            let reason = decision.reason.expect("deny carries a reason");
            assert!(reason.contains("explicit policy allowance"), "{reason}");
        }
    }

    #[test]
    fn delete_is_denied_even_when_write_class_is_allowed() {
        let engine = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
        let decision = engine.check(
            &tool_with(SideEffectSubclass::Delete, crate::tools::SideEffectClass::Write),
            PolicyContext::default(),
        );
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("destructive side effect delete"));
    }

    #[test]
    fn overwrite_is_denied_without_configured_allowance() {
        let engine = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
        let decision = engine.check(
            &tool_with(SideEffectSubclass::Overwrite, crate::tools::SideEffectClass::Write),
            PolicyContext::default(),
        );
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("destructive side effect overwrite"));
    }

    #[test]
    fn explicit_allowance_permits_the_subclass_only() {
        let engine = PolicyEngine::new(
            ToolPolicy { allow_write: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::Overwrite),
        );
        let policy = engine.policy();
        assert!(policy.allowed_side_effect_subclasses.contains(&SideEffectSubclass::Overwrite));
        assert!(
            engine
                .check(
                    &tool_with(SideEffectSubclass::Overwrite, crate::tools::SideEffectClass::Write),
                    PolicyContext::default()
                )
                .allow
        );
        assert!(
            !engine
                .check(
                    &tool_with(SideEffectSubclass::Delete, crate::tools::SideEffectClass::Write),
                    PolicyContext::default()
                )
                .allow,
            "allowance is per-subclass, not blanket"
        );
    }

    #[test]
    fn terminal_kill_is_denied_outside_owned_terminal_scope() {
        let engine = PolicyEngine::new(
            ToolPolicy { allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::TerminalKill),
        );
        let tool =
            tool_with(SideEffectSubclass::TerminalKill, crate::tools::SideEffectClass::Execute);
        let unowned = engine.check_with_arguments(
            &tool,
            PolicyContext::default(),
            &json!({ "terminal_id": "term-9" }),
        );
        assert!(!unowned.allow);
        assert!(unowned.reason.unwrap().contains("not owned"));

        let engine = PolicyEngine::new(
            ToolPolicy { allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::TerminalKill)
                .with_owned_terminal("term-1"),
        );
        let owned = engine.check_with_arguments(
            &tool,
            PolicyContext::default(),
            &json!({ "terminal_id": "term-1" }),
        );
        assert!(owned.allow);
    }

    #[test]
    fn terminal_kill_without_argument_is_denied() {
        let engine = PolicyEngine::new(
            ToolPolicy { allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::TerminalKill)
                .with_owned_terminal("term-1"),
        );
        let tool =
            tool_with(SideEffectSubclass::TerminalKill, crate::tools::SideEffectClass::Execute);
        let decision = engine.check_with_arguments(&tool, PolicyContext::default(), &json!({}));
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("terminal_id"));
    }

    #[test]
    fn subclasses_serialize_deterministically() {
        for subclass in SideEffectSubclass::ALL {
            let json = serde_json::to_string(&subclass).expect("serializes");
            let restored: SideEffectSubclass = serde_json::from_str(&json).expect("parses");
            assert_eq!(restored, subclass);
            assert_eq!(subclass.to_string(), subclass.as_str());
        }
    }

    // ── Executor-level gates ─────────────────────────────────────────────

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn harness(policy: PolicyEngine) -> (Arc<Mutex<ToolRegistry>>, ToolExecutor) {
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let budget =
            Arc::new(Mutex::new(crate::budget::BudgetTracker::new(&OrchestratorConfig::default())));
        let executor = ToolExecutor::new(
            OrchestratorConfig::default(),
            tools.clone(),
            budget,
            policy,
            0,
            EventRecorder::new(),
        );
        (tools, executor)
    }

    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    fn task_fixture() -> crate::tasks::TaskNode {
        crate::tasks::TaskNode::new(crate::tasks::TaskId::new("task-1"), "t", "d")
    }

    #[tokio::test]
    async fn builtin_write_file_is_denied_by_overwrite_gate_before_bridge() {
        let (sink, bridge, mut rx) = plumbing();
        let policy = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
        let (tools, executor) = harness(policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "write_file", json!({ "path": "/a", "content": "x" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert!(result.text_output.contains("destructive side effect overwrite"));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert!(rx.try_recv().is_err(), "overwrite gate must block the bridge request");
    }

    #[tokio::test]
    async fn builtin_kill_terminal_is_denied_outside_owned_terminal_scope() {
        let (sink, bridge, mut rx) = plumbing();
        let policy = PolicyEngine::new(
            ToolPolicy { allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::TerminalKill),
        );
        let (tools, executor) = harness(policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "kill_terminal", json!({ "terminal_id": "term-9" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let result = executor
            .execute(&intent, &sink, &bridge, cancel_rx, &task_fixture(), &[])
            .await
            .expect("executes");

        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert!(result.text_output.contains("not owned"));
        let SessionUpdate::ToolCallUpdate(failed) = next_update(&mut rx).await else {
            panic!("expected failed tool update");
        };
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert!(rx.try_recv().is_err(), "unowned kill must not reach the bridge");
    }

    #[tokio::test]
    async fn owned_terminal_kill_runs_and_reaches_bridge() {
        let (sink, bridge, mut rx) = plumbing();
        let policy = PolicyEngine::new(
            ToolPolicy { allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::TerminalKill)
                .with_owned_terminal("term-1"),
        );
        let (tools, executor) = harness(policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let intent = ToolIntent::new("tc-1", "kill_terminal", json!({ "terminal_id": "term-1" }));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let bridge_task = bridge.clone();
        let executor_task = tokio::spawn(async move {
            executor.execute(&intent, &sink, &bridge_task, cancel_rx, &task_fixture(), &[]).await
        });

        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::ToolCallUpdate(_)));
        match rx.recv().await.expect("client request queued") {
            OutboundEvent::ClientRequest { .. } => {}
            other => panic!("expected client request, got {other:?}"),
        }
        bridge.handle_response(Response::Result { id: RequestId::Number(1), result: json!({}) });
        let result = executor_task.await.expect("task joins").expect("executes");
        assert!(result.success);
    }

    #[test]
    fn policy_with_subclass_fields_roundtrips_through_json() {
        let policy = ToolPolicy::default()
            .allow_side_effect_subclass(SideEffectSubclass::Delete)
            .with_owned_terminal("term-1");
        let json = serde_json::to_string(&policy).expect("serializes");
        let restored: ToolPolicy = serde_json::from_str(&json).expect("parses");
        assert_eq!(
            restored.allowed_side_effect_subclasses,
            HashSet::from([SideEffectSubclass::Delete])
        );
        assert!(restored.owned_terminal_ids.contains("term-1"));
        assert_eq!(restored.allow_read, policy.allow_read);
    }
}
