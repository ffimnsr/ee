//! `agent.tools@1`: scoped `ServerTool` registration over [`ToolRegistry`].
//!
//! Execution still runs through [`crate::tools::ToolExecutor`], so plugin tools
//! inherit the cache, retries, write transactions, evidence checks, and policy
//! gates. This service owns registration, ownership, and scoped disposal only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ee_agent_protocol::SessionId;
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::tools::{ServerTool, ToolDefinition, ToolRegistry};

/// Maximum tools a single plugin may register on one host.
pub const MAX_PLUGIN_TOOLS: usize = 64;

/// Lifetime scope of a contributed tool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ToolScope {
    /// Lives until the host stops.
    Host,
    /// Lives until the owning agent session closes.
    Session { session_id: String },
    /// Lives until the current prompt turn ends.
    Turn { session_id: String, turn: u64 },
}

impl ToolScope {
    /// Stable label used in diagnostics and ownership checks.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Host => "host".to_owned(),
            Self::Session { session_id } => format!("session:{session_id}"),
            Self::Turn { session_id, turn } => format!("turn:{session_id}:{turn}"),
        }
    }

    /// Session this scope belongs to, when narrowed to one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Host => None,
            Self::Session { session_id } | Self::Turn { session_id, .. } => Some(session_id),
        }
    }

    fn matches_registration(&self, expected_session: Option<&str>) -> bool {
        match (self.session_id(), expected_session) {
            // Registration without a caller session trusts the declared scope.
            (_, None) => true,
            (Some(scope), Some(expected)) => scope == expected,
            (None, Some(_)) => false,
        }
    }
}

#[derive(Debug, Clone)]
struct OwnedTool {
    plugin: PluginId,
    scope: ToolScope,
}

type OwnerMap = Arc<Mutex<BTreeMap<String, OwnedTool>>>;

/// Registry service for plugin-contributed tools.
pub struct ToolService {
    tools: Arc<Mutex<ToolRegistry>>,
    owners: OwnerMap,
    max_tools_per_plugin: usize,
}

impl Service for ToolService {
    const ID: &'static str = "agent.tools@1";
}

impl Default for ToolService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: Arc::new(Mutex::new(ToolRegistry::new())),
            owners: Arc::new(Mutex::new(BTreeMap::new())),
            max_tools_per_plugin: MAX_PLUGIN_TOOLS,
        }
    }

    /// Build a service over an injected registry (tests and shared runtimes).
    #[must_use]
    pub fn with_registry(tools: Arc<Mutex<ToolRegistry>>) -> Self {
        Self {
            tools,
            owners: Arc::new(Mutex::new(BTreeMap::new())),
            max_tools_per_plugin: MAX_PLUGIN_TOOLS,
        }
    }

    /// Shared registry handle.
    #[must_use]
    pub fn registry(&self) -> Arc<Mutex<ToolRegistry>> {
        Arc::clone(&self.tools)
    }

    /// Registers a tool for `plugin` under `scope`.
    ///
    /// Fails closed on empty names, duplicate names, per-plugin quota, and a
    /// session-scoped registration whose session does not match the caller's
    /// session.
    pub fn register(
        &self,
        plugin: &PluginId,
        scope: ToolScope,
        tool: Arc<dyn ServerTool>,
    ) -> Result<Disposer, PluginError> {
        self.register_for_session(plugin, scope, None, tool)
    }

    /// Registers a tool and verifies the scope against the active session.
    pub fn register_for_session(
        &self,
        plugin: &PluginId,
        scope: ToolScope,
        active_session: Option<&str>,
        tool: Arc<dyn ServerTool>,
    ) -> Result<Disposer, PluginError> {
        let plugin = plugin.clone();
        if !scope.matches_registration(active_session) {
            return Err(PluginError::Runtime {
                message: format!(
                    "tool scope `{}` does not match the active session {active_session:?}",
                    scope.label()
                ),
            });
        }
        let name = tool.definition().name;
        if name.is_empty() {
            return Err(PluginError::Runtime { message: "tool name must not be empty".to_owned() });
        }
        {
            let owners = self.owners.lock().expect("tool ownership lock poisoned");
            let count = owners.values().filter(|owned| owned.plugin == plugin).count();
            if count >= self.max_tools_per_plugin {
                return Err(PluginError::Resource {
                    message: format!(
                        "plugin `{plugin}` exceeded the tool registration limit of {}",
                        self.max_tools_per_plugin
                    ),
                });
            }
        }
        self.tools
            .lock()
            .expect("tool registry poisoned")
            .register(tool)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        self.owners
            .lock()
            .expect("tool ownership lock poisoned")
            .insert(name.clone(), OwnedTool { plugin: plugin.clone(), scope: scope.clone() });

        let tools = Arc::clone(&self.tools);
        let owners = Arc::clone(&self.owners);
        Ok(Disposer::new(move || {
            let mut owners = owners.lock().expect("tool ownership lock poisoned");
            let still_owned = owners
                .get(&name)
                .is_some_and(|owned| owned.plugin == plugin && owned.scope == scope);
            if still_owned {
                owners.remove(&name);
                if let Ok(mut registry) = tools.lock() {
                    registry.remove(&name);
                }
            }
        }))
    }

    /// Removes one tool by name.
    pub fn remove(&self, name: &str) -> bool {
        let removed = self.tools.lock().expect("tool registry poisoned").remove(name).is_some();
        if removed {
            self.owners.lock().expect("tool ownership lock poisoned").remove(name);
        }
        removed
    }

    /// Removes one tool only when `plugin` owns it.
    pub fn remove_owned(&self, plugin: &PluginId, name: &str) -> bool {
        let owned = self
            .owners
            .lock()
            .expect("tool ownership lock poisoned")
            .get(name)
            .is_some_and(|owned| owned.plugin == *plugin);
        if !owned {
            return false;
        }
        self.remove(name)
    }

    /// Removes every tool registered under `scope`; returns removed names.
    pub fn remove_scope(&self, scope: &ToolScope) -> Vec<String> {
        let mut removed = Vec::new();
        let mut owners = self.owners.lock().expect("tool ownership lock poisoned");
        let names = owners
            .iter()
            .filter(|(_, owned)| &owned.scope == scope)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let mut registry = self.tools.lock().expect("tool registry poisoned");
        for name in names {
            registry.remove(&name);
            owners.remove(&name);
            removed.push(name);
        }
        removed.sort();
        removed
    }

    /// Registers the built-in client-bridge tools bound to `session_id`.
    ///
    /// The registry diff establishes ownership so a disposer can remove only the
    /// tools this call added.
    pub fn register_builtins(
        &self,
        plugin: &PluginId,
        session_id: &SessionId,
    ) -> Result<Vec<Disposer>, PluginError> {
        let before: std::collections::BTreeSet<String> = self.names().into_iter().collect();
        self.tools
            .lock()
            .expect("tool registry poisoned")
            .register_builtins(session_id)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        let added: Vec<String> =
            self.names().into_iter().filter(|name| !before.contains(name)).collect();
        let mut disposers = Vec::new();
        for name in added {
            self.owners
                .lock()
                .expect("tool ownership lock poisoned")
                .insert(name.clone(), OwnedTool { plugin: plugin.clone(), scope: ToolScope::Host });
            let tools = Arc::clone(&self.tools);
            let owners = Arc::clone(&self.owners);
            let plugin = plugin.clone();
            disposers.push(Disposer::new(move || {
                let mut owners = owners.lock().expect("tool ownership lock poisoned");
                if owners.get(&name).is_some_and(|owned| owned.plugin == plugin) {
                    owners.remove(&name);
                    if let Ok(mut registry) = tools.lock() {
                        registry.remove(&name);
                    }
                }
            }));
        }
        Ok(disposers)
    }

    /// Copies every registered tool into another registry, skipping duplicates.
    pub fn copy_into(&self, target: &mut ToolRegistry) -> Vec<String> {
        let mut registered = Vec::new();
        for tool in self.tools() {
            let name = tool.definition().name;
            if target.get(&name).is_some() {
                continue;
            }
            if target.register(tool).is_ok() {
                registered.push(name);
            }
        }
        registered.sort();
        registered
    }

    /// All registered tools, sorted by name for deterministic registration.
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn ServerTool>> {
        let names = self.names();
        let registry = self.tools.lock().expect("tool registry poisoned");
        names.into_iter().filter_map(|name| registry.get(&name)).collect()
    }

    /// Sorted tool definitions.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.lock().expect("tool registry poisoned").definitions()
    }

    /// Sorted tool names.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.lock().expect("tool registry poisoned").names()
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.lock().expect("tool registry poisoned").get(name).is_some()
    }

    /// Scope the tool was registered under.
    #[must_use]
    pub fn scope_of(&self, name: &str) -> Option<ToolScope> {
        self.owners
            .lock()
            .expect("tool ownership lock poisoned")
            .get(name)
            .map(|owned| owned.scope.clone())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.lock().expect("tool registry poisoned").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ToolService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ToolService").field("tools", &self.names()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;
    use crate::tools::{SideEffectClass, ToolResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::watch;

    struct TestTool {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    impl ServerTool for TestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(self.name.clone(), "test tool")
                .side_effect_class(SideEffectClass::Read)
        }

        fn execute(
            &self,
            _arguments: serde_json::Value,
            _client: ee_acp_agent_server::ClientBridge,
            _cancel: watch::Receiver<bool>,
            _context: crate::tools::ToolCallContext,
        ) -> crate::tools::ToolFuture<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { ToolResult::success("ok") })
        }
    }

    fn tool(name: &str) -> Arc<dyn ServerTool> {
        Arc::new(TestTool { name: name.to_owned(), calls: Arc::new(AtomicUsize::new(0)) })
    }

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    #[test]
    fn registers_and_removes_tools() {
        let service = ToolService::new();
        let disposer = service.register(&plugin(), ToolScope::Host, tool("demo.read")).unwrap();
        assert!(service.contains("demo.read"));
        assert_eq!(service.names(), vec!["demo.read".to_owned()]);
        disposer.dispose();
        assert!(!service.contains("demo.read"));
        assert!(service.is_empty());
    }

    #[test]
    fn rejects_duplicate_names() {
        let service = ToolService::new();
        service.register(&plugin(), ToolScope::Host, tool("demo.read")).unwrap();
        let error = service
            .register(&plugin(), ToolScope::Host, tool("demo.read"))
            .expect_err("duplicate rejected");
        assert!(matches!(error, PluginError::Runtime { .. }));
    }

    #[test]
    fn rejects_session_scope_mismatch() {
        let service = ToolService::new();
        let error = service
            .register_for_session(
                &plugin(),
                ToolScope::Session { session_id: "session-1".to_owned() },
                Some("session-2"),
                tool("demo.read"),
            )
            .expect_err("mismatched session rejected");
        assert!(matches!(error, PluginError::Runtime { .. }));
        service
            .register_for_session(
                &plugin(),
                ToolScope::Session { session_id: "session-1".to_owned() },
                Some("session-1"),
                tool("demo.read"),
            )
            .expect("matching session accepted");
    }

    #[test]
    fn turn_scope_removal_is_isolated() {
        let service = ToolService::new();
        let session = ToolScope::Session { session_id: "session-1".to_owned() };
        let turn = ToolScope::Turn { session_id: "session-1".to_owned(), turn: 1 };
        service.register(&plugin(), session.clone(), tool("demo.session")).unwrap();
        service.register(&plugin(), turn.clone(), tool("demo.turn")).unwrap();
        let removed = service.remove_scope(&turn);
        assert_eq!(removed, vec!["demo.turn".to_owned()]);
        assert!(service.contains("demo.session"));
    }

    #[test]
    fn builtin_registration_uses_the_service_path() {
        let service = ToolService::new();
        let session_id = SessionId::new("session-1");
        let disposers = service.register_builtins(&plugin(), &session_id).unwrap();
        let names = service.names();
        assert!(names.contains(&"read_file".to_owned()));
        assert!(names.contains(&"write_file".to_owned()));
        for disposer in disposers {
            disposer.dispose();
        }
        assert!(service.is_empty());
    }

    #[test]
    fn tool_cap_is_enforced() {
        let service = ToolService::new();
        let plugin = plugin();
        for index in 0..MAX_PLUGIN_TOOLS {
            service
                .register(&plugin, ToolScope::Host, tool(&format!("demo.read{index}")))
                .expect("within cap");
        }
        let error = service
            .register(&plugin, ToolScope::Host, tool("demo.over_cap"))
            .expect_err("cap enforced");
        assert!(matches!(error, PluginError::Resource { .. }));
    }
}
