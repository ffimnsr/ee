//! Revision-checked view snapshot used by native editor plugins.
//!
//! `EditorView` owns a [`DocumentCache`] and view metadata. Everything is
//! interior-mutable so background language-server callbacks can read a stable
//! snapshot without borrowing the plugin main loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{
    BufferId, EditAck, EditRequest, LanguageId, PluginUpdate, SelectionRange, TextEdit, ViewId,
};
use serde_json::{Value, json};

use crate::cache::{CacheError, DocumentCache};
use crate::client::{CommandsClient, DocumentClient, SyntaxClient, ViewsClient};

/// Editor settings relevant to LSP request formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewConfig {
    pub tab_size: usize,
    pub translate_tabs_to_spaces: bool,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self { tab_size: 4, translate_tabs_to_spaces: true }
    }
}

impl ViewConfig {
    fn from_value(value: &Value) -> Self {
        Self {
            tab_size: value
                .get("tab_size")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(4),
            translate_tabs_to_spaces: value
                .get("translate_tabs_to_spaces")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }
}

/// One cached editor view.
#[derive(Clone)]
pub struct EditorView {
    view_id: ViewId,
    buffer_id: BufferId,
    path: Option<PathBuf>,
    language: LanguageId,
    config: ViewConfig,
    cache: Arc<RwLock<DocumentCache>>,
    selections: Arc<RwLock<Vec<SelectionRange>>>,
    documents: DocumentClient,
    views: ViewsClient,
    syntax: SyntaxClient,
    commands: CommandsClient,
}

impl EditorView {
    /// Load a view through the v2 services.
    pub fn load(caller: Arc<dyn ServiceCaller>, view_id: ViewId) -> Result<Self, PluginError> {
        let views = ViewsClient::new(Arc::clone(&caller));
        let buffer_id = views.view_buffer(view_id)?.ok_or_else(|| PluginError::Runtime {
            message: format!("view {view_id:?} has no buffer"),
        })?;
        let documents = DocumentClient::new(Arc::clone(&caller));
        let snapshot = documents.snapshot(buffer_id)?;
        let text = documents.document_text(buffer_id, 16 * 1024 * 1024)?;
        let selections = selection_ranges(&caller, view_id)?;
        let language = snapshot.language.clone();
        Ok(Self {
            view_id,
            buffer_id,
            path: snapshot.path.clone().map(PathBuf::from),
            language,
            config: ViewConfig::default(),
            cache: Arc::new(RwLock::new(DocumentCache::new(snapshot.rev, text))),
            selections: Arc::new(RwLock::new(selections)),
            documents,
            views,
            syntax: SyntaxClient::new(Arc::clone(&caller)),
            commands: CommandsClient::new(caller),
        })
    }

    /// Reload revision and text without changing identity.
    pub fn refresh(&self) -> Result<(), PluginError> {
        let snapshot = self.documents.snapshot(self.buffer_id)?;
        let text = self.documents.document_text(self.buffer_id, 16 * 1024 * 1024)?;
        *self.cache.write().expect("document cache lock poisoned") =
            DocumentCache::new(snapshot.rev, text);
        Ok(())
    }

    pub fn view_id(&self) -> ViewId {
        self.view_id
    }

    pub fn get_id(&self) -> ViewId {
        self.view_id
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    pub fn get_buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get_path(&self) -> Option<&Path> {
        self.path()
    }

    pub fn language(&self) -> &LanguageId {
        &self.language
    }

    pub fn get_language_id(&self) -> &LanguageId {
        &self.language
    }

    pub fn set_language(&mut self, language: LanguageId) {
        self.language = language;
    }

    pub fn config(&self) -> ViewConfig {
        self.config
    }

    pub fn get_config(&self) -> ViewConfig {
        self.config
    }

    pub fn set_config(&mut self, config: Value) {
        self.config = ViewConfig::from_value(&config);
    }

    pub fn rev(&self) -> u64 {
        self.cache.read().expect("document cache lock poisoned").rev()
    }

    pub fn get_buf_size(&self) -> usize {
        self.cache.read().expect("document cache lock poisoned").len()
    }

    pub fn get_document(&self) -> Result<String, PluginError> {
        Ok(self.cache.read().expect("document cache lock poisoned").text().to_owned())
    }

    pub fn document_text(&self) -> Result<String, PluginError> {
        self.get_document()
    }

    pub fn get_selections(&self) -> Result<Vec<SelectionRange>, PluginError> {
        Ok(self.selections.read().expect("selection lock poisoned").clone())
    }

    pub fn set_selections(&self, selections: Vec<SelectionRange>) {
        *self.selections.write().expect("selection lock poisoned") = selections;
    }

    /// Apply one sequential document update, refetching when no delta is present.
    pub fn apply_update(&self, update: &PluginUpdate) -> Result<(), PluginError> {
        let mut cache = self.cache.write().expect("document cache lock poisoned");
        match cache.apply_update(update) {
            Ok(()) => Ok(()),
            Err(CacheError::NeedsSnapshot) => {
                drop(cache);
                self.refresh()
            }
            Err(error) => Err(PluginError::Schema { message: error.to_string() }),
        }
    }

    pub fn line_of_offset(&self, offset: usize) -> Result<usize, PluginError> {
        let cache = self.cache.read().expect("document cache lock poisoned");
        let text = cache.text();
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err(PluginError::Schema { message: format!("offset {offset} out of bounds") });
        }
        Ok(text[..offset].bytes().filter(|byte| *byte == b'\n').count())
    }

    pub fn offset_of_line(&self, line: usize) -> Result<usize, PluginError> {
        let cache = self.cache.read().expect("document cache lock poisoned");
        let text = cache.text();
        if line == 0 {
            return Ok(0);
        }
        let mut seen = 0;
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                seen += 1;
                if seen == line {
                    return Ok(index + 1);
                }
            }
        }
        Err(PluginError::Schema { message: format!("line {line} out of bounds") })
    }

    pub fn get_line(&self, line: usize) -> Result<String, PluginError> {
        let start = self.offset_of_line(line)?;
        let cache = self.cache.read().expect("document cache lock poisoned");
        let text = cache.text();
        let end = text[start..].find('\n').map(|offset| start + offset).unwrap_or(text.len());
        Ok(text[start..end].to_owned())
    }

    pub fn line_count(&self) -> usize {
        self.cache.read().expect("document cache lock poisoned").line_count()
    }

    pub fn syntax(&self) -> SyntaxClient {
        self.syntax.clone()
    }

    pub fn commands(&self) -> CommandsClient {
        self.commands.clone()
    }

    pub fn views(&self) -> ViewsClient {
        self.views.clone()
    }

    /// Apply LSP text edits through the revision-checked edit service.
    pub fn apply_text_edits(
        &self,
        edits: &[TextEdit],
        author: &str,
    ) -> Result<EditAck, PluginError> {
        let request = EditRequest {
            view_id: self.view_id,
            rev: self.rev(),
            edits: edits.to_vec(),
            priority: 0,
            after_cursor: false,
            undo_group: None,
            author: author.to_owned(),
        };
        self.documents.apply(&request)
    }
}

fn selection_ranges(
    caller: &Arc<dyn ServiceCaller>,
    view: ViewId,
) -> Result<Vec<SelectionRange>, PluginError> {
    let result = caller.call_service(
        ee_plugin_api::ids::ServiceId::from_static("editor.selection@1"),
        "get",
        json!({ "view_id": view }),
    )?;
    match result {
        Value::Array(items) => {
            Ok(items.into_iter().filter_map(|item| serde_json::from_value(item).ok()).collect())
        }
        Value::Object(map) => {
            let selections = map.get("selections").cloned().unwrap_or(Value::Array(Vec::new()));
            serde_json::from_value(selections)
                .map_err(|error| PluginError::Schema { message: error.to_string() })
        }
        _ => Ok(Vec::new()),
    }
}
