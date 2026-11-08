//! Cached document snapshot with revision-checked delta application.

use ee_plugin_api::model::PluginUpdate;

/// Failure modes for cache updates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    #[error("revision mismatch: expected {expected}, got {found}")]
    RevisionMismatch { expected: u64, found: u64 },
    #[error("update carries no delta; fetch a fresh snapshot")]
    NeedsSnapshot,
    #[error("delta produced an unexpected document size or line count")]
    InvalidDelta,
}

/// Cached document text and derived counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentCache {
    rev: u64,
    text: String,
    len: usize,
    line_count: usize,
}

impl DocumentCache {
    /// Build a cache from a full snapshot.
    pub fn new(rev: u64, text: impl Into<String>) -> Self {
        let text = text.into();
        let len = text.len();
        let line_count = count_lines(&text);
        Self { rev, text, len, line_count }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn line_count(&self) -> usize {
        self.line_count
    }

    /// Apply a sequential update.
    ///
    /// Updates must advance the revision by exactly one. A missing delta means
    /// the host could not stream the change, so the caller must fetch a full
    /// snapshot instead of guessing.
    pub fn apply_update(&mut self, update: &PluginUpdate) -> Result<(), CacheError> {
        if update.rev != self.rev + 1 {
            return Err(CacheError::RevisionMismatch { expected: self.rev + 1, found: update.rev });
        }
        let delta = update.delta.as_ref().ok_or(CacheError::NeedsSnapshot)?;
        let mut text = self.text.clone();
        delta.apply(&mut text).ok_or(CacheError::InvalidDelta)?;

        let len = text.len();
        let line_count = count_lines(&text);
        if len != update.new_len || line_count != update.new_line_count {
            return Err(CacheError::InvalidDelta);
        }

        self.text = text;
        self.len = len;
        self.line_count = line_count;
        self.rev = update.rev;
        Ok(())
    }
}

fn count_lines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_plugin_api::model::{DeltaOp, DocumentDelta, ViewId};

    fn update(rev: u64, ops: Vec<DeltaOp>, new_len: usize, new_line_count: usize) -> PluginUpdate {
        PluginUpdate {
            view_id: ViewId(1),
            delta: Some(DocumentDelta { ops }),
            new_len,
            new_line_count,
            rev,
            undo_group: None,
            edit_type: "insert".to_owned(),
            author: "core".to_owned(),
        }
    }

    #[test]
    fn applies_insert_and_delete() {
        let mut cache = DocumentCache::new(0, "hello");
        cache
            .apply_update(&update(
                1,
                vec![
                    DeltaOp::Insert { offset: 0, text: "> ".to_owned() },
                    DeltaOp::Delete { start: 2, end: 3 },
                ],
                6,
                1,
            ))
            .unwrap();
        assert_eq!(cache.text(), "> ello");
        assert_eq!(cache.rev(), 1);
        assert_eq!(cache.len(), 6);
        assert_eq!(cache.line_count(), 1);
    }

    #[test]
    fn rejects_stale_revision() {
        let mut cache = DocumentCache::new(4, "hello");
        let err = cache.apply_update(&update(4, vec![], 5, 1)).unwrap_err();
        assert_eq!(err, CacheError::RevisionMismatch { expected: 5, found: 4 });
    }

    #[test]
    fn missing_delta_requires_snapshot() {
        let mut cache = DocumentCache::new(0, "hello");
        let mut update = update(1, vec![], 5, 1);
        update.delta = None;
        assert_eq!(cache.apply_update(&update).unwrap_err(), CacheError::NeedsSnapshot);
    }

    #[test]
    fn inconsistent_counters_are_rejected() {
        let mut cache = DocumentCache::new(0, "hello");
        let err = cache
            .apply_update(&update(
                1,
                vec![DeltaOp::Insert { offset: 0, text: "!".to_owned() }],
                999,
                1,
            ))
            .unwrap_err();
        assert_eq!(err, CacheError::InvalidDelta);
        // Failed updates must not mutate the cache.
        assert_eq!(cache.text(), "hello");
        assert_eq!(cache.rev(), 0);
    }

    #[test]
    fn empty_document_has_one_line() {
        let cache = DocumentCache::new(0, "");
        assert_eq!(cache.line_count(), 1);
    }
}
