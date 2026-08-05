//! Conservative default tool policy.
//!
//! Read tools are allowed by default; write, execute, and delegate tools
//! fail closed unless explicitly allowed.  Delegate tools are additionally
//! bounded by subagent depth and parallel-count limits (subagent phase).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::destructive_policy::SideEffectSubclass;
use crate::tools::{SideEffectClass, ToolDefinition};
use crate::workspace_scope::WorkspaceScope;

/// Policy knobs for side-effect classes.
///
/// Defaults fail closed: only read-class tools are allowed, and delegate
/// tools are denied outright until explicitly enabled.  Destructive
/// side-effect subclasses (delete, overwrite, terminal kill, ...) are denied
/// by default even when their class is allowed, and terminal kills require
/// the target terminal to be owned by the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Whether read-class tools are allowed.
    pub allow_read: bool,
    /// Whether write-class tools are allowed.
    pub allow_write: bool,
    /// Whether execute-class tools are allowed.
    pub allow_execute: bool,
    /// Whether delegate-class tools are allowed.
    pub allow_delegate: bool,
    /// Maximum delegate nesting depth (0 denies every delegate).
    pub max_delegate_depth: usize,
    /// Maximum concurrently active delegates per delegating agent.
    pub max_parallel_delegates: usize,
    /// Destructive side-effect subclasses explicitly allowed; empty denies
    /// every destructive subclass even when its class is allowed.
    pub allowed_side_effect_subclasses: HashSet<SideEffectSubclass>,
    /// Terminal ids this session owns; `kill_terminal` requires the target
    /// here in addition to subclass allowance.
    pub owned_terminal_ids: HashSet<String>,
    /// Optional workspace scope; when set, file/cwd paths are checked before
    /// any client-bridge call.
    pub scope: Option<WorkspaceScope>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            allow_read: true,
            allow_write: false,
            allow_execute: false,
            allow_delegate: false,
            max_delegate_depth: 2,
            max_parallel_delegates: 4,
            allowed_side_effect_subclasses: HashSet::new(),
            owned_terminal_ids: HashSet::new(),
            scope: None,
        }
    }
}

impl ToolPolicy {
    /// Explicitly allows one destructive side-effect subclass.
    #[must_use]
    pub fn allow_side_effect_subclass(mut self, subclass: SideEffectSubclass) -> Self {
        self.allowed_side_effect_subclasses.insert(subclass);
        self
    }

    /// Marks a terminal as owned by this session (required for `kill_terminal`).
    #[must_use]
    pub fn with_owned_terminal(mut self, terminal_id: impl Into<String>) -> Self {
        self.owned_terminal_ids.insert(terminal_id.into());
        self
    }

    /// Activates a workspace scope; when set, file/cwd paths outside it are
    /// rejected before execution.
    #[must_use]
    pub fn with_scope(mut self, scope: WorkspaceScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

/// Delegation context a policy check runs under.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyContext {
    /// Nesting depth of the agent about to delegate (root is 0).
    pub subagent_depth: usize,
    /// Delegates currently running under that agent.
    pub active_delegates: usize,
}

/// Outcome of one policy check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Whether the operation may proceed.
    pub allow: bool,
    /// Why it was denied, when denied.
    pub reason: Option<String>,
}

impl PolicyDecision {
    /// Builds an allow decision.
    #[must_use]
    pub fn allowed() -> Self {
        Self { allow: true, reason: None }
    }

    /// Builds a deny decision with a reason.
    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self { allow: false, reason: Some(reason.into()) }
    }
}

/// Immutable policy gate checked before every tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyEngine {
    policy: ToolPolicy,
}

impl PolicyEngine {
    /// Creates an engine from a policy.
    #[must_use]
    pub fn new(policy: ToolPolicy) -> Self {
        Self { policy }
    }

    /// The active policy.
    #[must_use]
    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }

    /// Checks a tool definition against the policy, including the
    /// destructive-subclass gate.
    #[must_use]
    pub fn check(&self, tool: &ToolDefinition, context: PolicyContext) -> PolicyDecision {
        let decision = match tool.side_effect_class {
            SideEffectClass::Read => self
                .allow_or(self.policy.allow_read, "read tools require explicit policy allowance"),
            SideEffectClass::Write => self
                .allow_or(self.policy.allow_write, "write tools require explicit policy allowance"),
            SideEffectClass::Execute => self.allow_or(
                self.policy.allow_execute,
                "execute tools require explicit policy allowance",
            ),
            SideEffectClass::Delegate => self.check_delegate(context),
        };
        if !decision.allow {
            return decision;
        }
        if let Some(subclass) = tool.side_effect_subclass
            && !self.policy.allowed_side_effect_subclasses.contains(&subclass)
        {
            return PolicyDecision::denied(format!(
                "destructive side effect {} requires explicit policy allowance",
                subclass.as_str()
            ));
        }
        PolicyDecision::allowed()
    }

    /// Checks a tool definition with its concrete arguments: the class and
    /// subclass gates from [`PolicyEngine::check`], terminal ownership for
    /// `kill_terminal`, and the active workspace scope for path-bearing
    /// tools.
    #[must_use]
    pub fn check_with_arguments(
        &self,
        tool: &ToolDefinition,
        context: PolicyContext,
        arguments: &serde_json::Value,
    ) -> PolicyDecision {
        let decision = self.check(tool, context);
        if !decision.allow {
            return decision;
        }
        if tool.side_effect_subclass == Some(SideEffectSubclass::TerminalKill) {
            match arguments.get("terminal_id").and_then(serde_json::Value::as_str) {
                Some(id) if self.policy.owned_terminal_ids.contains(id) => {}
                Some(id) => {
                    return PolicyDecision::denied(format!(
                        "terminal {id} is not owned by this session"
                    ));
                }
                None => {
                    return PolicyDecision::denied("kill_terminal requires a terminal_id argument");
                }
            }
        }
        if let Some(scope) = &self.policy.scope
            && let Err(reason) = scope.check_arguments(tool.side_effect_class, arguments)
        {
            return PolicyDecision::denied(reason);
        }
        PolicyDecision::allowed()
    }

    fn check_delegate(&self, context: PolicyContext) -> PolicyDecision {
        if !self.policy.allow_delegate {
            return PolicyDecision::denied("delegate tools require explicit policy allowance");
        }
        if context.subagent_depth >= self.policy.max_delegate_depth {
            return PolicyDecision::denied(format!(
                "subagent depth {} exceeds max delegate depth {}",
                context.subagent_depth, self.policy.max_delegate_depth
            ));
        }
        if context.active_delegates >= self.policy.max_parallel_delegates {
            return PolicyDecision::denied(format!(
                "active delegates {} exceed max parallel delegates {}",
                context.active_delegates, self.policy.max_parallel_delegates
            ));
        }
        PolicyDecision::allowed()
    }

    fn allow_or(&self, allowed: bool, denied_reason: &'static str) -> PolicyDecision {
        if allowed { PolicyDecision::allowed() } else { PolicyDecision::denied(denied_reason) }
    }
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new(ToolPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(class: SideEffectClass) -> ToolDefinition {
        ToolDefinition::new("tool", "test").side_effect_class(class)
    }

    #[test]
    fn read_tools_are_allowed_by_default() {
        let engine = PolicyEngine::default();
        let decision = engine.check(&tool(SideEffectClass::Read), PolicyContext::default());
        assert!(decision.allow);
        assert_eq!(decision.reason, None);
    }

    #[test]
    fn write_and_execute_tools_are_denied_by_default() {
        let engine = PolicyEngine::default();
        for class in [SideEffectClass::Write, SideEffectClass::Execute, SideEffectClass::Delegate] {
            let decision = engine.check(&tool(class), PolicyContext::default());
            assert!(!decision.allow, "{class:?} must fail closed");
            let reason = decision.reason.expect("deny carries a reason");
            assert!(reason.contains("explicit policy allowance"), "{reason}");
        }
    }

    #[test]
    fn read_tools_can_be_denied_explicitly() {
        let policy = ToolPolicy { allow_read: false, ..ToolPolicy::default() };
        let engine = PolicyEngine::new(policy);
        let decision = engine.check(&tool(SideEffectClass::Read), PolicyContext::default());
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("read tools"));
    }

    #[test]
    fn explicit_allowance_permits_write_and_execute() {
        let engine = PolicyEngine::new(ToolPolicy {
            allow_write: true,
            allow_execute: true,
            ..ToolPolicy::default()
        });
        assert!(engine.check(&tool(SideEffectClass::Write), PolicyContext::default()).allow);
        assert!(engine.check(&tool(SideEffectClass::Execute), PolicyContext::default()).allow);
        assert!(!engine.check(&tool(SideEffectClass::Delegate), PolicyContext::default()).allow);
    }

    #[test]
    fn delegate_depth_limit_denies_beyond_max_depth() {
        let engine = PolicyEngine::new(ToolPolicy {
            allow_delegate: true,
            max_delegate_depth: 2,
            ..ToolPolicy::default()
        });
        let at_limit = PolicyContext { subagent_depth: 1, active_delegates: 0 };
        assert!(engine.check(&tool(SideEffectClass::Delegate), at_limit).allow);
        let beyond = PolicyContext { subagent_depth: 2, active_delegates: 0 };
        let decision = engine.check(&tool(SideEffectClass::Delegate), beyond);
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("depth"));
    }

    #[test]
    fn delegate_parallel_limit_denies_beyond_max_count() {
        let engine = PolicyEngine::new(ToolPolicy {
            allow_delegate: true,
            max_parallel_delegates: 2,
            ..ToolPolicy::default()
        });
        let at_limit = PolicyContext { subagent_depth: 0, active_delegates: 1 };
        assert!(engine.check(&tool(SideEffectClass::Delegate), at_limit).allow);
        let beyond = PolicyContext { subagent_depth: 0, active_delegates: 2 };
        let decision = engine.check(&tool(SideEffectClass::Delegate), beyond);
        assert!(!decision.allow);
        assert!(decision.reason.unwrap().contains("parallel"));
    }

    #[test]
    fn zero_max_depth_denies_every_delegate() {
        let engine = PolicyEngine::new(ToolPolicy {
            allow_delegate: true,
            max_delegate_depth: 0,
            ..ToolPolicy::default()
        });
        assert!(!engine.check(&tool(SideEffectClass::Delegate), PolicyContext::default()).allow);
    }

    #[test]
    fn policy_roundtrips_through_json() {
        let policy = ToolPolicy {
            allow_write: true,
            allow_execute: false,
            allow_delegate: true,
            max_delegate_depth: 2,
            max_parallel_delegates: 3,
            ..ToolPolicy::default()
        };
        let json = serde_json::to_string(&policy).expect("serializes");
        let restored: ToolPolicy = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, policy);
    }
}
