//! Bounded memory store for per-turn and per-session facts.
//!
//! Facts are kept inside the configured byte limit by evicting the oldest
//! items first.  Sensitive items (secrets, raw terminal output) are rejected
//! outright so they never reach the model context.  [`MemoryStore::compact_context`]
//! exports a deterministic summary for model requests.

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::sensitive_data::SensitiveDataGuard;
use crate::tasks::TaskId;
use crate::trust::TrustLevel;

/// One fact stored in [`MemoryStore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryItem {
    /// Stable lookup key.
    pub key: String,
    /// The fact value.
    pub value: String,
    /// The task that produced the fact, when known.
    pub source_task: Option<TaskId>,
    /// Whether the value is sensitive (secrets, credentials, raw output);
    /// sensitive items are never stored.
    pub sensitive: bool,
    /// Trust level of the fact; tool and subagent facts default to untrusted.
    pub trust: TrustLevel,
}

impl MemoryItem {
    /// Creates a non-sensitive item with no source task; trust defaults to
    /// untrusted tool output (the conservative default for stored facts).
    #[must_use]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source_task: None,
            sensitive: false,
            trust: TrustLevel::ToolOutputUntrusted,
        }
    }

    /// Creates a non-sensitive item attributed to a task.
    #[must_use]
    pub fn from_task(key: impl Into<String>, value: impl Into<String>, source: TaskId) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source_task: Some(source),
            sensitive: false,
            trust: TrustLevel::ToolOutputUntrusted,
        }
    }

    /// Marks the item as sensitive; the store rejects such items.
    #[must_use]
    pub fn as_sensitive(mut self) -> Self {
        self.sensitive = true;
        self
    }

    /// Overrides the trust level of the item.
    #[must_use]
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Serialized byte size of the item (key + value).
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.key.len() + self.value.len()
    }
}

/// Bounded, deterministic memory container.
///
/// Inserting an item evicts the oldest stored items until the new item fits,
/// so the store always stays within its configured limit.  Eviction order is
/// insertion order (oldest first), which is deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStore {
    limit_bytes: usize,
    total_bytes: usize,
    items: Vec<MemoryItem>,
}

impl MemoryStore {
    /// Creates a store that evicts oldest items to stay within `limit_bytes`.
    #[must_use]
    pub fn new(limit_bytes: usize) -> Self {
        Self { limit_bytes, total_bytes: 0, items: Vec::new() }
    }

    /// Inserts an item, evicting oldest items until it fits.
    ///
    /// Sensitive items are rejected with a policy error, and secret-like
    /// values are redacted before storage.  Returns the number of evicted
    /// items, or an error when the item alone exceeds the limit.
    pub fn insert(&mut self, item: MemoryItem) -> Result<usize, OrchestratorError> {
        if item.sensitive {
            return Err(OrchestratorError::PolicyDenied(
                "sensitive memory items (secrets, credentials, raw output) are not stored".into(),
            ));
        }
        let item = MemoryItem { value: SensitiveDataGuard::new().redact(&item.value), ..item };
        let size = item.byte_size();
        if size > self.limit_bytes {
            return Err(OrchestratorError::BudgetExceeded(format!(
                "memory item of {size} bytes exceeds the configured {} byte limit",
                self.limit_bytes
            )));
        }
        let mut evicted = 0usize;
        while self.total_bytes.saturating_add(size) > self.limit_bytes {
            let removed = self.items.remove(0);
            self.total_bytes -= removed.byte_size();
            evicted += 1;
        }
        self.total_bytes += size;
        self.items.push(item);
        Ok(evicted)
    }

    /// First item with the exact key, if any.
    #[must_use]
    pub fn query(&self, key: &str) -> Option<MemoryItem> {
        self.items.iter().find(|item| item.key == key).cloned()
    }

    /// Items whose key starts with `prefix`, in insertion order.
    #[must_use]
    pub fn query_prefix(&self, prefix: &str) -> Vec<MemoryItem> {
        self.items.iter().filter(|item| item.key.starts_with(prefix)).cloned().collect()
    }

    /// Items produced by `source`, in insertion order.
    #[must_use]
    pub fn query_by_source(&self, source: &TaskId) -> Vec<MemoryItem> {
        self.items
            .iter()
            .filter(|item| item.source_task.as_ref() == Some(source))
            .cloned()
            .collect()
    }

    /// Fraction of the byte limit currently used (0.0–1.0; 1.0 when the
    /// limit is zero or the store is over budget).  Used by compaction to
    /// decide when low-value observations should be decayed.
    #[must_use]
    pub fn pressure(&self) -> f64 {
        if self.limit_bytes == 0 {
            return 1.0;
        }
        (self.total_bytes as f64 / self.limit_bytes as f64).clamp(0.0, 1.0)
    }

    /// Removes every item not passing `keep`, recomputing byte totals.
    /// Crate-internal so compaction ([`crate::memory_compaction`]) can
    /// rewrite the store in one deterministic pass.
    pub(crate) fn retain(&mut self, keep: impl FnMut(&MemoryItem) -> bool) -> usize {
        let before = self.items.len();
        self.items.retain(keep);
        let removed = before - self.items.len();
        self.total_bytes = self.items.iter().map(MemoryItem::byte_size).sum();
        removed
    }

    /// Removes every stored item whose key starts with `prefix`, returning
    /// the number of removed items.  Used by milestone compaction to drop
    /// low-value raw observations after a summary replaced them.
    pub fn remove_prefix(&mut self, prefix: &str) -> usize {
        let before = self.items.len();
        self.items.retain(|item| !item.key.starts_with(prefix));
        let removed = before - self.items.len();
        self.total_bytes = self.items.iter().map(MemoryItem::byte_size).sum();
        removed
    }

    /// Deterministic compact context for model requests: one `key: value`
    /// line per stored item in insertion order.  `None` when empty.
    #[must_use]
    pub fn compact_context(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        Some(
            self.items
                .iter()
                .map(|item| format!("{}: {}", item.key, item.value))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// All stored items in insertion order.
    #[must_use]
    pub fn items(&self) -> &[MemoryItem] {
        &self.items
    }

    /// Total serialized bytes currently stored.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// The configured byte limit.
    #[must_use]
    pub fn limit_bytes(&self) -> usize {
        self.limit_bytes
    }

    /// Number of stored items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_query_roundtrip() {
        let mut store = MemoryStore::new(1024);
        let item = MemoryItem::from_task("cwd", "/work", TaskId::new("task-1"));
        store.insert(item.clone()).expect("inserts");
        assert_eq!(store.len(), 1);
        assert_eq!(store.items()[0], item);
        assert_eq!(store.total_bytes(), item.byte_size());
        assert_eq!(store.limit_bytes(), 1024);
        assert_eq!(store.query("cwd"), Some(item.clone()));
        assert_eq!(store.query("missing"), None);
    }

    #[test]
    fn sensitive_items_are_rejected_by_default() {
        let mut store = MemoryStore::new(1024);
        let item = MemoryItem::new("api_token", "secret").as_sensitive();
        let error = store.insert(item).expect_err("sensitive item rejected");
        assert!(
            matches!(error, OrchestratorError::PolicyDenied(ref reason) if reason.contains("sensitive"))
        );
        assert!(store.is_empty(), "rejected item must not be stored");
    }

    #[test]
    fn byte_limit_evicts_oldest_items_first() {
        let mut store = MemoryStore::new(10);
        assert_eq!(store.insert(MemoryItem::new("a", "12345")).expect("fits"), 0);
        assert_eq!(store.insert(MemoryItem::new("b", "123456")).expect("evicts"), 1);
        assert_eq!(store.total_bytes(), 7, "only the newest item remains");
        assert_eq!(store.items().len(), 1);
        assert_eq!(store.items()[0].key, "b");
        assert_eq!(store.query("a"), None, "oldest item evicted");
        assert_eq!(store.query("b").expect("kept").value, "123456");
    }

    #[test]
    fn single_item_larger_than_limit_is_rejected() {
        let mut store = MemoryStore::new(4);
        let error = store.insert(MemoryItem::new("key", "too long")).expect_err("too big");
        assert!(matches!(error, OrchestratorError::BudgetExceeded(_)));
        assert!(store.is_empty());
    }

    #[test]
    fn queries_filter_by_prefix_and_source() {
        let mut store = MemoryStore::new(1024);
        let task = TaskId::new("task-1");
        let other = TaskId::new("task-2");
        store.insert(MemoryItem::from_task("file:a.txt", "read", task.clone())).expect("inserts");
        store.insert(MemoryItem::from_task("file:b.txt", "read", task.clone())).expect("inserts");
        store.insert(MemoryItem::from_task("note", "note", other.clone())).expect("inserts");

        assert_eq!(store.query_prefix("file:").len(), 2);
        assert_eq!(store.query_prefix("missing:").len(), 0);
        assert_eq!(store.query_by_source(&task).len(), 2);
        assert_eq!(store.query_by_source(&other).len(), 1);
        assert_eq!(store.query_by_source(&TaskId::new("task-99")).len(), 0);
    }

    #[test]
    fn remove_prefix_drops_matching_items_and_fixes_bytes() {
        let mut store = MemoryStore::new(1024);
        store.insert(MemoryItem::new("obs:1", "raw one")).expect("inserts");
        store.insert(MemoryItem::new("obs:2", "raw two")).expect("inserts");
        store.insert(MemoryItem::new("milestone:1", "summary")).expect("inserts");
        let total = store.total_bytes();

        assert_eq!(store.remove_prefix("obs:"), 2);
        assert_eq!(store.query("obs:1"), None);
        assert_eq!(store.query("obs:2"), None);
        assert_eq!(store.query("milestone:1").expect("kept").value, "summary");
        assert_eq!(store.total_bytes(), total - 24, "bytes recomputed after removal");
        assert_eq!(store.remove_prefix("missing:"), 0);
    }

    #[test]
    fn compact_context_excludes_evicted_and_sensitive_items() {
        let mut store = MemoryStore::new(20);
        store.insert(MemoryItem::new("a", "11111")).expect("fits"); // 6 bytes
        store.insert(MemoryItem::new("b", "22222")).expect("fits"); // 6 bytes
        store.insert(MemoryItem::new("c", "33333")).expect("fits"); // 6 bytes
        store
            .insert(MemoryItem::new("api_token", "secret").as_sensitive())
            .expect_err("sensitive rejected");
        // Inserting a 7-byte item evicts the oldest.
        store.insert(MemoryItem::new("d", "4444444")).expect("evicts");

        let context = store.compact_context().expect("non-empty");
        assert!(!context.contains("a:"), "evicted item excluded");
        assert!(context.contains("b: 22222"));
        assert!(context.contains("c: 33333"));
        assert!(context.contains("d: 4444444"));
        assert!(!context.contains("api_token"), "sensitive item never stored");
        assert!(store.total_bytes() <= store.limit_bytes(), "byte limit holds");
    }

    #[test]
    fn compact_context_is_none_when_empty() {
        let store = MemoryStore::new(1024);
        assert_eq!(store.compact_context(), None);
    }

    #[test]
    fn secret_like_values_are_redacted_before_storage() {
        let mut store = MemoryStore::new(1024);
        store.insert(MemoryItem::new("note", "the key is sk-live-1234567890")).expect("inserts");
        let stored = store.query("note").expect("stored");
        assert!(!stored.value.contains("sk-live-1234567890"), "{} must be redacted", stored.value);
        assert!(stored.value.contains("[redacted]"), "{} must carry the marker", stored.value);
        assert!(
            store.insert(MemoryItem::new("env", "OPENROUTER_API_KEY=sk-x")).is_ok(),
            "assignment-style secrets are masked, not rejected"
        );
        let stored = store.query("env").expect("stored");
        assert_eq!(stored.value, "OPENROUTER_API_KEY=[redacted]");
    }

    #[test]
    fn memory_items_carry_trust_labels() {
        let mut store = MemoryStore::new(1024);
        store.insert(MemoryItem::new("fact", "value")).expect("inserts");
        assert_eq!(
            store.query("fact").expect("stored").trust,
            crate::trust::TrustLevel::ToolOutputUntrusted
        );
        store
            .insert(MemoryItem::new("note", "v").with_trust(crate::trust::TrustLevel::UserPrompt))
            .expect("inserts");
        assert_eq!(
            store.query("note").expect("stored").trust,
            crate::trust::TrustLevel::UserPrompt
        );
    }

    #[test]
    fn pressure_reflects_used_fraction() {
        let mut store = MemoryStore::new(20);
        assert_eq!(store.pressure(), 0.0);
        store.insert(MemoryItem::new("a", "11111")).expect("fits"); // 6 bytes
        store.insert(MemoryItem::new("b", "22222")).expect("fits"); // 6 bytes
        assert!((store.pressure() - 0.6).abs() < f64::EPSILON);
        let zero = MemoryStore::new(0);
        assert_eq!(zero.pressure(), 1.0, "zero limit is always full");
    }

    #[test]
    fn retain_rewrites_items_and_fixes_bytes() {
        let mut store = MemoryStore::new(1024);
        store.insert(MemoryItem::new("a", "11111")).expect("inserts");
        store.insert(MemoryItem::new("b", "22222")).expect("inserts");
        store.insert(MemoryItem::new("c", "33333")).expect("inserts");
        let before = store.total_bytes();

        let mut keep = vec![true, false, true].into_iter();
        assert_eq!(store.retain(|_| keep.next().expect("flag")), 1);
        assert_eq!(store.len(), 2);
        assert_eq!(store.items()[0].key, "a");
        assert_eq!(store.items()[1].key, "c");
        assert_eq!(store.total_bytes(), before - 6, "bytes recomputed");
    }

    #[test]
    fn memory_store_roundtrips_through_json() {
        let mut store = MemoryStore::new(1024);
        store.insert(MemoryItem::new("fact", "value")).expect("inserts");
        let json = serde_json::to_string(&store).expect("serializes");
        let restored: MemoryStore = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.limit_bytes(), 1024);
        assert_eq!(restored.items(), store.items());
    }
}
