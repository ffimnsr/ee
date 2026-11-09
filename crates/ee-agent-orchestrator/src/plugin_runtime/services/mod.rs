//! `agent.*` service implementations.
//!
//! Each service wraps an existing orchestrator store so the host has exactly one
//! extension path:
//!
//! | Service | Wraps |
//! |---|---|
//! | [`tools::ToolService`] | [`crate::tools::ToolRegistry`] |
//! | [`models::ModelService`] | [`crate::model_registry::ModelRegistry`] / [`crate::model_router::ModelRouter`] |
//! | [`policy::PolicyService`] | [`crate::policy::PolicyEngine`] plus bail listeners |
//! | [`context::ContextService`] | bounded, trust-labeled context providers |
//! | [`strategies::StrategyService`] | [`crate::strategy::StrategySelector`] |
//! | [`validation::ValidationService`] | [`crate::validation::ValidationPlanner`] |
//! | [`sessions::SessionService`] | session/turn plugin scopes |

pub mod context;
pub mod models;
pub mod policy;
pub mod router;
pub mod sessions;
pub mod strategies;
pub mod tools;
pub mod validation;

/// Stable service identifiers from the v2 design document.
pub mod ids {
    pub const TOOLS: &str = "agent.tools@1";
    pub const MODELS: &str = "agent.models@1";
    pub const STRATEGIES: &str = "agent.strategies@1";
    pub const POLICY: &str = "agent.policy@1";
    pub const CONTEXT: &str = "agent.context@1";
    pub const SESSIONS: &str = "agent.sessions@1";
    pub const VALIDATION: &str = "agent.validation@1";
}

/// Permission required to register a tool through `agent.tools`.
pub const PERM_TOOLS_REGISTER: &str = "agent.tools.register";
/// Permission required to register a model adapter or route.
pub const PERM_MODELS_REGISTER: &str = "agent.models.register";
/// Permission required to register a strategy candidate.
pub const PERM_STRATEGIES_REGISTER: &str = "agent.strategies.register";
/// Permission required to gate tool/approval decisions.
pub const PERM_POLICY_GATE: &str = "agent.policy.gate";
/// Permission required to observe policy decisions.
pub const PERM_POLICY_OBSERVE: &str = "agent.policy.observe";
/// Permission required to contribute turn context.
pub const PERM_CONTEXT_CONTRIBUTE: &str = "agent.context.contribute";
/// Permission required to read session state.
pub const PERM_SESSIONS_READ: &str = "agent.sessions.read";
/// Permission required to control sessions.
pub const PERM_SESSIONS_CONTROL: &str = "agent.sessions.control";
/// Permission required to contribute validation planners or metadata.
pub const PERM_VALIDATION_CONTRIBUTE: &str = "agent.validation.contribute";

pub use context::{
    ContextCollectRequest, ContextCollection, ContextContribution, ContextProvider, ContextService,
    DEFAULT_CONTEXT_BYTES, DEFAULT_CONTEXT_ITEMS,
};
pub use models::{ModelService, ModelServiceSnapshot};
pub use policy::{ApprovalPolicy, ApprovalRequest, MandatoryApprovalGate, PolicyService};
pub use router::AgentServiceRouter;
pub use sessions::{SessionHandle, SessionService, SessionState, TurnScope};
pub use strategies::{StrategyCandidate, StrategyService};
pub use tools::{ToolScope, ToolService};
pub use validation::{ValidationContribution, ValidationService};
