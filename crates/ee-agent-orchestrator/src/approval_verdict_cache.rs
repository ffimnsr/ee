//! Bounded short-TTL verdict cache for [`ApprovalClassifier`].
//!
//! Classifier calls cost a model round-trip, so identical requests are served
//! from a small LRU instead of re-classifying.  Only `low` verdicts are ever
//! inserted: `medium`, `high`, failures, and timeouts must re-evaluate every
//! call so a changed argument shape can never hide behind a stale entry.
//!
//! [`ApprovalClassifier`]: super::approval_classifier::ApprovalClassifier

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::approval_classifier::ClassifierVerdict;
use crate::destructive_policy::SideEffectSubclass;
use crate::tools::SideEffectClass;

/// Maximum auto-approve verdicts retained per classifier.
pub(super) const VERDICT_CACHE_CAPACITY: usize = 64;
/// How long a cached auto-approve verdict stays valid.
pub(super) const VERDICT_CACHE_TTL: Duration = Duration::from_secs(120);
/// Cache key schema marker; bump when the key shape or risk semantics change.
pub(super) const VERDICT_CACHE_SCHEMA_VERSION: u8 = 1;

/// Identity of one classification request. Arguments are already redacted and
/// bounded by `ApprovalCandidate::new`, so their JSON rendering is stable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct VerdictCacheKey {
    pub(super) schema: u8,
    pub(super) model_id: String,
    pub(super) tool_name: String,
    pub(super) side_effect_class: SideEffectClass,
    pub(super) side_effect_subclass: Option<SideEffectSubclass>,
    pub(super) arguments: String,
}

#[derive(Debug)]
struct CachedVerdict {
    verdict: ClassifierVerdict,
    stored_at: tokio::time::Instant,
}

/// Bounded LRU with a lazy TTL. Expired entries are dropped on every
/// lookup/insert; a hit refreshes recency so hot verdicts survive eviction.
#[derive(Debug)]
pub(super) struct VerdictCache {
    capacity: usize,
    ttl: Duration,
    entries: Mutex<VecDeque<(VerdictCacheKey, CachedVerdict)>>,
}

impl VerdictCache {
    pub(super) fn new(capacity: usize, ttl: Duration) -> Self {
        Self { capacity: capacity.max(1), ttl, entries: Mutex::new(VecDeque::new()) }
    }

    pub(super) fn get(&self, key: &VerdictCacheKey) -> Option<ClassifierVerdict> {
        let now = tokio::time::Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|(_, cached)| Self::is_fresh(now, cached, self.ttl));
        let position = entries.iter().position(|(existing, _)| existing == key)?;
        let (_, entry) = entries.remove(position)?;
        let verdict = entry.verdict.clone();
        entries.push_back((key.clone(), entry));
        Some(verdict)
    }

    pub(super) fn insert(&self, key: VerdictCacheKey, verdict: ClassifierVerdict) {
        let now = tokio::time::Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|(_, cached)| Self::is_fresh(now, cached, self.ttl));
        if let Some(position) = entries.iter().position(|(existing, _)| *existing == key) {
            entries.remove(position);
        }
        while entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back((key, CachedVerdict { verdict, stored_at: now }));
    }

    fn is_fresh(now: tokio::time::Instant, cached: &CachedVerdict, ttl: Duration) -> bool {
        now.checked_duration_since(cached.stored_at).is_some_and(|age| age < ttl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::approval_classifier::RiskLevel;

    fn key(tool: &str, path: &str) -> VerdictCacheKey {
        VerdictCacheKey {
            schema: VERDICT_CACHE_SCHEMA_VERSION,
            model_id: String::from("classifier"),
            tool_name: tool.to_string(),
            side_effect_class: SideEffectClass::Write,
            side_effect_subclass: Some(SideEffectSubclass::Overwrite),
            arguments: path.to_string(),
        }
    }

    fn verdict(reason: &str) -> ClassifierVerdict {
        ClassifierVerdict { risk: RiskLevel::Low, reason: reason.to_string() }
    }

    #[test]
    fn insert_then_get_returns_the_cached_verdict() {
        let cache = VerdictCache::new(4, Duration::from_secs(60));
        let first = key("write_file", "/work/a.rs");
        assert!(cache.get(&first).is_none());
        cache.insert(first.clone(), verdict("narrow edit"));
        let cached = cache.get(&first).expect("cached");
        assert_eq!(cached.reason, "narrow edit");
        assert!(cache.get(&key("write_file", "/work/b.rs")).is_none());
    }

    #[test]
    fn capacity_eviction_prefers_the_least_recently_used_entry() {
        let cache = VerdictCache::new(2, Duration::from_secs(60));
        let first = key("write_file", "/work/a.rs");
        let second = key("write_file", "/work/b.rs");
        let third = key("write_file", "/work/c.rs");
        cache.insert(first.clone(), verdict("a"));
        cache.insert(second.clone(), verdict("b"));
        assert!(cache.get(&first).is_some());
        cache.insert(third, verdict("c"));
        assert!(cache.get(&first).is_some(), "recently used entry survives");
        assert!(cache.get(&second).is_none(), "least recently used entry evicted");
    }

    #[test]
    fn insert_replaces_an_existing_key() {
        let cache = VerdictCache::new(4, Duration::from_secs(60));
        let first = key("write_file", "/work/a.rs");
        cache.insert(first.clone(), verdict("old"));
        cache.insert(first.clone(), verdict("new"));
        assert_eq!(cache.get(&first).expect("cached").reason, "new");
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_the_ttl() {
        let cache = VerdictCache::new(4, Duration::from_secs(30));
        let first = key("write_file", "/work/a.rs");
        cache.insert(first.clone(), verdict("narrow edit"));
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(cache.get(&first).is_some());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(cache.get(&first).is_none());
    }
}
