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
    ///
    /// `plugin` is the originating plugin when the runtime knows it (native
    /// transports always do). In-process callers may pass `None`.
    fn submit_effects(
        &self,
        plugin: Option<&crate::ids::PluginId>,
        effects: Vec<Effect>,
    ) -> Result<(), PluginError>;

    /// Answer a service call initiated by a plugin.
    ///
    /// Hosts that expose services override this; the default fails closed with a
    /// typed unsupported error so a runtime test host cannot silently drop a
    /// plugin request.
    fn handle_service_request(
        &self,
        _plugin: &crate::ids::PluginId,
        _request: &crate::wire::ServiceRequestParams,
    ) -> Result<serde_json::Value, PluginError> {
        Err(PluginError::Unsupported { feature: "service requests" })
    }
}
