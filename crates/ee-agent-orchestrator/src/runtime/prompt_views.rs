//! Read-only prompt views over runtime state.
//!
//! Kept small and separate so the prompt boundary never reaches into registry
//! internals.

use super::*;

use crate::policy::PolicyContext;

impl OrchestratorRuntime {
    /// Names of registered tools that the active policy admits for a root turn.
    ///
    /// Guidance sections use this set so prompt prose can never describe a tool
    /// that policy hides. MCP tools appear here only after they are registered
    /// for the prompt.
    #[must_use]
    pub(crate) fn advertised_tool_names(&self) -> Vec<String> {
        let policy = self.policy.read().expect("policy poisoned");
        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        definitions
            .into_iter()
            .filter(|definition| policy.check(definition, PolicyContext::default()).allow)
            .map(|definition| definition.name)
            .collect()
    }
}
