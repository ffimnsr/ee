//! Host identity and the host adapter boundary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::effect::Effect;
use crate::error::PluginError;

/// Which host owns a plugin instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    Editor,
    Agent,
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Agent => "agent",
        }
    }
}

/// Boundary implemented by the editor host, agent host, and test harnesses.
///
/// Runtime code depends on this trait instead of concrete hosts so the loader
/// stays host-agnostic.
pub trait HostAdapter: Send + Sync + 'static {
    /// Which host this adapter belongs to.
    fn host_kind(&self) -> HostKind;

    /// Apply effects produced by a command or listener.
    fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError>;
}
