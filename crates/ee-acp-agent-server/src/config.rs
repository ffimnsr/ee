//! Framework configuration.
//!
//! [`AcpAgentServerConfig`] carries the framework-wide knobs that later
//! phases use when wiring transports, timeouts, and session handling.

use std::time::Duration;

use ee_agent_protocol::Implementation;
use serde::{Deserialize, Serialize};

/// Default request timeout: 30 seconds.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on one transport frame, in bytes: 4 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Default prefix for generated session ids.
pub const DEFAULT_SESSION_ID_PREFIX: &str = "session";

/// Shared configuration for one [`crate::server::AcpAgentServer`] instance
/// (server module lands in a later phase).  Exhaustive so callers can use
/// field updates off [`Default`]; construct with
/// [`AcpAgentServerConfig::default`] and override the knobs needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpAgentServerConfig {
    /// How long a client request may take before it fails with a request
    /// timeout error.
    pub request_timeout: Duration,
    /// Optional per-prompt timeout; `None` falls back to [`Self::request_timeout`].
    pub prompt_timeout: Option<Duration>,
    /// Maximum accepted transport frame size in bytes.  Frames larger than
    /// this fail closed before any parsing happens.
    pub max_frame_bytes: usize,
    /// Prefix used by the session-id generator for provider-created sessions.
    pub session_id_prefix: String,
    /// Agent identity advertised in `initialize` responses.
    pub implementation: Implementation,
}

impl Default for AcpAgentServerConfig {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            prompt_timeout: None,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            session_id_prefix: DEFAULT_SESSION_ID_PREFIX.to_string(),
            implementation: Implementation::new("ee-acp-agent-server", env!("CARGO_PKG_VERSION"))
                .title("EE ACP Agent Server"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_framework_knobs() {
        let config = AcpAgentServerConfig::default();
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.prompt_timeout, None);
        assert_eq!(config.max_frame_bytes, 4 * 1024 * 1024);
        assert_eq!(config.session_id_prefix, "session");
    }

    #[test]
    fn defaults_identify_the_framework() {
        let config = AcpAgentServerConfig::default();
        assert_eq!(config.implementation.name, "ee-acp-agent-server");
        assert_eq!(config.implementation.title.as_deref(), Some("EE ACP Agent Server"));
        assert_eq!(config.implementation.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn serde_roundtrip_preserves_config() {
        let config = AcpAgentServerConfig {
            request_timeout: Duration::from_secs(5),
            prompt_timeout: Some(Duration::from_secs(2)),
            max_frame_bytes: 1024,
            session_id_prefix: "provider-session".to_string(),
            implementation: Implementation::new("test-provider", "1.2.3"),
        };
        let json = serde_json::to_string(&config).expect("config serializes");
        let restored: AcpAgentServerConfig = serde_json::from_str(&json).expect("config parses");
        assert_eq!(restored.request_timeout, config.request_timeout);
        assert_eq!(restored.prompt_timeout, config.prompt_timeout);
        assert_eq!(restored.max_frame_bytes, config.max_frame_bytes);
        assert_eq!(restored.session_id_prefix, config.session_id_prefix);
        assert_eq!(restored.implementation.name, "test-provider");
        assert_eq!(restored.implementation.version, "1.2.3");
    }
}
