//! `server/discover` snapshot parsing, version pinning, and caching.
//!
//! The wire types come from [`rmcp`] (`DiscoverResult`, `ServerCapabilities`,
//! `CacheScope`); ee-owned code adds the version pin (only `2026-07-28`),
//! the deprecated-capability analysis, and the TTL cache.

use std::time::{Duration, Instant};

use rmcp::model::{
    CacheScope, DiscoverResult, Implementation, ProtocolVersion, ServerCapabilities,
};

use crate::McpError;

/// The protocol version constant as an `rmcp` value.
pub fn pinned_protocol_version() -> ProtocolVersion {
    ProtocolVersion::V_2026_07_28
}

/// Analyzed capability view of one server after `server/discover`.
///
/// Deprecated capabilities (`logging`, and anything under `experimental` /
/// `extensions` that would enable roots/sampling/dynamic registration) are
/// never exposed here: `logging` is surfaced only as a diagnostics flag, and
/// roots/sampling/registration have no snapshot fields at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    /// Whether the server offers `tools` primitives.
    pub tools: bool,
    /// Whether the server offers `resources` primitives.
    pub resources: bool,
    /// Whether the server advertises `resources.listChanged`.
    pub resources_list_changed: bool,
    /// Whether the server offers `prompts` primitives.
    pub prompts: bool,
    /// Whether the server advertises `prompts.listChanged`.
    pub prompts_list_changed: bool,
    /// Whether the server advertises `tools.listChanged`.
    pub tools_list_changed: bool,
    /// Whether the server advertises `completions`.
    pub completions: bool,
    /// Deprecated `logging` capability present — treated as diagnostics-only.
    pub logging_diagnostics_only: bool,
}

impl CapabilitySnapshot {
    /// Builds the snapshot from negotiated server capabilities.
    #[must_use]
    pub fn from_capabilities(capabilities: &ServerCapabilities) -> Self {
        Self {
            tools: capabilities.tools.is_some(),
            tools_list_changed: capabilities
                .tools
                .as_ref()
                .and_then(|tools| tools.list_changed)
                .unwrap_or(false),
            resources: capabilities.resources.is_some(),
            resources_list_changed: capabilities
                .resources
                .as_ref()
                .and_then(|resources| resources.list_changed)
                .unwrap_or(false),
            prompts: capabilities.prompts.is_some(),
            prompts_list_changed: capabilities
                .prompts
                .as_ref()
                .and_then(|prompts| prompts.list_changed)
                .unwrap_or(false),
            completions: capabilities.completions.is_some(),
            logging_diagnostics_only: capabilities.logging.is_some(),
        }
    }

    /// Whether any primitive list-changed capability is advertised.
    #[must_use]
    pub fn any_list_changed(&self) -> bool {
        self.tools_list_changed || self.resources_list_changed || self.prompts_list_changed
    }
}

/// The parsed and validated result of one `server/discover` round.
#[derive(Debug, Clone)]
pub struct DiscoverySnapshot {
    /// Server implementation identity, when provided.
    pub server_info: Option<Implementation>,
    /// Negotiated capability view.
    pub capabilities: CapabilitySnapshot,
    /// Versions the server advertised (only `2026-07-28` survives pinning).
    pub supported_versions: Vec<String>,
    /// Server-provided instructions, when provided.
    pub instructions: Option<String>,
    /// Server-provided cache lifetime hint.
    pub ttl_ms: u64,
    /// Server-provided cache scope.
    pub cache_scope: CacheScope,
}

impl DiscoverySnapshot {
    /// Parses and validates a discovery result.
    ///
    /// # Errors
    ///
    /// Rejects servers that do not advertise `2026-07-28`.
    pub fn parse(result: DiscoverResult) -> Result<Self, McpError> {
        let supported: Vec<String> =
            result.supported_versions.iter().map(ToString::to_string).collect();
        if !result.supported_versions.contains(&ProtocolVersion::V_2026_07_28) {
            return Err(McpError::UnsupportedProtocolVersion { server_supported: supported });
        }
        Ok(Self {
            server_info: result.server_info(),
            capabilities: CapabilitySnapshot::from_capabilities(&result.capabilities),
            supported_versions: supported,
            instructions: result.instructions,
            ttl_ms: result.ttl_ms,
            cache_scope: result.cache_scope,
        })
    }

    /// Effective freshness window for this discovery result.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms)
    }

    /// Cache scope is `Public` when the server allows sharing across
    /// authorization contexts.
    #[must_use]
    pub fn shareable(&self) -> bool {
        self.cache_scope == CacheScope::Public
    }
}

/// TTL cache for one server's discovery snapshot.
#[derive(Debug, Default)]
pub struct DiscoveryCache {
    snapshot: Option<DiscoverySnapshot>,
    discovered_at: Option<Instant>,
}

impl DiscoveryCache {
    /// Stores a fresh snapshot.
    pub fn store(&mut self, snapshot: DiscoverySnapshot) {
        self.discovered_at = Some(Instant::now());
        self.snapshot = Some(snapshot);
    }

    /// The cached snapshot, if any.
    #[must_use]
    pub fn get(&self) -> Option<&DiscoverySnapshot> {
        self.snapshot.as_ref()
    }

    /// Whether the cached snapshot is still within its server-provided TTL.
    ///
    /// A zero TTL means the server asked for no caching (always stale).
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        let (Some(snapshot), Some(discovered_at)) = (&self.snapshot, self.discovered_at) else {
            return false;
        };
        let ttl = snapshot.ttl();
        !ttl.is_zero() && discovered_at.elapsed() < ttl
    }

    /// Forces the snapshot stale (list-changed notification).
    pub fn invalidate(&mut self) {
        self.discovered_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        CacheScope, DiscoverResult, PromptsCapability, ProtocolVersion, ResourcesCapability,
        ResultType, ServerCapabilities, ToolsCapability,
    };

    fn discover_result(versions: Vec<ProtocolVersion>) -> DiscoverResult {
        let mut result = DiscoverResult::new(versions, ServerCapabilities::default());
        result.cache_scope = CacheScope::Public;
        result.ttl_ms = 5000;
        result
    }

    #[test]
    fn parse_accepts_2026_07_28_only() {
        let ok =
            discover_result(vec![ProtocolVersion::V_2025_11_25, ProtocolVersion::V_2026_07_28]);
        let snapshot = DiscoverySnapshot::parse(ok).expect("accepted");
        assert_eq!(snapshot.supported_versions, vec!["2025-11-25", "2026-07-28"]);
        assert!(snapshot.shareable());
    }

    #[test]
    fn parse_rejects_missing_2026_07_28() {
        let bad = discover_result(vec![ProtocolVersion::V_2025_11_25]);
        let error = DiscoverySnapshot::parse(bad).expect_err("rejected");
        assert!(error.is_unsupported_version());
    }

    #[test]
    fn deprecated_logging_is_diagnostics_only() {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability::default());
        capabilities.logging = Some(serde_json::Map::new());
        let snapshot = CapabilitySnapshot::from_capabilities(&capabilities);
        assert!(snapshot.tools);
        assert!(snapshot.logging_diagnostics_only);
        assert!(!snapshot.any_list_changed());
    }

    #[test]
    fn list_changed_flags_are_parsed() {
        let mut capabilities = ServerCapabilities::default();
        let mut tools = ToolsCapability::default();
        tools.list_changed = Some(true);
        capabilities.tools = Some(tools);
        let mut prompts = PromptsCapability::default();
        prompts.list_changed = Some(true);
        capabilities.prompts = Some(prompts);
        let mut resources = ResourcesCapability::default();
        resources.list_changed = Some(true);
        capabilities.resources = Some(resources);
        let snapshot = CapabilitySnapshot::from_capabilities(&capabilities);
        assert!(snapshot.tools_list_changed);
        assert!(snapshot.prompts_list_changed);
        assert!(snapshot.resources_list_changed);
        assert!(snapshot.any_list_changed());
    }

    #[test]
    fn cache_ttl_and_invalidation() {
        let mut cache = DiscoveryCache::default();
        assert!(!cache.is_fresh());
        cache.store(
            DiscoverySnapshot::parse(discover_result(vec![ProtocolVersion::V_2026_07_28]))
                .expect("snapshot"),
        );
        assert!(cache.is_fresh());
        cache.invalidate();
        assert!(!cache.is_fresh());
        let _ = ResultType::COMPLETE;
    }
}
