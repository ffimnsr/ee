//! Optional semantic memory adapter.
//!
//! [`SemanticMemory`] wraps an external vector/index lookup behind the
//! [`SemanticMemoryAdapter`] trait, so embedding backends are never a required
//! dependency.  Hits are merged into a [`ContextPack`] as untrusted memory
//! items with semantic provenance; secret-like keys are skipped, values are
//! redacted and truncated, and the pack is re-trimmed to its byte budget.
//! Without an adapter (or with the feature disabled) merging is a no-op that
//! leaves the pack untouched.

use serde::{Deserialize, Serialize};

use crate::context_pack::ContextPack;
use crate::error::OrchestratorError;

/// Default hit limit for one adapter search.
pub const DEFAULT_SEMANTIC_LIMIT: usize = 8;
/// Default cap on hits merged into one context pack.
pub const DEFAULT_MAX_SEMANTIC_HITS: usize = 8;
/// Cap on one semantic hit's value characters inside a pack.
pub const SEMANTIC_VALUE_MAX_CHARS: usize = 500;

/// One external index hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SemanticMemoryHit {
    /// Stable lookup key.
    pub key: String,
    /// The hit value.
    pub value: String,
    /// Similarity score reported by the index (diagnostic only).
    pub score: f32,
    /// External document id for provenance.
    pub source_id: String,
}

impl SemanticMemoryHit {
    /// Creates a hit.
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
        source_id: impl Into<String>,
    ) -> Self {
        Self { key: key.into(), value: value.into(), score: 0.0, source_id: source_id.into() }
    }

    /// Attaches a similarity score.
    #[must_use]
    pub fn with_score(mut self, score: f32) -> Self {
        self.score = score;
        self
    }
}

/// External vector/index lookup backend.
///
/// Implementations must be `Send + Sync + 'static` and return deterministic,
/// bounded results.  Errors are logged as diagnostics and fail closed: the
/// caller sees no hits rather than partial results.
pub trait SemanticMemoryAdapter: Send + Sync + 'static {
    /// Searches the index; `limit` caps the returned hits.
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SemanticMemoryHit>, OrchestratorError>;
}

/// Semantic memory knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticMemoryConfig {
    /// Whether semantic lookup is active at all; `false` forces the no-op
    /// path even when an adapter is installed.
    pub enabled: bool,
    /// Hit limit passed to the adapter on search.
    pub default_limit: usize,
    /// Cap on hits merged into one context pack.
    pub max_hits: usize,
    /// Cap on one merged value's characters.
    pub max_value_chars: usize,
}

impl Default for SemanticMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_limit: DEFAULT_SEMANTIC_LIMIT,
            max_hits: DEFAULT_MAX_SEMANTIC_HITS,
            max_value_chars: SEMANTIC_VALUE_MAX_CHARS,
        }
    }
}

/// Optional semantic memory frontend.
///
/// Construct with `Some(adapter)` to enable lookup, or `None` to keep the
/// feature inert.  [`SemanticMemory::merge_into`] appends hits to a
/// [`ContextPack`] with semantic provenance and re-trims the pack to its
/// budget.
#[derive(Clone)]
pub struct SemanticMemory {
    adapter: Option<std::sync::Arc<dyn SemanticMemoryAdapter>>,
    config: SemanticMemoryConfig,
}

impl SemanticMemory {
    /// Creates the frontend; `adapter: None` disables lookup.
    #[must_use]
    pub fn new(
        adapter: Option<Box<dyn SemanticMemoryAdapter>>,
        config: SemanticMemoryConfig,
    ) -> Self {
        Self { adapter: adapter.map(std::sync::Arc::from), config }
    }

    /// Whether semantic lookup is active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && self.adapter.is_some()
    }

    /// Searches the index, bounded by the configured limit.
    ///
    /// Disabled frontends and adapter errors return an empty vec (fail
    /// closed); errors are logged as diagnostics only.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<SemanticMemoryHit> {
        let Some(adapter) = &self.adapter else {
            return Vec::new();
        };
        if !self.config.enabled {
            return Vec::new();
        }
        match adapter.search(query, self.config.default_limit) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::debug!(%error, "semantic memory search failed; excluding hits");
                Vec::new()
            }
        }
    }

    /// Merges semantic hits into the pack with provenance.
    ///
    /// Returns the number of hits merged (0 when disabled, empty, or all
    /// rejected).  The pack is re-trimmed to its byte budget afterwards.
    #[must_use]
    pub fn merge_into(&self, pack: &mut ContextPack, query: &str) -> usize {
        if !self.is_enabled() {
            return 0;
        }
        let hits = match &self.adapter {
            Some(adapter) => match adapter.search(query, self.config.default_limit) {
                Ok(hits) => hits,
                Err(error) => {
                    tracing::debug!(%error, "semantic memory merge failed; excluding hits");
                    return 0;
                }
            },
            None => return 0,
        };
        pack.merge_semantic_hits(hits, self.config.max_hits, self.config.max_value_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_pack::{ContextPackBuilder, ContextPackConfig};
    use crate::memory::MemoryItem;
    use crate::memory::MemoryStore;

    /// Deterministic fake adapter for tests.
    #[derive(Debug, Clone)]
    struct FakeAdapter {
        hits: Vec<SemanticMemoryHit>,
        fail: bool,
    }

    impl FakeAdapter {
        fn new(hits: Vec<SemanticMemoryHit>) -> Self {
            Self { hits, fail: false }
        }
    }

    impl SemanticMemoryAdapter for FakeAdapter {
        fn search(
            &self,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<SemanticMemoryHit>, OrchestratorError> {
            if self.fail {
                return Err(OrchestratorError::ModelFailure("index down".into()));
            }
            let mut hits = self.hits.clone();
            hits.truncate(limit);
            Ok(hits)
        }
    }

    fn empty_pack() -> ContextPack {
        ContextPackBuilder::new(ContextPackConfig::default()).build()
    }

    fn hit(key: &str, value: impl Into<String>, source: &str) -> SemanticMemoryHit {
        SemanticMemoryHit::new(key, value, source).with_score(0.9)
    }

    #[test]
    fn disabled_without_adapter_is_a_noop() {
        let semantic = SemanticMemory::new(None, SemanticMemoryConfig::default());
        assert!(!semantic.is_enabled());
        assert!(semantic.search("anything").is_empty());
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "anything"), 0);
        assert!(pack.memory_items.is_empty(), "disabled merge leaves pack untouched");
    }

    #[test]
    fn disabled_by_config_ignores_installed_adapter() {
        let semantic = SemanticMemory::new(
            Some(Box::new(FakeAdapter::new(vec![hit("k", "v", "doc-1")]))),
            SemanticMemoryConfig { enabled: false, ..SemanticMemoryConfig::default() },
        );
        assert!(!semantic.is_enabled());
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "k"), 0);
        assert!(pack.memory_items.is_empty());
    }

    #[test]
    fn fake_adapter_hits_merge_with_provenance() {
        let semantic = SemanticMemory::new(
            Some(Box::new(FakeAdapter::new(vec![
                hit("rust:modules", "modules map to crates", "doc-1"),
                hit("rust:traits", "traits are interfaces", "doc-2"),
            ]))),
            SemanticMemoryConfig::default(),
        );
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "rust"), 2);
        assert_eq!(pack.memory_items.len(), 2);
        let item = &pack.memory_items[0];
        assert_eq!(item.key, "rust:modules");
        assert_eq!(
            item.provenance.source_kind,
            crate::context_pack::ProvenanceSourceKind::Semantic
        );
        assert_eq!(item.provenance.source_id, "doc-1");
        assert!(item.provenance.trust.is_untrusted(), "external hits are untrusted");
        let rendered = pack.render();
        assert!(rendered.contains("[untrusted tool_output] rust:modules: modules map to crates"));
    }

    #[test]
    fn secret_like_hits_are_rejected_and_values_redacted() {
        let semantic = SemanticMemory::new(
            Some(Box::new(FakeAdapter::new(vec![
                hit("api_token", "sk-live-1234567890", "doc-1"),
                hit("note", "key is sk-live-1234567890", "doc-2"),
            ]))),
            SemanticMemoryConfig::default(),
        );
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "note"), 1);
        assert_eq!(pack.memory_items.len(), 1, "secret-keyed hit skipped");
        let value = &pack.memory_items[0].value;
        assert!(!value.contains("sk-live-1234567890"), "value redacted");
        assert!(value.contains("[redacted]"));
    }

    #[test]
    fn adapter_errors_fail_closed() {
        let mut adapter = FakeAdapter::new(vec![hit("k", "v", "doc-1")]);
        adapter.fail = true;
        let semantic =
            SemanticMemory::new(Some(Box::new(adapter)), SemanticMemoryConfig::default());
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "k"), 0);
        assert!(pack.memory_items.is_empty(), "error yields no hits");
    }

    #[test]
    fn max_hits_and_value_chars_are_bounded() {
        let semantic = SemanticMemory::new(
            Some(Box::new(FakeAdapter::new(vec![
                hit("a", "x".repeat(5_000), "doc-1"),
                hit("b", "short", "doc-2"),
                hit("c", "third", "doc-3"),
            ]))),
            SemanticMemoryConfig {
                max_hits: 2,
                max_value_chars: 100,
                ..SemanticMemoryConfig::default()
            },
        );
        let mut pack = empty_pack();
        assert_eq!(semantic.merge_into(&mut pack, "a"), 2);
        assert_eq!(pack.memory_items.len(), 2);
        assert!(
            pack.memory_items[0].value.len() <= SEMANTIC_VALUE_MAX_CHARS + 1,
            "value truncated with ellipsis"
        );
        let mut store = MemoryStore::new(4_096);
        store.insert(MemoryItem::new("k", "v")).expect("inserts");
        assert!(pack.total_bytes() <= pack.truncation.max_bytes, "budget holds after merge");
    }

    #[test]
    fn hit_roundtrips_through_json() {
        let hit = hit("key", "value", "doc-1");
        let json = serde_json::to_string(&hit).expect("serializes");
        let restored: SemanticMemoryHit = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, hit);
        assert_eq!(restored.score, 0.9);
    }
}
