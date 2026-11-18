//! Model-adapter failure type.

/// Model-adapter failure, distinct from loop failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// The underlying model backend failed.
    Adapter(String),
    /// The adapter returned a response shape the orchestrator rejects.
    InvalidResponse(String),
    /// The adapter observed cancellation and stopped.
    Cancelled,
    /// The provider returned a typed failure the loop can act on (a context
    /// overflow triggers compaction, a quota failure reports its reset time).
    Provider(ee_agent_acp_server::ProviderError),
    /// The caller forced provider-native compaction on a route that does not
    /// advertise it. The message names the provider, model, and route.
    UnsupportedCompaction(String),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adapter(message) => write!(f, "model adapter failed: {message}"),
            Self::InvalidResponse(message) => write!(f, "invalid model response: {message}"),
            Self::Cancelled => f.write_str("model call cancelled"),
            Self::Provider(error) => write!(f, "provider failure: {error}"),
            Self::UnsupportedCompaction(message) => {
                write!(f, "provider compaction is not supported: {message}")
            }
        }
    }
}

impl std::error::Error for ModelError {}
