//! Optional semantic memory adapter.
//!
//! [`SemanticMemory`] wraps an external vector/index lookup behind the
//! [`SemanticMemoryAdapter`] trait, so embedding backends are never a required
//! dependency.  Hits are merged into a [`ContextPack`] as untrusted memory
//! items with semantic provenance; secret-like keys are skipped, values are
//! redacted and truncated, and the pack is re-trimmed to its byte budget.
//! Without an adapter (or with the feature disabled) merging is a no-op that
//! leaves the pack untouched.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::context_pack::{
    ContextPack, WorkspaceContextFact, WorkspaceFactAuthority, WorkspaceFactFreshness,
    WorkspaceFactSelectionReason, WorkspaceFactState,
};
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
    adapter: Option<Arc<dyn SemanticMemoryAdapter>>,
    config: SemanticMemoryConfig,
}

impl SemanticMemory {
    /// Creates the frontend; `adapter: None` disables lookup.
    #[must_use]
    pub fn new(
        adapter: Option<Box<dyn SemanticMemoryAdapter>>,
        config: SemanticMemoryConfig,
    ) -> Self {
        Self { adapter: adapter.map(Arc::from), config }
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

/// Projection schema consumed by workspace semantic sidecars.
pub const WORKSPACE_SEMANTIC_PROJECTION_SCHEMA_VERSION: u32 = 1;
/// Maximum normalized similarity value. Similarity is diagnostic, never authority.
pub const MAX_WORKSPACE_SEMANTIC_SIMILARITY: u32 = 1_000_000;
/// Default deterministic cap for workspace semantic expansion.
pub const DEFAULT_WORKSPACE_SEMANTIC_HIT_CAP: usize = 8;

/// Opaque digest identifying one authoritative workspace fact snapshot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceDigestId(String);

impl WorkspaceDigestId {
    /// Wraps an identity generated by the authoritative workspace-memory owner.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns opaque identity bytes as text without interpreting them.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque identity for one canonical ordered workspace root set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceRootSetId(String);

impl WorkspaceRootSetId {
    /// Wraps an identity generated by the canonical workspace owner.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns opaque identity bytes as text without interpreting them.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Deterministic workspace semantic filters.
///
/// Empty namespace, kind, or authority lists mean "all". Freshness and state
/// defaults admit only current active facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticMemoryFilters {
    /// Allowed namespaces, compared exactly.
    pub namespaces: Vec<String>,
    /// Allowed opaque fact-kind labels, compared exactly.
    pub fact_kinds: Vec<String>,
    /// Allowed authority metadata. Similarity cannot modify this field.
    pub authorities: Vec<WorkspaceFactAuthority>,
    /// Allowed freshness metadata.
    pub freshness: Vec<WorkspaceFactFreshness>,
    /// Allowed lifecycle states.
    pub states: Vec<WorkspaceFactState>,
}

impl Default for WorkspaceSemanticMemoryFilters {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            fact_kinds: Vec::new(),
            authorities: Vec::new(),
            freshness: vec![WorkspaceFactFreshness::Current],
            states: vec![WorkspaceFactState::Active],
        }
    }
}

impl WorkspaceSemanticMemoryFilters {
    fn admits(&self, projection: &WorkspaceSemanticFactProjection) -> bool {
        (self.namespaces.is_empty() || self.namespaces.contains(&projection.namespace))
            && (self.fact_kinds.is_empty() || self.fact_kinds.contains(&projection.fact_kind))
            && (self.authorities.is_empty()
                || self.authorities.contains(&projection.fact.authority))
            && (self.freshness.is_empty() || self.freshness.contains(&projection.fact.freshness))
            && (self.states.is_empty() || self.states.contains(&projection.fact.state))
    }
}

/// Structured, workspace-scoped semantic query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticMemoryQuery {
    /// Digest of authoritative facts used to build expected sidecar contents.
    pub workspace_digest: WorkspaceDigestId,
    /// Canonical root-set identity restricting lookup scope.
    pub root_set: WorkspaceRootSetId,
    /// Local semantic model/index implementation version.
    pub model_version: String,
    /// User/task query. Adapter must not treat this as authority.
    pub query: String,
    /// Deterministic total result cap, including caller-supplied authoritative hits.
    pub cap: usize,
    /// Metadata filters applied by adapter and checked again by frontend.
    pub filters: WorkspaceSemanticMemoryFilters,
}

/// Authoritative fact projection stored in an optional semantic sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticFactProjection {
    /// Stable fact namespace.
    pub namespace: String,
    /// Opaque stable fact-kind label.
    pub fact_kind: String,
    /// Complete context-compatible fact metadata and bounded value.
    pub fact: WorkspaceContextFact,
}

impl WorkspaceSemanticFactProjection {
    /// Creates a sidecar projection from an authoritative context fact.
    #[must_use]
    pub fn new(
        namespace: impl Into<String>,
        fact_kind: impl Into<String>,
        fact: WorkspaceContextFact,
    ) -> Self {
        Self { namespace: namespace.into(), fact_kind: fact_kind.into(), fact }
    }

    fn into_context_fact(mut self) -> WorkspaceContextFact {
        self.fact.selection_reason = WorkspaceFactSelectionReason::Semantic;
        self.fact
    }
}

/// One sidecar hit. Integer similarity avoids platform-sensitive float ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticMemoryHit {
    /// Rich authoritative projection copied into sidecar.
    pub projection: WorkspaceSemanticFactProjection,
    /// Normalized similarity in `0..=1_000_000`; diagnostic/ranking only.
    pub similarity: u32,
}

impl WorkspaceSemanticMemoryHit {
    /// Creates a semantic hit without changing fact authority metadata.
    #[must_use]
    pub fn new(projection: WorkspaceSemanticFactProjection, similarity: u32) -> Self {
        Self { projection, similarity }
    }
}

/// Why sidecar must be rebuilt from authoritative projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSemanticRebuildReason {
    /// Sidecar files were lost.
    Loss,
    /// Sidecar failed integrity checks.
    Corruption,
    /// Projection schema changed.
    Migration,
    /// Local semantic model/index version changed.
    ModelChange,
    /// Explicit host maintenance request.
    Manual,
}

/// Staleness reason reported by sidecar backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSemanticSidecarStaleness {
    /// Authoritative workspace digest differs.
    WorkspaceDigest,
    /// Canonical workspace root set differs.
    RootSet,
    /// Projection schema differs.
    ProjectionSchema,
    /// Semantic model/index version differs.
    ModelVersion,
    /// Backend detected loss or corruption.
    Backend,
}

/// Stable identity of one rebuildable local sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticSidecarIdentity {
    /// Opaque backend-generated sidecar identity.
    pub sidecar_id: String,
    /// Authoritative digest used for rebuild.
    pub workspace_digest: WorkspaceDigestId,
    /// Canonical root-set identity used for rebuild.
    pub root_set: WorkspaceRootSetId,
    /// Projection schema used for rebuild.
    pub projection_schema_version: u32,
}

/// Sidecar identity, model version, and freshness metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticSidecarMetadata {
    /// Stable sidecar identity.
    pub identity: WorkspaceSemanticSidecarIdentity,
    /// Local semantic model/index implementation version.
    pub model_version: String,
    /// Explicit backend staleness marker.
    pub stale: bool,
    /// Optional typed staleness cause.
    pub stale_reason: Option<WorkspaceSemanticSidecarStaleness>,
}

impl WorkspaceSemanticSidecarMetadata {
    fn is_current(&self) -> bool {
        !self.stale
            && self.stale_reason.is_none()
            && self.identity.projection_schema_version
                == WORKSPACE_SEMANTIC_PROJECTION_SCHEMA_VERSION
    }

    fn matches(&self, query: &WorkspaceSemanticMemoryQuery) -> bool {
        self.is_current()
            && self.identity.workspace_digest == query.workspace_digest
            && self.identity.root_set == query.root_set
            && self.model_version == query.model_version
    }

    fn matches_rebuild(&self, request: &WorkspaceSemanticRebuildRequest<'_>) -> bool {
        self.is_current()
            && self.identity.workspace_digest == request.workspace_digest
            && self.identity.root_set == request.root_set
            && self.model_version == request.model_version
    }
}

/// Bounded adapter search response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticMemorySearchResult {
    /// Sidecar metadata validated before any hit is accepted.
    pub sidecar: WorkspaceSemanticSidecarMetadata,
    /// Candidate hits. Frontend deterministically sorts, filters, deduplicates, and caps them.
    pub hits: Vec<WorkspaceSemanticMemoryHit>,
}

/// Complete authoritative input for rebuilding one semantic sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSemanticRebuildRequest<'a> {
    /// Digest represented by complete projections.
    pub workspace_digest: WorkspaceDigestId,
    /// Canonical root set represented by complete projections.
    pub root_set: WorkspaceRootSetId,
    /// Target local model/index version.
    pub model_version: String,
    /// Rebuild trigger.
    pub reason: WorkspaceSemanticRebuildReason,
    /// Complete bounded projections from authoritative storage.
    pub projections: &'a [WorkspaceSemanticFactProjection],
}

/// Optional local workspace semantic retrieval and rebuild backend.
///
/// Implementations may use local in-process indexing, but this crate adds no
/// network, embedding, or vector database dependency. Authoritative storage
/// must be able to discard and rebuild all adapter state.
pub trait WorkspaceSemanticMemoryAdapter: Send + Sync + 'static {
    /// Searches one exact workspace/root-set sidecar.
    fn search(
        &self,
        query: &WorkspaceSemanticMemoryQuery,
    ) -> Result<WorkspaceSemanticMemorySearchResult, OrchestratorError>;

    /// Replaces sidecar contents from complete authoritative fact projections.
    fn rebuild(
        &self,
        request: &WorkspaceSemanticRebuildRequest<'_>,
    ) -> Result<WorkspaceSemanticSidecarMetadata, OrchestratorError>;
}

/// Disabled-by-default workspace semantic retrieval knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSemanticMemoryConfig {
    /// Explicit runtime opt-in.
    pub enabled: bool,
    /// Hard upper bound applied to caller's deterministic cap.
    pub max_hits: usize,
}

impl Default for WorkspaceSemanticMemoryConfig {
    fn default() -> Self {
        Self { enabled: false, max_hits: DEFAULT_WORKSPACE_SEMANTIC_HIT_CAP }
    }
}

/// Optional semantic expansion over caller-supplied authoritative exact/FTS results.
#[derive(Clone)]
pub struct WorkspaceSemanticMemory {
    adapter: Option<Arc<dyn WorkspaceSemanticMemoryAdapter>>,
    config: WorkspaceSemanticMemoryConfig,
}

impl WorkspaceSemanticMemory {
    /// Creates disabled/optional frontend. Both config opt-in and adapter are required.
    #[must_use]
    pub fn new(
        adapter: Option<Box<dyn WorkspaceSemanticMemoryAdapter>>,
        config: WorkspaceSemanticMemoryConfig,
    ) -> Self {
        Self { adapter: adapter.map(Arc::from), config }
    }

    /// Whether semantic expansion and rebuild hooks are active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && self.adapter.is_some()
    }

    /// Adds healthy semantic candidates after authoritative exact/FTS results.
    ///
    /// Backend failure, stale/mismatched sidecar metadata, and malformed hits
    /// all fail closed to deterministic authoritative results supplied by caller.
    #[must_use]
    pub fn retrieve(
        &self,
        query: &WorkspaceSemanticMemoryQuery,
        mut authoritative: Vec<WorkspaceContextFact>,
    ) -> Vec<WorkspaceContextFact> {
        let cap = query.cap.min(self.config.max_hits);
        authoritative.truncate(cap);
        if authoritative.len() == cap || !self.is_enabled() {
            return authoritative;
        }
        let Some(adapter) = &self.adapter else {
            return authoritative;
        };
        let mut bounded_query = query.clone();
        bounded_query.cap = cap;
        let result = match adapter.search(&bounded_query) {
            Ok(result) if result.sidecar.matches(&bounded_query) => result,
            Ok(_) => return authoritative,
            Err(error) => {
                tracing::debug!(%error, "workspace semantic search failed; using authoritative results");
                return authoritative;
            }
        };

        if result.hits.len() > cap
            || result.hits.iter().any(|hit| hit.similarity > MAX_WORKSPACE_SEMANTIC_SIMILARITY)
        {
            return authoritative;
        }
        let mut seen = authoritative.iter().map(|fact| fact.key.clone()).collect::<BTreeSet<_>>();
        let mut hits = result.hits;
        hits.retain(|hit| bounded_query.filters.admits(&hit.projection));
        hits.sort_by(|left, right| {
            right
                .similarity
                .cmp(&left.similarity)
                .then_with(|| left.projection.fact.key.cmp(&right.projection.fact.key))
                .then_with(|| left.projection.fact.source_id.cmp(&right.projection.fact.source_id))
                .then_with(|| left.projection.namespace.cmp(&right.projection.namespace))
                .then_with(|| left.projection.fact_kind.cmp(&right.projection.fact_kind))
        });
        for hit in hits {
            if authoritative.len() == cap {
                break;
            }
            if seen.insert(hit.projection.fact.key.clone()) {
                authoritative.push(hit.projection.into_context_fact());
            }
        }
        authoritative
    }

    /// Invokes sidecar replacement from authoritative projections.
    ///
    /// Disabled frontend returns `Ok(None)` without touching backend.
    pub fn rebuild(
        &self,
        request: &WorkspaceSemanticRebuildRequest<'_>,
    ) -> Result<Option<WorkspaceSemanticSidecarMetadata>, OrchestratorError> {
        if !self.is_enabled() {
            return Ok(None);
        }
        let Some(adapter) = &self.adapter else {
            return Ok(None);
        };
        let metadata = adapter.rebuild(request)?;
        if !metadata.matches_rebuild(request) {
            return Err(OrchestratorError::InvalidState(
                "workspace semantic rebuild returned stale or mismatched metadata".into(),
            ));
        }
        Ok(Some(metadata))
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

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct WorkspaceFakeState {
        search_calls: AtomicUsize,
        rebuild_calls: AtomicUsize,
        searched_cap: AtomicUsize,
        rebuilt: Mutex<Vec<WorkspaceSemanticFactProjection>>,
    }

    impl Default for WorkspaceFakeState {
        fn default() -> Self {
            Self {
                search_calls: AtomicUsize::new(0),
                rebuild_calls: AtomicUsize::new(0),
                searched_cap: AtomicUsize::new(0),
                rebuilt: Mutex::new(Vec::new()),
            }
        }
    }

    struct WorkspaceFakeAdapter {
        state: Arc<WorkspaceFakeState>,
        result: Result<WorkspaceSemanticMemorySearchResult, OrchestratorError>,
    }

    impl WorkspaceSemanticMemoryAdapter for WorkspaceFakeAdapter {
        fn search(
            &self,
            query: &WorkspaceSemanticMemoryQuery,
        ) -> Result<WorkspaceSemanticMemorySearchResult, OrchestratorError> {
            self.state.search_calls.fetch_add(1, Ordering::SeqCst);
            self.state.searched_cap.store(query.cap, Ordering::SeqCst);
            self.result.clone()
        }

        fn rebuild(
            &self,
            request: &WorkspaceSemanticRebuildRequest<'_>,
        ) -> Result<WorkspaceSemanticSidecarMetadata, OrchestratorError> {
            self.state.rebuild_calls.fetch_add(1, Ordering::SeqCst);
            *self.state.rebuilt.lock().expect("rebuild capture lock") =
                request.projections.to_vec();
            Ok(sidecar_metadata_for(
                &request.workspace_digest,
                &request.root_set,
                &request.model_version,
                false,
            ))
        }
    }

    fn workspace_query(cap: usize) -> WorkspaceSemanticMemoryQuery {
        WorkspaceSemanticMemoryQuery {
            workspace_digest: WorkspaceDigestId::new("digest-1"),
            root_set: WorkspaceRootSetId::new("roots-1"),
            model_version: "local-model-v1".into(),
            query: "routing policy".into(),
            cap,
            filters: WorkspaceSemanticMemoryFilters::default(),
        }
    }

    fn workspace_fact(key: &str, source_id: &str) -> WorkspaceContextFact {
        WorkspaceContextFact::new(
            key,
            format!("value for {key}"),
            WorkspaceFactAuthority::HostVerified,
            WorkspaceFactFreshness::Current,
            WorkspaceFactState::Active,
            source_id,
            WorkspaceFactSelectionReason::FullText,
        )
        .with_source_file("src/lib.rs", Some((3, 5)))
    }

    fn workspace_projection(key: &str, source_id: &str) -> WorkspaceSemanticFactProjection {
        WorkspaceSemanticFactProjection::new(
            "architecture",
            "constraint",
            workspace_fact(key, source_id),
        )
    }

    fn sidecar_metadata_for(
        digest: &WorkspaceDigestId,
        root_set: &WorkspaceRootSetId,
        model_version: &str,
        stale: bool,
    ) -> WorkspaceSemanticSidecarMetadata {
        WorkspaceSemanticSidecarMetadata {
            identity: WorkspaceSemanticSidecarIdentity {
                sidecar_id: "sidecar-1".into(),
                workspace_digest: digest.clone(),
                root_set: root_set.clone(),
                projection_schema_version: WORKSPACE_SEMANTIC_PROJECTION_SCHEMA_VERSION,
            },
            model_version: model_version.into(),
            stale,
            stale_reason: stale.then_some(WorkspaceSemanticSidecarStaleness::Backend),
        }
    }

    fn workspace_result(
        query: &WorkspaceSemanticMemoryQuery,
        hits: Vec<WorkspaceSemanticMemoryHit>,
        stale: bool,
    ) -> WorkspaceSemanticMemorySearchResult {
        WorkspaceSemanticMemorySearchResult {
            sidecar: sidecar_metadata_for(
                &query.workspace_digest,
                &query.root_set,
                &query.model_version,
                stale,
            ),
            hits,
        }
    }

    fn workspace_semantic(
        state: Arc<WorkspaceFakeState>,
        result: Result<WorkspaceSemanticMemorySearchResult, OrchestratorError>,
        enabled: bool,
    ) -> WorkspaceSemanticMemory {
        WorkspaceSemanticMemory::new(
            Some(Box::new(WorkspaceFakeAdapter { state, result })),
            WorkspaceSemanticMemoryConfig { enabled, max_hits: 8 },
        )
    }

    #[test]
    fn workspace_adapter_is_disabled_by_default() {
        let query = workspace_query(8);
        let state = Arc::new(WorkspaceFakeState::default());
        let semantic = WorkspaceSemanticMemory::new(
            Some(Box::new(WorkspaceFakeAdapter {
                state: Arc::clone(&state),
                result: Ok(workspace_result(&query, Vec::new(), false)),
            })),
            WorkspaceSemanticMemoryConfig::default(),
        );
        let authoritative = vec![workspace_fact("exact", "fact-1")];

        assert!(!semantic.is_enabled());
        assert_eq!(semantic.retrieve(&query, authoritative.clone()), authoritative);
        assert_eq!(state.search_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn workspace_backend_failure_falls_back_to_authoritative_results() {
        let query = workspace_query(8);
        let state = Arc::new(WorkspaceFakeState::default());
        let semantic = workspace_semantic(
            Arc::clone(&state),
            Err(OrchestratorError::ToolFailure("sidecar unavailable".into())),
            true,
        );
        let authoritative = vec![workspace_fact("exact", "fact-1")];

        assert_eq!(semantic.retrieve(&query, authoritative.clone()), authoritative);
        assert_eq!(state.search_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_workspace_sidecar_falls_back_to_authoritative_results() {
        let query = workspace_query(8);
        let state = Arc::new(WorkspaceFakeState::default());
        let stale_hit =
            WorkspaceSemanticMemoryHit::new(workspace_projection("semantic", "fact-2"), 900_000);
        let semantic =
            workspace_semantic(state, Ok(workspace_result(&query, vec![stale_hit], true)), true);
        let authoritative = vec![workspace_fact("exact", "fact-1")];

        assert_eq!(semantic.retrieve(&query, authoritative.clone()), authoritative);
    }

    #[test]
    fn workspace_semantic_ties_and_caps_are_deterministic() {
        let query = workspace_query(3);
        let state = Arc::new(WorkspaceFakeState::default());
        let hits = vec![
            WorkspaceSemanticMemoryHit::new(workspace_projection("z", "fact-z"), 500_000),
            WorkspaceSemanticMemoryHit::new(workspace_projection("a", "fact-a"), 500_000),
            WorkspaceSemanticMemoryHit::new(workspace_projection("b", "fact-b"), 800_000),
        ];
        let semantic =
            workspace_semantic(Arc::clone(&state), Ok(workspace_result(&query, hits, false)), true);

        let facts = semantic.retrieve(&query, vec![workspace_fact("exact", "fact-1")]);
        let keys = facts.iter().map(|fact| fact.key.as_str()).collect::<Vec<_>>();
        assert_eq!(keys, vec!["exact", "b", "a"]);
        assert_eq!(state.searched_cap.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn workspace_semantic_hit_projects_rich_metadata_without_changing_authority() {
        let query = workspace_query(2);
        let state = Arc::new(WorkspaceFakeState::default());
        let hit =
            WorkspaceSemanticMemoryHit::new(workspace_projection("semantic", "fact-2"), 700_000);
        let semantic =
            workspace_semantic(state, Ok(workspace_result(&query, vec![hit], false)), true);

        let facts = semantic.retrieve(&query, Vec::new());
        let fact = &facts[0];
        assert_eq!(fact.authority, WorkspaceFactAuthority::HostVerified);
        assert_eq!(fact.freshness, WorkspaceFactFreshness::Current);
        assert_eq!(fact.state, WorkspaceFactState::Active);
        assert_eq!(fact.source_id, "fact-2");
        assert_eq!(fact.source_file.as_deref(), Some("src/lib.rs"));
        assert_eq!(fact.source_range, Some((3, 5)));
        assert_eq!(fact.selection_reason, WorkspaceFactSelectionReason::Semantic);
        assert_eq!(fact.provenance.source_id, "fact-2");
    }

    #[test]
    fn workspace_rebuild_hook_receives_authoritative_projections() {
        let query = workspace_query(8);
        let state = Arc::new(WorkspaceFakeState::default());
        let semantic = workspace_semantic(
            Arc::clone(&state),
            Ok(workspace_result(&query, Vec::new(), false)),
            true,
        );
        let projections = vec![workspace_projection("exact", "fact-1")];
        let request = WorkspaceSemanticRebuildRequest {
            workspace_digest: query.workspace_digest.clone(),
            root_set: query.root_set.clone(),
            model_version: query.model_version.clone(),
            reason: WorkspaceSemanticRebuildReason::ModelChange,
            projections: &projections,
        };

        let metadata = semantic.rebuild(&request).expect("rebuild succeeds").expect("enabled");
        assert_eq!(state.rebuild_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.rebuilt.lock().expect("rebuild capture lock").as_slice(), projections);
        assert_eq!(metadata.identity.workspace_digest, query.workspace_digest);
        assert_eq!(metadata.model_version, query.model_version);
    }
}
