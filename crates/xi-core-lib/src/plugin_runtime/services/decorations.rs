//! `editor.decorations` service.
//!
//! Patches are revisioned and validated against the current document. The
//! backend never interprets colours; only declared semantic style roles are
//! accepted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::effect::{DecorationPatch, DecorationSpan};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::{ContributionId, PluginId};
use ee_plugin_api::model::ViewId;
use ee_plugin_api::service::Service;

use crate::plugin_runtime::core_api::SharedEditorAccess;

/// Maximum spans in one patch.
pub const MAX_DECORATION_SPANS: usize = 10_000;
/// Maximum total text bytes in one patch.
pub const MAX_DECORATION_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct StoredPatch {
    owner: Option<PluginId>,
    patch: DecorationPatch,
    order: u64,
}

/// Dynamic decoration provider installed by a plugin.
pub trait DecorationProvider: Send + Sync + 'static {
    fn collect(&self, view: ViewId, rev: u64) -> Result<Vec<DecorationPatch>, PluginError>;
}

struct ProviderEntry {
    provider: Arc<dyn DecorationProvider>,
}

impl Clone for ProviderEntry {
    fn clone(&self) -> Self {
        Self { provider: Arc::clone(&self.provider) }
    }
}

/// Revisioned decoration provider registry.
#[derive(Clone)]
pub struct DecorationService {
    access: SharedEditorAccess,
    patches: Arc<Mutex<Vec<StoredPatch>>>,
    providers: Arc<std::sync::RwLock<BTreeMap<ContributionId, ProviderEntry>>>,
    next_order: Arc<AtomicU64>,
}

impl DecorationService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self {
            access,
            patches: Arc::new(Mutex::new(Vec::new())),
            providers: Arc::new(std::sync::RwLock::new(BTreeMap::new())),
            next_order: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Register a provider that answers `editor.decorations.collect`.
    pub fn register_provider(
        &self,
        id: ContributionId,
        provider: Arc<dyn DecorationProvider>,
    ) -> Disposer {
        let providers = Arc::clone(&self.providers);
        providers
            .write()
            .expect("decoration provider lock poisoned")
            .insert(id.clone(), ProviderEntry { provider: Arc::clone(&provider) });
        Disposer::new(move || {
            if let Ok(mut providers) = providers.write()
                && providers.get(&id).is_some_and(|entry| Arc::ptr_eq(&entry.provider, &provider))
            {
                providers.remove(&id);
            }
        })
    }

    /// Validate and store a patch. Replaces the previous patch by the same
    /// owner, view, and decoration kind.
    pub fn apply(
        &self,
        owner: Option<&PluginId>,
        patch: &DecorationPatch,
    ) -> Result<(), PluginError> {
        self.validate(patch)?;
        let mut patches = self.patches.lock().expect("decoration lock poisoned");
        patches.retain(|stored| {
            !(stored.owner.as_ref() == owner
                && stored.patch.view_id == patch.view_id
                && stored.patch.decoration_kind == patch.decoration_kind)
        });
        let order = self.next_order.fetch_add(1, Ordering::SeqCst);
        patches.push(StoredPatch { owner: owner.cloned(), patch: patch.clone(), order });
        Ok(())
    }

    /// Remove patches by owner and optional kind, newest first.
    pub fn remove(&self, owner: Option<&PluginId>, kind: Option<&str>) -> usize {
        let mut patches = self.patches.lock().expect("decoration lock poisoned");
        let before = patches.len();
        patches.retain(|stored| {
            let owner_matches = stored.owner.as_ref() == owner;
            let kind_matches = kind.is_none_or(|kind| stored.patch.decoration_kind == kind);
            !(owner_matches && kind_matches)
        });
        before - patches.len()
    }

    /// Patches for one view in deterministic provider order.
    pub fn collect(&self, view: ViewId, rev: u64) -> Result<Vec<DecorationPatch>, PluginError> {
        let buffer = self.access.view_buffer(view).ok_or_else(|| PluginError::Schema {
            message: format!("view {} is not open", view.0),
        })?;
        let snapshot = self.access.document_snapshot(buffer)?;
        if rev != snapshot.rev {
            return Err(PluginError::Runtime {
                message: format!("stale decoration revision {rev} (current {})", snapshot.rev),
            });
        }
        let mut patches = self
            .patches
            .lock()
            .expect("decoration lock poisoned")
            .iter()
            .filter(|stored| stored.patch.view_id == view)
            .map(|stored| (stored.order, stored.patch.clone()))
            .collect::<Vec<_>>();

        let providers = self
            .providers
            .read()
            .expect("decoration provider lock poisoned")
            .iter()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect::<Vec<_>>();
        for (_, entry) in providers {
            for patch in entry.provider.collect(view, rev)? {
                self.validate(&patch)?;
                let order = self.next_order.fetch_add(1, Ordering::SeqCst);
                patches.push((order, patch));
            }
        }

        patches.sort_by(|left, right| {
            left.1.decoration_kind.cmp(&right.1.decoration_kind).then(left.0.cmp(&right.0))
        });
        Ok(patches.into_iter().map(|(_, patch)| patch).collect())
    }

    /// Number of stored patches.
    pub fn len(&self) -> usize {
        self.patches.lock().expect("decoration lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn validate(&self, patch: &DecorationPatch) -> Result<(), PluginError> {
        if patch.decoration_kind.trim().is_empty() {
            return Err(PluginError::Schema {
                message: "decoration kind must not be empty".to_owned(),
            });
        }
        let buffer = self.access.view_buffer(patch.view_id).ok_or_else(|| PluginError::Schema {
            message: format!("view {} is not open", patch.view_id.0),
        })?;
        let snapshot = self.access.document_snapshot(buffer)?;
        if patch.rev != snapshot.rev {
            return Err(PluginError::Runtime {
                message: format!(
                    "stale decoration revision {} (current {})",
                    patch.rev, snapshot.rev
                ),
            });
        }
        if patch.spans.len() > MAX_DECORATION_SPANS {
            return Err(PluginError::Resource {
                message: format!("decoration span count exceeds {MAX_DECORATION_SPANS}"),
            });
        }

        let mut last_end = 0;
        let mut bytes = 0usize;
        for span in &patch.spans {
            validate_span(span, snapshot.len, &mut last_end)?;
            bytes = bytes.saturating_add(span.text.as_deref().map_or(0, str::len));
        }
        if bytes > MAX_DECORATION_BYTES {
            return Err(PluginError::Resource {
                message: format!("decoration text exceeds {MAX_DECORATION_BYTES} bytes"),
            });
        }
        Ok(())
    }
}

fn validate_span(
    span: &DecorationSpan,
    len: usize,
    last_end: &mut usize,
) -> Result<(), PluginError> {
    if span.start > span.end || span.end > len {
        return Err(PluginError::Schema {
            message: format!(
                "decoration range {}..{} is invalid for length {len}",
                span.start, span.end
            ),
        });
    }
    if span.start < *last_end {
        return Err(PluginError::Schema {
            message: "decoration spans must be sorted and non-overlapping".to_owned(),
        });
    }
    *last_end = span.end;
    if !is_known_style_role(span.style.as_str()) {
        return Err(PluginError::Schema {
            message: format!("unknown style role `{}`", span.style.as_str()),
        });
    }
    Ok(())
}

/// Style roles are semantic. Plugins cannot send RGB/ANSI.
pub fn is_known_style_role(role: &str) -> bool {
    const PREFIXES: [&str; 6] = ["ui.", "syntax.", "editor.", "diagnostic.", "search.", "vcs."];
    PREFIXES.iter().any(|prefix| role.starts_with(prefix)) && !role.chars().any(char::is_control)
}

impl Service for DecorationService {
    const ID: &'static str = "editor.decorations@1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::test_support::MemoryEditor;
    use ee_plugin_api::effect::StyleRole;
    use ee_plugin_api::model::BufferId;

    fn service() -> (DecorationService, Arc<MemoryEditor>) {
        let editor = Arc::new(MemoryEditor::new());
        editor.set_document(BufferId(1), ViewId(1), "hello world");
        (DecorationService::new(editor.clone()), editor)
    }

    fn patch(rev: u64, span: DecorationSpan) -> DecorationPatch {
        DecorationPatch {
            view_id: ViewId(1),
            rev,
            decoration_kind: "diagnostic".to_owned(),
            spans: vec![span],
        }
    }

    fn span(start: usize, end: usize, style: &str) -> DecorationSpan {
        DecorationSpan { start, end, style: StyleRole::new(style), text: None }
    }

    #[test]
    fn valid_patch_round_trips() {
        let (service, _editor) = service();
        service.apply(None, &patch(0, span(0, 5, "syntax.keyword"))).unwrap();
        let patches = service.collect(ViewId(1), 0).unwrap();
        assert_eq!(patches.len(), 1);
    }

    #[test]
    fn stale_revision_is_rejected() {
        let (service, _editor) = service();
        let error = service.apply(None, &patch(9, span(0, 1, "syntax.keyword"))).unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[test]
    fn invalid_range_is_rejected() {
        let (service, _editor) = service();
        let error = service.apply(None, &patch(0, span(5, 20, "syntax.keyword"))).unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Schema);
    }

    #[test]
    fn unknown_style_is_rejected() {
        let (service, _editor) = service();
        let error = service.apply(None, &patch(0, span(0, 1, "red"))).unwrap_err();
        assert!(error.to_string().contains("unknown style"));
    }

    #[test]
    fn quota_breach_is_rejected() {
        let (service, _editor) = service();
        let mut oversized = patch(0, span(0, 1, "syntax.keyword"));
        oversized.spans =
            (0..=MAX_DECORATION_SPANS).map(|index| span(index, index, "syntax.keyword")).collect();
        let error = service.apply(None, &oversized).unwrap_err();
        assert!(error.to_string().contains("span count"));
    }

    #[test]
    fn removal_is_scoped_by_owner_and_kind() {
        use ee_plugin_api::ids::PluginId;
        let (service, _editor) = service();
        let first = PluginId::new("first").unwrap();
        let second = PluginId::new("second").unwrap();
        service.apply(Some(&first), &patch(0, span(0, 1, "syntax.keyword"))).unwrap();
        service.apply(Some(&second), &patch(0, span(0, 1, "syntax.keyword"))).unwrap();
        assert_eq!(service.remove(Some(&first), Some("diagnostic")), 1);
        assert_eq!(service.len(), 1);
    }

    #[test]
    fn collect_is_deterministic() {
        let (service, _editor) = service();
        let mut second = patch(0, span(2, 3, "ui.status.text"));
        second.decoration_kind = "annotation".to_owned();
        service.apply(None, &patch(0, span(0, 1, "syntax.keyword"))).unwrap();
        service.apply(None, &second).unwrap();
        let patches = service.collect(ViewId(1), 0).unwrap();
        assert_eq!(patches[0].decoration_kind, "annotation");
        assert_eq!(patches[1].decoration_kind, "diagnostic");
    }

    #[test]
    fn provider_patches_are_collected_and_removed() {
        struct Provider;
        impl DecorationProvider for Provider {
            fn collect(&self, view: ViewId, rev: u64) -> Result<Vec<DecorationPatch>, PluginError> {
                Ok(vec![DecorationPatch {
                    view_id: view,
                    rev,
                    decoration_kind: "gutter".to_owned(),
                    spans: vec![span(0, 1, "ui.gutter")],
                }])
            }
        }

        let (service, _editor) = service();
        let id = ContributionId::new("demo.decorations").unwrap();
        let disposer = service.register_provider(id, Arc::new(Provider));
        assert_eq!(service.collect(ViewId(1), 0).unwrap().len(), 1);
        disposer.dispose();
        assert!(service.collect(ViewId(1), 0).unwrap().is_empty());
    }

    #[test]
    fn style_role_prefixes_are_enforced() {
        assert!(is_known_style_role("syntax.keyword"));
        assert!(!is_known_style_role("red"));
        assert!(!is_known_style_role("syntax.\u{7}"));
    }
}
