//! Memory compaction and decay.
//!
//! [`compact_memory`] rewrites a [`MemoryStore`] in one deterministic pass:
//! repeated facts with the same key *and* compatible provenance (same source
//! task, same trust label) are merged by keeping the newest value, and stale
//! low-value observations (keys under the configured low-value prefix, e.g.
//! `obs:`) are decayed oldest-first while the store is over its pressure
//! threshold.  Decisions, constraints, and validation results (keys under
//! [`PROTECTED_MEMORY_PREFIXES`]) are never merged, never decayed, and never
//! removed by compaction.
//!
//! Compaction is distinct from insert-time eviction: it runs proactively
//! (milestones, turn boundaries) to keep high-value knowledge alive without
//! touching protected state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::memory::MemoryStore;
use crate::milestones::{DEFAULT_COMPACTION_PRESSURE, DEFAULT_LOW_VALUE_PREFIX};
use crate::tasks::TaskId;
use crate::trust::TrustLevel;

/// Key prefixes compaction never merges, decays, or removes.
pub const PROTECTED_MEMORY_PREFIXES: [&str; 3] = ["decision:", "constraint:", "validation:"];

/// Compaction knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryCompactionConfig {
    /// Used-bytes fraction (0.0–1.0) above which low-value observations are
    /// decayed; 1.0 disables pressure decay, 0.0 always decays.
    pub pressure: f64,
    /// Key prefix of low-value raw observations to decay under pressure.
    pub low_value_prefix: String,
    /// Whether duplicate facts are merged at all; disable to only decay.
    pub merge_duplicates: bool,
}

impl Default for MemoryCompactionConfig {
    fn default() -> Self {
        Self {
            pressure: DEFAULT_COMPACTION_PRESSURE,
            low_value_prefix: DEFAULT_LOW_VALUE_PREFIX.to_string(),
            merge_duplicates: true,
        }
    }
}

/// What one compaction pass did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompactionReport {
    /// Duplicate facts merged away (older versions dropped).
    pub merged_duplicates: usize,
    /// Low-value observations decayed under pressure.
    pub decayed_observations: usize,
    /// Protected items present before compaction (all preserved).
    pub preserved_protected: usize,
    /// Stored items before the pass.
    pub items_before: usize,
    /// Stored items after the pass.
    pub items_after: usize,
    /// Stored bytes before the pass.
    pub bytes_before: usize,
    /// Stored bytes after the pass.
    pub bytes_after: usize,
}

/// Whether a key is protected knowledge (decisions, constraints, validation
/// results) that compaction must preserve.
#[must_use]
pub fn is_protected_key(key: &str) -> bool {
    PROTECTED_MEMORY_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Compacts the store in one deterministic pass.
///
/// 1. Merge: for every key, older items whose `(source_task, trust)` match the
///    newest item's are dropped, keeping the newest value.  Items with
///    different provenance (or protected keys) are never merged.
/// 2. Decay: while the store is over `pressure * limit`, the oldest
///    low-value-prefix items are dropped.  Protected items are never decayed.
///
/// Returns a [`CompactionReport`] with before/after counts.
#[must_use]
pub fn compact_memory(
    store: &mut MemoryStore,
    config: &MemoryCompactionConfig,
) -> CompactionReport {
    let items_before = store.len();
    let bytes_before = store.total_bytes();
    let preserved_protected =
        store.items().iter().filter(|item| is_protected_key(&item.key)).count();

    let mut keep = vec![true; store.len()];
    let mut merged_duplicates = 0usize;
    let mut decayed_observations = 0usize;

    if config.merge_duplicates {
        // Newest first; remember (key → provenance) of the newest survivor.
        let mut newest: BTreeMap<String, (Option<TaskId>, TrustLevel)> = BTreeMap::new();
        for index in (0..store.len()).rev() {
            let item = &store.items()[index];
            if is_protected_key(&item.key) {
                continue;
            }
            let provenance = (item.source_task.clone(), item.trust);
            match newest.get(&item.key) {
                Some(seen) if *seen == provenance => {
                    keep[index] = false;
                    merged_duplicates += 1;
                }
                _ => {
                    newest.insert(item.key.clone(), provenance);
                }
            }
        }
    }

    if config.pressure < 1.0 {
        let pressure_bytes =
            ((store.limit_bytes() as f64) * config.pressure.clamp(0.0, 1.0)).floor() as usize;
        let mut bytes = store.total_bytes();
        for (index, keep_item) in keep.iter_mut().enumerate().take(store.len()) {
            if bytes <= pressure_bytes {
                break;
            }
            if !*keep_item {
                continue;
            }
            let item = &store.items()[index];
            if item.key.starts_with(&config.low_value_prefix) && !is_protected_key(&item.key) {
                *keep_item = false;
                decayed_observations += 1;
                bytes -= item.byte_size();
            }
        }
    }

    let mut flags = keep.into_iter();
    let removed = store.retain(|_| flags.next().expect("keep flag"));

    CompactionReport {
        merged_duplicates,
        decayed_observations,
        preserved_protected,
        items_before,
        items_after: items_before - removed,
        bytes_before,
        bytes_after: store.total_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryItem;

    #[test]
    fn repeated_facts_with_same_provenance_merge_to_newest() {
        let mut store = MemoryStore::new(4_096);
        let task = TaskId::new("task-1");
        store
            .insert(MemoryItem::from_task("obs:file", "first read", task.clone()))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("obs:file", "second read", task.clone()))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("obs:file", "third read", task.clone()))
            .expect("inserts");

        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.merged_duplicates, 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.query("obs:file").expect("kept").value, "third read");
    }

    #[test]
    fn incompatible_provenance_is_not_merged() {
        let mut store = MemoryStore::new(4_096);
        let task = TaskId::new("task-1");
        // Same key, different trust labels → both are distinct facts.
        store
            .insert(MemoryItem::new("note", "user fact").with_trust(TrustLevel::UserPrompt))
            .expect("inserts");
        store.insert(MemoryItem::from_task("note", "tool fact", task.clone())).expect("inserts");

        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.merged_duplicates, 0);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn pressure_decay_drops_oldest_low_value_observations() {
        let mut store = MemoryStore::new(40);
        store.insert(MemoryItem::new("obs:1", "raw one")).expect("fits"); // 11 bytes
        store.insert(MemoryItem::new("obs:2", "raw two")).expect("fits"); // 11 bytes
        store.insert(MemoryItem::new("obs:3", "raw three")).expect("fits"); // 13 bytes
        store.insert(MemoryItem::new("fact", "important")).expect("fits"); // 14 bytes
        // 49 bytes / 40 limit → pressure 1.225, clamped to 1.0.

        let report = compact_memory(
            &mut store,
            &MemoryCompactionConfig { pressure: 0.5, ..MemoryCompactionConfig::default() },
        );
        assert!(report.decayed_observations >= 2, "oldest low-value observations decayed");
        assert_eq!(store.query("fact").expect("kept").value, "important");
        assert!(store.total_bytes() <= 20, "under the 50% pressure threshold");
        assert!(store.query("obs:1").is_none());
        assert!(store.query("obs:2").is_none());
        assert!(store.query("obs:3").is_none());
    }

    #[test]
    fn protected_items_survive_compaction_under_pressure() {
        let mut store = MemoryStore::new(64);
        store.insert(MemoryItem::new("decision:keep", "decision")).expect("fits"); // 21 bytes
        store.insert(MemoryItem::new("obs:1", "raw one")).expect("fits"); // 12 bytes
        store.insert(MemoryItem::new("constraint:keep", "constraint")).expect("fits"); // 26 bytes

        let report = compact_memory(
            &mut store,
            &MemoryCompactionConfig { pressure: 0.5, ..MemoryCompactionConfig::default() },
        );
        assert_eq!(report.decayed_observations, 1);
        assert_eq!(report.preserved_protected, 2);
        assert_eq!(store.query("decision:keep").expect("kept").value, "decision");
        assert_eq!(store.query("constraint:keep").expect("kept").value, "constraint");
        assert!(store.query("obs:1").is_none(), "low-value observation decayed");
    }

    #[test]
    fn merge_prefers_newest_and_preserves_decisions() {
        let mut store = MemoryStore::new(4_096);
        store
            .insert(MemoryItem::from_task("obs:file", "old", TaskId::new("task-1")))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("obs:file", "new", TaskId::new("task-1")))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("decision:api", "use v2", TaskId::new("task-1")))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("decision:api", "use v3", TaskId::new("task-1")))
            .expect("inserts");

        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.merged_duplicates, 1, "only the observation merged");
        assert_eq!(report.preserved_protected, 2, "both decision versions preserved");
        assert_eq!(store.query("obs:file").expect("merged").value, "new");
        assert_eq!(store.query("decision:api").expect("kept").value, "use v2");
    }

    #[test]
    fn no_work_is_a_noop_report() {
        let mut store = MemoryStore::new(4_096);
        store.insert(MemoryItem::new("fact", "value")).expect("inserts");
        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.merged_duplicates, 0);
        assert_eq!(report.decayed_observations, 0);
        assert_eq!(report.items_before, 1);
        assert_eq!(report.items_after, 1);
        assert_eq!(report.bytes_before, report.bytes_after);
    }

    #[test]
    fn merge_disabled_only_decays() {
        let mut store = MemoryStore::new(4_096);
        store
            .insert(MemoryItem::from_task("obs:x", "one", TaskId::new("task-1")))
            .expect("inserts");
        store
            .insert(MemoryItem::from_task("obs:x", "two", TaskId::new("task-1")))
            .expect("inserts");
        let report = compact_memory(
            &mut store,
            &MemoryCompactionConfig {
                merge_duplicates: false,
                pressure: 1.0,
                ..MemoryCompactionConfig::default()
            },
        );
        assert_eq!(report.merged_duplicates, 0);
        assert_eq!(report.decayed_observations, 0);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn pressure_of_one_disables_decay() {
        let mut store = MemoryStore::new(20);
        store.insert(MemoryItem::new("obs:1", "11111")).expect("fits");
        store.insert(MemoryItem::new("obs:2", "22222")).expect("fits");
        let report = compact_memory(
            &mut store,
            &MemoryCompactionConfig { pressure: 1.0, ..MemoryCompactionConfig::default() },
        );
        assert_eq!(report.decayed_observations, 0);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn report_counts_are_consistent() {
        let mut store = MemoryStore::new(4_096);
        for i in 0..3 {
            store
                .insert(MemoryItem::from_task("k", format!("v{i}"), TaskId::new("t")))
                .expect("inserts");
        }
        store.insert(MemoryItem::new("decision:x", "yes")).expect("inserts");
        let report = compact_memory(&mut store, &MemoryCompactionConfig::default());
        assert_eq!(report.items_before, 4);
        assert_eq!(report.items_after, 2);
        assert_eq!(report.merged_duplicates, 2);
        assert_eq!(report.preserved_protected, 1);
        assert_eq!(report.bytes_after, store.total_bytes());
    }
}
