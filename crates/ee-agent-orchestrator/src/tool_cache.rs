//! Turn-scoped cache of read-only tool results.
//!
//! [`ToolResultCache`] stores successful results of read-class tools keyed by
//! tool name, normalized arguments, session id, and path scope.  Write/edit
//! tools invalidate entries whose path scope overlaps the written path, so a
//! later read never serves stale content.  The cache is turn-scoped by
//! construction: callers create one cache per turn (or leave the executor
//! cache-less) and bound it with a maximum entry count that evicts the
//! oldest entries first.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::tools::{ToolDefinition, ToolResult};

/// Default maximum cached entries before oldest entries are evicted.
pub const DEFAULT_CACHE_MAX_ENTRIES: usize = 256;

/// Deterministic cache key for one tool result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCacheKey {
    /// Registered tool name.
    pub tool_name: String,
    /// Canonically serialized tool arguments.
    pub normalized_args: String,
    /// ACP session id the tool ran under.
    pub session_id: String,
    /// Path scope of the read (the file path argument, when present).
    pub scope: Option<String>,
}

/// Builds the cache key for one tool invocation.
///
/// Arguments are normalized through canonical JSON serialization (object
/// keys sorted recursively) so that equal argument objects always produce
/// equal keys regardless of map insertion order.
#[must_use]
pub fn cache_key(
    tool_name: impl Into<String>,
    arguments: &serde_json::Value,
    session_id: impl Into<String>,
    scope: Option<String>,
) -> ToolCacheKey {
    ToolCacheKey {
        tool_name: tool_name.into(),
        normalized_args: canonical_json(arguments),
        session_id: session_id.into(),
        scope,
    }
}

/// Canonical JSON serialization: object keys sorted recursively.
fn canonical_json(value: &serde_json::Value) -> String {
    serde_json::to_string(&canonical_value(value)).unwrap_or_else(|_| "{}".into())
}

fn canonical_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(String, serde_json::Value)> =
                map.iter().map(|(key, value)| (key.clone(), canonical_value(value))).collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (key, value) in pairs {
                sorted.insert(key, value);
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_value).collect())
        }
        other => other.clone(),
    }
}

/// The `path` string argument of a tool intent, when present.
pub(crate) fn path_argument(arguments: &serde_json::Value) -> Option<String> {
    arguments.get("path").and_then(serde_json::Value::as_str).map(str::to_string)
}

/// Every path scope a completed tool affects: the static dependency scope
/// plus path/cwd arguments, for write-result cache invalidation.
pub(crate) fn affected_paths(
    definition: &ToolDefinition,
    arguments: &serde_json::Value,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = &definition.dependency.affected_path {
        paths.push(path.clone());
    }
    if let Some(path) = path_argument(arguments) {
        paths.push(path);
    }
    if let Some(cwd) = arguments.get("cwd").and_then(serde_json::Value::as_str) {
        paths.push(cwd.to_string());
    }
    paths
}

/// Whether a cached scope overlaps a written path: equal, the write is a
/// parent directory of the scope, or the write targets a file under the
/// scope directory.
fn scope_overlaps(scope: &str, written: &str) -> bool {
    scope == written
        || scope.starts_with(&format!("{written}/"))
        || written.starts_with(&format!("{scope}/"))
}

/// Bounded in-memory cache of successful read-only tool results.
#[derive(Debug, Clone, Default)]
pub struct ToolResultCache {
    max_entries: usize,
    entries: HashMap<ToolCacheKey, (u64, ToolResult)>,
    order: VecDeque<ToolCacheKey>,
    sequence: u64,
}

impl ToolResultCache {
    /// Creates an empty cache with the default entry bound.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_CACHE_MAX_ENTRIES)
    }

    /// Creates an empty cache with the given entry bound; older entries are
    /// evicted first when the bound is reached.
    #[must_use]
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
            sequence: 0,
        }
    }

    /// Looks up a cached result; returns `None` on miss.
    #[must_use]
    pub fn get(&self, key: &ToolCacheKey) -> Option<ToolResult> {
        self.entries.get(key).map(|(_, result)| result.clone())
    }

    /// Stores a result under the key.  Only successful results are stored;
    /// failures are never cached.  When the entry bound is reached the
    /// oldest entry is evicted.
    pub fn insert(&mut self, key: ToolCacheKey, result: ToolResult) {
        if !result.success {
            return;
        }
        if self.entries.contains_key(&key) {
            return;
        }
        self.entries.insert(key.clone(), (self.sequence, result));
        self.sequence += 1;
        self.order.push_back(key);
        while self.entries.len() > self.max_entries
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
    }

    /// Removes every entry for the session whose path scope overlaps the
    /// written path (exact match or one path under the other).
    pub fn invalidate_path(&mut self, session_id: &str, written: &str) {
        let matches = |key: &ToolCacheKey| {
            key.session_id == session_id
                && key.scope.as_deref().is_some_and(|scope| scope_overlaps(scope, written))
        };
        let removed: Vec<ToolCacheKey> =
            self.entries.keys().filter(|key| matches(key)).cloned().collect();
        for key in removed {
            self.entries.remove(&key);
        }
        self.order.retain(|key| !self.entries.contains_key(key) || matches(key));
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every entry (end of a turn).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::{RawJsonRpcMessage, Response, SessionId, SessionUpdate};
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::budget::BudgetTracker;
    use crate::config::OrchestratorConfig;
    use crate::destructive_policy::SideEffectSubclass;
    use crate::events::EventRecorder;
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::tasks::TaskId;
    use crate::tool_dependencies::ToolDependency;
    use crate::tools::{ToolErrorKind, ToolExecutor, ToolIntent, ToolRegistry};

    fn key(name: &str, arguments: Value, session: &str, scope: Option<&str>) -> ToolCacheKey {
        cache_key(name, &arguments, session, scope.map(str::to_string))
    }

    #[test]
    fn hit_returns_cached_result() {
        let mut cache = ToolResultCache::new();
        let result = ToolResult::success("file contents");
        cache.insert(key("read_file", json!({ "path": "/a" }), "s-1", Some("/a")), result.clone());
        let cached =
            cache.get(&key("read_file", json!({ "path": "/a" }), "s-1", Some("/a"))).expect("hit");
        assert_eq!(cached, result);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn miss_returns_none() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("read_file", json!({ "path": "/a" }), "s-1", Some("/a")),
            ToolResult::success("x"),
        );
        assert!(
            cache.get(&key("read_file", json!({ "path": "/b" }), "s-1", Some("/b"))).is_none(),
            "different path misses"
        );
        assert!(
            cache.get(&key("read_file", json!({ "path": "/a" }), "s-2", Some("/a"))).is_none(),
            "different session misses"
        );
        assert!(
            cache.get(&key("write_file", json!({ "path": "/a" }), "s-1", Some("/a"))).is_none(),
            "different tool misses"
        );
    }

    #[test]
    fn only_successful_results_are_stored() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("failing", json!({}), "s-1", None),
            ToolResult::failure(ToolErrorKind::Backend, "boom"),
        );
        assert!(cache.is_empty(), "failures are never cached");
        cache.insert(key("failing", json!({}), "s-1", None), ToolResult::success("ok"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn arguments_normalize_to_the_same_key() {
        let mut first = serde_json::Map::new();
        first.insert("path".into(), json!("/a"));
        first.insert("limit".into(), json!(2));
        let mut second = serde_json::Map::new();
        second.insert("limit".into(), json!(2));
        second.insert("path".into(), json!("/a"));
        let a = key("read_file", Value::Object(first), "s-1", Some("/a"));
        let b = key("read_file", Value::Object(second), "s-1", Some("/a"));
        assert_eq!(a, b, "argument order must not change the key");
        assert_eq!(a.normalized_args, b.normalized_args);
    }

    #[test]
    fn invalidate_path_removes_exact_and_nested_scopes() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("read_file", json!({ "path": "/a/b.txt" }), "s-1", Some("/a/b.txt")),
            ToolResult::success("1"),
        );
        cache.insert(
            key("read_file", json!({ "path": "/a/c.txt" }), "s-1", Some("/a/c.txt")),
            ToolResult::success("2"),
        );
        cache.insert(
            key("read_file", json!({ "path": "/other.txt" }), "s-1", Some("/other.txt")),
            ToolResult::success("3"),
        );
        cache.insert(
            key("read_file", json!({ "path": "/a/b.txt" }), "s-2", Some("/a/b.txt")),
            ToolResult::success("4"),
        );

        cache.invalidate_path("s-1", "/a/b.txt");
        assert!(
            cache
                .get(&key("read_file", json!({ "path": "/a/b.txt" }), "s-1", Some("/a/b.txt")))
                .is_none(),
            "exact scope is invalidated"
        );
        assert!(
            cache
                .get(&key("read_file", json!({ "path": "/a/c.txt" }), "s-1", Some("/a/c.txt")))
                .is_some(),
            "sibling scopes survive"
        );
        assert!(
            cache
                .get(&key("read_file", json!({ "path": "/a/b.txt" }), "s-2", Some("/a/b.txt")))
                .is_some(),
            "other sessions survive"
        );
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn invalidate_path_covers_directory_prefixes() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("read_file", json!({ "path": "/a/b.txt" }), "s-1", Some("/a/b.txt")),
            ToolResult::success("1"),
        );
        cache.invalidate_path("s-1", "/a");
        assert!(cache.is_empty(), "write under /a invalidates /a/b.txt");
    }

    #[test]
    fn invalidate_path_clears_scopes_under_a_written_directory() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("read_file", json!({ "path": "/a/b.txt" }), "s-1", Some("/a")),
            ToolResult::success("1"),
        );
        cache.invalidate_path("s-1", "/a/b.txt");
        assert!(cache.is_empty(), "write of /a/b.txt invalidates scope /a");
    }

    #[test]
    fn max_entries_evicts_oldest_first() {
        let mut cache = ToolResultCache::with_max_entries(2);
        cache.insert(
            key("read_file", json!({ "path": "/1" }), "s-1", Some("/1")),
            ToolResult::success("1"),
        );
        cache.insert(
            key("read_file", json!({ "path": "/2" }), "s-1", Some("/2")),
            ToolResult::success("2"),
        );
        cache.insert(
            key("read_file", json!({ "path": "/3" }), "s-1", Some("/3")),
            ToolResult::success("3"),
        );
        assert_eq!(cache.len(), 2);
        assert!(
            cache.get(&key("read_file", json!({ "path": "/1" }), "s-1", Some("/1"))).is_none(),
            "oldest entry is evicted"
        );
        assert!(
            cache.get(&key("read_file", json!({ "path": "/3" }), "s-1", Some("/3"))).is_some(),
            "newest entry survives"
        );
    }

    #[test]
    fn clear_drops_every_entry() {
        let mut cache = ToolResultCache::new();
        cache.insert(
            key("read_file", json!({ "path": "/a" }), "s-1", Some("/a")),
            ToolResult::success("1"),
        );
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn affected_paths_include_definition_scope_and_arguments() {
        let definition = ToolDefinition::new("write_file", "writes")
            .dependency(ToolDependency::default().affected_path("/static"));
        let arguments = json!({ "path": "/x", "cwd": "/work" });
        let paths = affected_paths(&definition, &arguments);
        assert_eq!(paths, vec!["/static", "/x", "/work"]);
    }

    // ── Executor integration ─────────────────────────────────────────────

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn task_fixture() -> crate::tasks::TaskNode {
        crate::tasks::TaskNode::new(TaskId::new("task-1"), "t", "d")
    }

    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    /// Drains one tool lifecycle (pending → in-progress → completed/failed),
    /// responding to any interleaved client request with `result`, and
    /// returning whether the bridge was hit.
    async fn drain_lifecycle(
        rx: &mut mpsc::UnboundedReceiver<OutboundEvent>,
        bridge: &ClientBridge,
        result: Value,
    ) -> bool {
        assert!(matches!(next_update(rx).await, SessionUpdate::ToolCall(_)));
        assert!(matches!(next_update(rx).await, SessionUpdate::ToolCallUpdate(_)));
        let mut hit_bridge = false;
        loop {
            match rx.recv().await.expect("lifecycle completes") {
                OutboundEvent::ClientRequest { frame } => {
                    let RawJsonRpcMessage::Request(request) = frame else {
                        panic!("expected request frame");
                    };
                    bridge.handle_response(Response::Result {
                        id: request.id,
                        result: result.clone(),
                    });
                    hit_bridge = true;
                }
                OutboundEvent::Update { update, .. }
                    if matches!(*update, SessionUpdate::ToolCallUpdate(_)) =>
                {
                    return hit_bridge;
                }
                OutboundEvent::Update { update, .. } => panic!("unexpected update: {update:?}"),
                other => panic!("unexpected outbound event: {other:?}"),
            }
        }
    }

    /// Spawns one intent execution and drains its lifecycle with responses.
    async fn run_intent(
        executor: &ToolExecutor,
        sink: &UpdateSink,
        bridge: &ClientBridge,
        rx: &mut mpsc::UnboundedReceiver<OutboundEvent>,
        intent: &ToolIntent,
        result: Value,
    ) -> ToolResult {
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let task_executor = executor.clone();
        let task_sink = sink.clone();
        let task_bridge = bridge.clone();
        let task_intent = intent.clone();
        let task = tokio::spawn(async move {
            task_executor
                .execute(&task_intent, &task_sink, &task_bridge, cancel_rx, &task_fixture(), &[])
                .await
        });
        drain_lifecycle(rx, bridge, result).await;
        task.await.expect("task joins").expect("execution succeeds")
    }

    fn read_intent(id: &str) -> ToolIntent {
        ToolIntent::new(id, "read_file", json!({ "path": "/a.txt" }))
    }

    fn read_result() -> Value {
        json!({ "content": "/a.txt contents" })
    }

    fn executor_with_cache(
        cache: Arc<Mutex<ToolResultCache>>,
        policy: PolicyEngine,
    ) -> (ToolExecutor, Arc<Mutex<ToolRegistry>>) {
        let config = OrchestratorConfig::default();
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let executor = ToolExecutor::new(
            config,
            tools.clone(),
            Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default()))),
            policy,
            0,
            EventRecorder::new(),
        )
        .with_cache(cache);
        (executor, tools)
    }

    #[tokio::test]
    async fn read_cache_hit_skips_a_second_client_request() {
        let (sink, bridge, mut rx) = plumbing();
        let cache = Arc::new(Mutex::new(ToolResultCache::new()));
        let (executor, tools) = executor_with_cache(cache.clone(), PolicyEngine::default());
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let first =
            run_intent(&executor, &sink, &bridge, &mut rx, &read_intent("tc-1"), read_result())
                .await;
        assert_eq!(first.text_output, "/a.txt contents");

        // Second identical read: cached, so the bridge must not see it.
        let second =
            run_intent(&executor, &sink, &bridge, &mut rx, &read_intent("tc-2"), read_result())
                .await;
        assert_eq!(second, first);
        assert!(
            rx.try_recv().is_err(),
            "cache hit emits no client request and leaves no stray events"
        );
        assert_eq!(cache.lock().expect("cache").len(), 1);
    }

    #[tokio::test]
    async fn write_invalidation_clears_scoped_reads() {
        let (sink, bridge, mut rx) = plumbing();
        let cache = Arc::new(Mutex::new(ToolResultCache::new()));
        let policy = PolicyEngine::new(
            ToolPolicy { allow_write: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::Overwrite),
        );
        let (executor, tools) = executor_with_cache(cache.clone(), policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        run_intent(&executor, &sink, &bridge, &mut rx, &read_intent("tc-1"), read_result()).await;
        run_intent(&executor, &sink, &bridge, &mut rx, &read_intent("tc-2"), read_result()).await;
        assert_eq!(cache.lock().expect("cache").len(), 1, "second read is a cache hit");

        // Write the same path: the cached read is invalidated.
        let write =
            ToolIntent::new("tc-3", "write_file", json!({ "path": "/a.txt", "content": "new" }));
        let result = run_intent(&executor, &sink, &bridge, &mut rx, &write, json!({})).await;
        assert!(result.success);
        assert_eq!(cache.lock().expect("cache").len(), 0, "write invalidates the read");

        // The next read misses and hits the bridge again.
        run_intent(&executor, &sink, &bridge, &mut rx, &read_intent("tc-4"), read_result()).await;
        assert_eq!(cache.lock().expect("cache").len(), 1, "read after write reaches the bridge");
    }

    #[tokio::test]
    async fn write_and_execute_results_are_not_cached() {
        let (sink, bridge, mut rx) = plumbing();
        let cache = Arc::new(Mutex::new(ToolResultCache::new()));
        let policy = PolicyEngine::new(
            ToolPolicy { allow_write: true, allow_execute: true, ..ToolPolicy::default() }
                .allow_side_effect_subclass(SideEffectSubclass::Overwrite),
        );
        let (executor, tools) = executor_with_cache(cache.clone(), policy);
        tools
            .lock()
            .expect("registry")
            .register_builtins(&SessionId::new("s-1"))
            .expect("builtins");

        let write = ToolIntent::new("tc-1", "write_file", json!({ "path": "/a", "content": "x" }));
        let result = run_intent(&executor, &sink, &bridge, &mut rx, &write, json!({})).await;
        assert!(result.success);
        assert!(cache.lock().expect("cache").is_empty(), "write results are not cached");

        let terminal = ToolIntent::new("tc-2", "create_terminal", json!({ "command": "ls" }));
        let result = run_intent(
            &executor,
            &sink,
            &bridge,
            &mut rx,
            &terminal,
            json!({ "terminalId": "term-1" }),
        )
        .await;
        assert!(result.success);
        assert!(cache.lock().expect("cache").is_empty(), "execute results are not cached");
    }
}
