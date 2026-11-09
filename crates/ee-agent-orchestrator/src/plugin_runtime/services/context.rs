//! `agent.context@1`: bounded, trust-labeled context providers.
//!
//! Providers never discover or expand content themselves: they return bounded
//! excerpts, and the service enforces the per-provider and total byte caps,
//! redacts secret-like values, preserves provenance, and deduplicates
//! deterministically. Repository and tool content stays untrusted.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use ee_plugin_api::contribution::{ContextProviderContribution, TrustLevel as ContributionTrust};
use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};

use crate::plugin_runtime::effects::ContextUpdateEffect;
use crate::trust::TrustLevel;

/// Default maximum selected context items.
pub const DEFAULT_CONTEXT_ITEMS: usize = 32;
/// Default maximum selected context bytes.
pub const DEFAULT_CONTEXT_BYTES: usize = 16 * 1024;
/// Maximum registered context providers.
pub const MAX_CONTEXT_PROVIDERS: usize = 32;

/// One bounded context excerpt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextContribution {
    /// Stable source-local identifier.
    pub id: String,
    /// Source label retained in provenance.
    pub source: String,
    /// Trust classification; controls transcript metadata.
    pub trust: TrustLevel,
    /// Source revision.
    pub revision: String,
    /// Redacted, bounded excerpt.
    pub excerpt: String,
}

/// Inputs handed to a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCollectRequest {
    pub session_id: String,
    pub turn: u64,
    pub max_items: usize,
    pub max_bytes: usize,
}

/// Merged, bounded context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCollection {
    pub items: Vec<ContextContribution>,
    pub omitted: usize,
    pub bytes: usize,
}

/// In-process context provider.
pub trait ContextProvider: Send + Sync + 'static {
    fn collect(&self, request: &ContextCollectRequest) -> Vec<ContextContribution>;
}

struct ProviderEntry {
    plugin: PluginId,
    trust: TrustLevel,
    max_bytes: usize,
    provider: Option<Arc<dyn ContextProvider>>,
}

type ProviderSnapshot = (String, TrustLevel, usize, Option<Arc<dyn ContextProvider>>);

/// Context registry service.
pub struct ContextService {
    providers: Arc<RwLock<BTreeMap<String, ProviderEntry>>>,
    applied: Arc<Mutex<BTreeMap<String, Vec<ContextContribution>>>>,
    max_items: usize,
    max_bytes: usize,
}

impl Service for ContextService {
    const ID: &'static str = "agent.context@1";
}

impl Default for ContextService {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(BTreeMap::new())),
            applied: Arc::new(Mutex::new(BTreeMap::new())),
            max_items: DEFAULT_CONTEXT_ITEMS,
            max_bytes: DEFAULT_CONTEXT_BYTES,
        }
    }

    /// Sets host-wide caps. Per-provider caps still apply.
    pub fn set_caps(&mut self, max_items: usize, max_bytes: usize) {
        self.max_items = max_items.max(1);
        self.max_bytes = max_bytes.max(1);
    }

    /// Registers a provider with its trust label and byte cap.
    pub fn register_provider(
        &self,
        plugin: &PluginId,
        id: impl Into<String>,
        trust: TrustLevel,
        max_bytes: usize,
        provider: Arc<dyn ContextProvider>,
    ) -> Result<Disposer, PluginError> {
        self.insert_entry(plugin, id, trust, max_bytes, Some(provider))
    }

    /// Records a declarative provider contribution; it produces no content
    /// until the plugin submits a context update effect.
    pub fn register_contribution(
        &self,
        plugin: &PluginId,
        contribution: &ContextProviderContribution,
    ) -> Result<Disposer, PluginError> {
        self.insert_entry(
            plugin,
            contribution.id.clone(),
            trust_from_contribution(contribution.trust),
            contribution.max_bytes,
            None,
        )
    }

    fn insert_entry(
        &self,
        plugin: &PluginId,
        id: impl Into<String>,
        trust: TrustLevel,
        max_bytes: usize,
        provider: Option<Arc<dyn ContextProvider>>,
    ) -> Result<Disposer, PluginError> {
        let id = id.into();
        let mut providers = self.providers.write().expect("context provider lock poisoned");
        if providers.len() >= MAX_CONTEXT_PROVIDERS {
            return Err(PluginError::Resource {
                message: format!("context provider limit of {MAX_CONTEXT_PROVIDERS} reached"),
            });
        }
        if providers.contains_key(&id) {
            return Err(PluginError::Runtime {
                message: format!("duplicate context provider `{id}`"),
            });
        }
        providers.insert(
            id.clone(),
            ProviderEntry { plugin: plugin.clone(), trust, max_bytes: max_bytes.max(1), provider },
        );
        drop(providers);

        let handle = Arc::clone(&self.providers);
        let applied = Arc::clone(&self.applied);
        let plugin = plugin.clone();
        Ok(Disposer::new(move || {
            if let Ok(mut providers) = handle.write()
                && providers.get(&id).is_some_and(|entry| entry.plugin == plugin)
            {
                providers.remove(&id);
                if let Ok(mut applied) = applied.lock() {
                    applied.remove(&id);
                }
            }
        }))
    }

    /// Applies a plugin context update effect.
    pub fn apply_update(&self, update: &ContextUpdateEffect) -> Result<(), PluginError> {
        let providers = self.providers.read().expect("context provider lock poisoned");
        let max_bytes = providers
            .get(&update.provider_id)
            .map_or(DEFAULT_CONTEXT_BYTES, |entry| entry.max_bytes);
        drop(providers);
        let mut items = update.items.clone();
        for item in &mut items {
            item.trust = update.trust;
            item.excerpt = bound_excerpt(&item.excerpt, max_bytes);
        }
        self.applied
            .lock()
            .expect("applied context lock poisoned")
            .insert(update.provider_id.clone(), items);
        Ok(())
    }

    /// Collects bounded context from registered providers.
    #[must_use]
    pub fn collect(&self, session_id: &str, turn: u64) -> ContextCollection {
        self.collect_with(ContextCollectRequest {
            session_id: session_id.to_owned(),
            turn,
            max_items: self.max_items,
            max_bytes: self.max_bytes,
        })
    }

    /// Collects bounded context with explicit caps.
    #[must_use]
    pub fn collect_with(&self, request: ContextCollectRequest) -> ContextCollection {
        let mut candidates: Vec<ContextContribution> = Vec::new();
        // Snapshot the registry before invoking any provider so a provider that
        // submits an update cannot deadlock on this read lock.
        let entries: Vec<ProviderSnapshot> = {
            let providers = self.providers.read().expect("context provider lock poisoned");
            providers
                .iter()
                .map(|(id, entry)| {
                    (id.clone(), entry.trust, entry.max_bytes, entry.provider.clone())
                })
                .collect()
        };
        let applied_snapshot = self.applied.lock().expect("applied context lock poisoned").clone();
        for (id, trust, max_bytes, provider) in entries {
            let mut items = match &provider {
                Some(provider) => provider.collect(&request),
                None => applied_snapshot.get(&id).cloned().unwrap_or_default(),
            };
            for item in &mut items {
                item.trust = trust;
                item.excerpt = bound_excerpt(&item.excerpt, max_bytes);
            }
            candidates.extend(items);
        }
        candidates.sort_by(|left, right| {
            left.source
                .cmp(&right.source)
                .then(left.id.cmp(&right.id))
                .then(left.revision.cmp(&right.revision))
        });

        let guard = crate::sensitive_data::SensitiveDataGuard::new();
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        let mut bytes = 0usize;
        let mut omitted = 0usize;
        for mut item in candidates {
            let key = (item.source.clone(), item.id.clone());
            if !seen.insert(key) {
                omitted += 1;
                continue;
            }
            item.excerpt = guard.redact(&item.excerpt);
            let item_bytes = item.excerpt.len();
            if items.len() >= request.max_items || bytes + item_bytes > request.max_bytes {
                omitted += 1;
                continue;
            }
            bytes += item_bytes;
            items.push(item);
        }
        ContextCollection { items, omitted, bytes }
    }

    /// Number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.read().expect("context provider lock poisoned").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ContextService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextService")
            .field("providers", &self.len())
            .finish_non_exhaustive()
    }
}

fn trust_from_contribution(trust: ContributionTrust) -> TrustLevel {
    match trust {
        ContributionTrust::Untrusted => TrustLevel::ToolOutputUntrusted,
        ContributionTrust::Workspace => TrustLevel::ToolOutputUntrusted,
        ContributionTrust::Trusted => TrustLevel::UserPrompt,
    }
}

/// Truncates on a character boundary within `max_bytes`.
pub(crate) fn bound_excerpt(excerpt: &str, max_bytes: usize) -> String {
    if excerpt.len() <= max_bytes {
        return excerpt.to_owned();
    }
    let mut end = max_bytes;
    while !excerpt.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    excerpt[..end].to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]
    use super::*;

    fn plugin() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    struct FixedProvider {
        items: Vec<ContextContribution>,
    }

    impl ContextProvider for FixedProvider {
        fn collect(&self, _request: &ContextCollectRequest) -> Vec<ContextContribution> {
            self.items.clone()
        }
    }

    fn item(id: &str, source: &str, excerpt: &str) -> ContextContribution {
        ContextContribution {
            id: id.to_owned(),
            source: source.to_owned(),
            trust: TrustLevel::ToolOutputUntrusted,
            revision: "rev-1".to_owned(),
            excerpt: excerpt.to_owned(),
        }
    }

    #[test]
    fn labels_trust_from_the_registration() {
        let service = ContextService::new();
        service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::SystemPolicy,
                1024,
                Arc::new(FixedProvider { items: vec![item("a", "file", "hello")] }),
            )
            .expect("provider");
        let collection = service.collect("session-1", 1);
        assert_eq!(collection.items.len(), 1);
        assert_eq!(collection.items[0].trust, TrustLevel::SystemPolicy);
        assert_eq!(collection.items[0].source, "file");
    }

    #[test]
    fn provider_byte_cap_truncates_on_boundary() {
        let service = ContextService::new();
        service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::ToolOutputUntrusted,
                5,
                Arc::new(FixedProvider { items: vec![item("a", "file", "abcdefghij")] }),
            )
            .expect("provider");
        let collection = service.collect("session-1", 1);
        assert_eq!(collection.items[0].excerpt, "abcde");
    }

    #[test]
    fn total_caps_omit_deterministically() {
        let service = ContextService::new();
        service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(FixedProvider {
                    items: vec![item("b", "file", "bbbb"), item("a", "file", "aaaa")],
                }),
            )
            .expect("provider");
        let collection = service.collect_with(ContextCollectRequest {
            session_id: "session-1".to_owned(),
            turn: 1,
            max_items: 1,
            max_bytes: 1024,
        });
        assert_eq!(collection.items.len(), 1);
        assert_eq!(collection.items[0].id, "a");
        assert_eq!(collection.omitted, 1);
    }

    #[test]
    fn duplicate_source_ids_are_deduplicated() {
        let service = ContextService::new();
        service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(FixedProvider { items: vec![item("a", "file", "first")] }),
            )
            .expect("provider");
        service
            .register_provider(
                &plugin(),
                "demo.other",
                TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(FixedProvider { items: vec![item("a", "file", "second")] }),
            )
            .expect("provider");
        let collection = service.collect("session-1", 1);
        assert_eq!(collection.items.len(), 1);
        assert_eq!(collection.omitted, 1);
    }

    #[test]
    fn secrets_are_redacted() {
        let service = ContextService::new();
        service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(FixedProvider {
                    items: vec![item("a", "file", "api_key=sk-live-abcdef123456")],
                }),
            )
            .expect("provider");
        let collection = service.collect("session-1", 1);
        assert!(!collection.items[0].excerpt.contains("sk-live-abcdef123456"));
    }

    #[test]
    fn removal_disposes_only_the_owner() {
        let service = ContextService::new();
        let disposer = service
            .register_provider(
                &plugin(),
                "demo.files",
                TrustLevel::ToolOutputUntrusted,
                1024,
                Arc::new(FixedProvider { items: Vec::new() }),
            )
            .expect("provider");
        assert_eq!(service.len(), 1);
        disposer.dispose();
        assert!(service.is_empty());
    }
}
