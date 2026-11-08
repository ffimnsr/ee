//! In-memory editor used by plugin host and service tests.
//!
//! It implements the same access and effect boundaries as the real core so
//! plugin behavior can be tested without a renderer, RPC loop, or filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use ee_plugin_api::effect::{DecorationPatch, ScrollRequest, SelectionRequest, StatusItem};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::model::{
    BufferId, EditAck, EditRequest, LanguageId, SelectionRange, SymbolItem, ViewId,
};

use crate::plugin_runtime::core_api::{
    DocumentSnapshot, EditorAccess, EffectSink, FoldRange, SyntaxScope, Viewport,
};

#[derive(Debug, Clone)]
struct MemoryDocument {
    text: String,
    rev: u64,
    path: Option<String>,
    language: LanguageId,
    vlf: bool,
}

#[derive(Debug, Clone)]
struct MemoryView {
    buffer: BufferId,
    first_line: usize,
    height: usize,
    selection: Vec<SelectionRange>,
}

/// In-memory editor state.
pub struct MemoryEditor {
    documents: Mutex<BTreeMap<BufferId, MemoryDocument>>,
    views: Mutex<BTreeMap<ViewId, MemoryView>>,
    statuses: Mutex<Vec<(Option<PluginId>, StatusItem)>>,
    decorations: Mutex<Vec<(Option<PluginId>, DecorationPatch)>>,
    agent_results: Mutex<Vec<(String, bool, serde_json::Value)>>,
    folds: RwLock<Vec<FoldRange>>,
    symbols: RwLock<Vec<SymbolItem>>,
    scopes: RwLock<Vec<SyntaxScope>>,
    grammars: RwLock<BTreeSet<String>>,
    workspace_root: PathBuf,
    last_edit: Mutex<Option<EditRequest>>,
}

impl Default for MemoryEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryEditor {
    pub fn new() -> Self {
        Self {
            documents: Mutex::new(BTreeMap::new()),
            views: Mutex::new(BTreeMap::new()),
            statuses: Mutex::new(Vec::new()),
            decorations: Mutex::new(Vec::new()),
            agent_results: Mutex::new(Vec::new()),
            folds: RwLock::new(Vec::new()),
            symbols: RwLock::new(Vec::new()),
            scopes: RwLock::new(Vec::new()),
            grammars: RwLock::new(BTreeSet::new()),
            workspace_root: PathBuf::from("."),
            last_edit: Mutex::new(None),
        }
    }

    pub fn set_document(&self, buffer: BufferId, view: ViewId, text: &str) {
        self.documents.lock().expect("document lock poisoned").insert(
            buffer,
            MemoryDocument {
                text: text.to_owned(),
                rev: 0,
                path: None,
                language: LanguageId::new("text"),
                vlf: false,
            },
        );
        self.views
            .lock()
            .expect("view lock poisoned")
            .insert(view, MemoryView { buffer, first_line: 0, height: 10, selection: Vec::new() });
    }

    pub fn add_view(&self, buffer: BufferId, view: ViewId) {
        self.views
            .lock()
            .expect("view lock poisoned")
            .insert(view, MemoryView { buffer, first_line: 0, height: 10, selection: Vec::new() });
    }

    pub fn set_path(&self, buffer: BufferId, path: &str) {
        if let Some(document) =
            self.documents.lock().expect("document lock poisoned").get_mut(&buffer)
        {
            document.path = Some(path.to_owned());
        }
    }

    pub fn set_language(&self, view: ViewId, language: &str) {
        let buffer = {
            let mut views = self.views.lock().expect("view lock poisoned");
            let Some(view) = views.get_mut(&view) else {
                return;
            };
            view.buffer
        };
        if let Some(document) =
            self.documents.lock().expect("document lock poisoned").get_mut(&buffer)
        {
            document.language = LanguageId::new(language);
        }
    }

    pub fn set_rev(&self, buffer: BufferId, rev: u64) {
        if let Some(document) =
            self.documents.lock().expect("document lock poisoned").get_mut(&buffer)
        {
            document.rev = rev;
        }
    }

    pub fn set_vlf(&self, view: ViewId, vlf: bool) {
        let buffer =
            self.views.lock().expect("view lock poisoned").get(&view).map(|view| view.buffer);
        if let Some(buffer) = buffer
            && let Some(document) =
                self.documents.lock().expect("document lock poisoned").get_mut(&buffer)
        {
            document.vlf = vlf;
        }
    }

    pub fn set_folds(&self, folds: Vec<FoldRange>) {
        *self.folds.write().expect("fold lock poisoned") = folds;
    }

    pub fn set_symbols(&self, symbols: Vec<SymbolItem>) {
        *self.symbols.write().expect("symbol lock poisoned") = symbols;
    }

    pub fn set_scopes(&self, scopes: Vec<SyntaxScope>) {
        *self.scopes.write().expect("scope lock poisoned") = scopes;
    }

    pub fn add_grammar(&self, language: &str) {
        self.grammars.write().expect("grammar lock poisoned").insert(language.to_owned());
    }

    pub fn set_workspace_root(&mut self, root: impl Into<PathBuf>) {
        self.workspace_root = root.into();
    }

    pub fn statuses(&self) -> Vec<(Option<PluginId>, StatusItem)> {
        self.statuses.lock().expect("status lock poisoned").clone()
    }

    pub fn decorations(&self) -> Vec<(Option<PluginId>, DecorationPatch)> {
        self.decorations.lock().expect("decoration lock poisoned").clone()
    }

    pub fn agent_results(&self) -> Vec<(String, bool, serde_json::Value)> {
        self.agent_results.lock().expect("agent result lock poisoned").clone()
    }

    pub fn last_edit(&self) -> Option<EditRequest> {
        self.last_edit.lock().expect("edit lock poisoned").clone()
    }

    pub fn selection_for(&self, view: ViewId) -> Vec<SelectionRange> {
        self.views
            .lock()
            .expect("view lock poisoned")
            .get(&view)
            .map(|view| view.selection.clone())
            .unwrap_or_default()
    }

    fn text_len(&self, buffer: BufferId) -> Result<usize, PluginError> {
        self.documents
            .lock()
            .expect("document lock poisoned")
            .get(&buffer)
            .map(|document| document.text.len())
            .ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })
    }
}

impl EditorAccess for MemoryEditor {
    fn document_snapshot(&self, buffer: BufferId) -> Result<DocumentSnapshot, PluginError> {
        let documents = self.documents.lock().expect("document lock poisoned");
        let document = documents.get(&buffer).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer.0),
        })?;
        Ok(DocumentSnapshot {
            buffer_id: buffer,
            rev: document.rev,
            len: document.text.len(),
            line_count: document.text.lines().count().max(1),
            path: document.path.clone(),
            language: document.language.clone(),
            vlf: document.vlf,
        })
    }

    fn read_range(
        &self,
        buffer: BufferId,
        start: usize,
        end: usize,
    ) -> Result<String, PluginError> {
        let documents = self.documents.lock().expect("document lock poisoned");
        let document = documents.get(&buffer).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer.0),
        })?;
        if start > end
            || end > document.text.len()
            || !document.text.is_char_boundary(start)
            || !document.text.is_char_boundary(end)
        {
            return Err(PluginError::Schema { message: "invalid read range".to_owned() });
        }
        Ok(document.text[start..end].to_owned())
    }

    fn read_document(&self, buffer: BufferId, max_bytes: usize) -> Result<String, PluginError> {
        let documents = self.documents.lock().expect("document lock poisoned");
        let document = documents.get(&buffer).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer.0),
        })?;
        let mut end = max_bytes.min(document.text.len());
        while end > 0 && !document.text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(document.text[..end].to_owned())
    }

    fn apply_edit(&self, request: &EditRequest) -> Result<EditAck, PluginError> {
        let buffer = self.view_buffer(request.view_id).ok_or_else(|| PluginError::Schema {
            message: format!("view {} is not open", request.view_id.0),
        })?;
        let mut documents = self.documents.lock().expect("document lock poisoned");
        let document = documents.get_mut(&buffer).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer.0),
        })?;
        if document.vlf {
            return Err(PluginError::Permission { permission: "VLF edit".to_owned() });
        }
        if document.rev != request.rev {
            return Err(PluginError::Runtime {
                message: format!("stale revision {}", request.rev),
            });
        }
        let mut edits = request.edits.clone();
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in &edits {
            if edit.range.start > edit.range.end
                || edit.range.end > document.text.len()
                || !document.text.is_char_boundary(edit.range.start)
                || !document.text.is_char_boundary(edit.range.end)
            {
                return Err(PluginError::Schema { message: "invalid edit range".to_owned() });
            }
            document.text.replace_range(edit.range.start..edit.range.end, &edit.new_text);
        }
        *self.last_edit.lock().expect("edit lock poisoned") = Some(request.clone());
        document.rev += 1;
        Ok(EditAck { applied: true, rev: document.rev, reason: None })
    }

    fn views_for_buffer(&self, buffer: BufferId) -> Vec<ViewId> {
        self.views
            .lock()
            .expect("view lock poisoned")
            .iter()
            .filter(|(_, view)| view.buffer == buffer)
            .map(|(id, _)| *id)
            .collect()
    }

    fn active_view(&self) -> Option<ViewId> {
        self.views.lock().expect("view lock poisoned").keys().next().copied()
    }

    fn viewport(&self, view_id: ViewId) -> Result<Viewport, PluginError> {
        let views = self.views.lock().expect("view lock poisoned");
        let view = views
            .get(&view_id)
            .ok_or_else(|| PluginError::Schema { message: "view is not open".to_owned() })?;
        Ok(Viewport { view_id, first_line: view.first_line, height: view.height })
    }

    fn view_buffer(&self, view: ViewId) -> Option<BufferId> {
        self.views.lock().expect("view lock poisoned").get(&view).map(|view| view.buffer)
    }

    fn view_language(&self, view: ViewId) -> Option<LanguageId> {
        let buffer = self.view_buffer(view)?;
        self.documents
            .lock()
            .expect("document lock poisoned")
            .get(&buffer)
            .map(|document| document.language.clone())
    }

    fn selection(&self, view: ViewId) -> Result<Vec<SelectionRange>, PluginError> {
        let views = self.views.lock().expect("view lock poisoned");
        let view = views
            .get(&view)
            .ok_or_else(|| PluginError::Schema { message: "view is not open".to_owned() })?;
        Ok(view.selection.clone())
    }

    fn apply_selection(&self, request: &SelectionRequest) -> Result<(), PluginError> {
        let mut views = self.views.lock().expect("view lock poisoned");
        let view = views
            .get_mut(&request.view_id)
            .ok_or_else(|| PluginError::Schema { message: "view is not open".to_owned() })?;
        let len = self.text_len(view.buffer)?;
        view.selection = request
            .selections
            .iter()
            .map(|range| SelectionRange { start: range.start.min(len), end: range.end.min(len) })
            .collect();
        Ok(())
    }

    fn scroll_to(&self, request: &ScrollRequest) -> Result<(), PluginError> {
        let mut views = self.views.lock().expect("view lock poisoned");
        let view = views
            .get_mut(&request.view_id)
            .ok_or_else(|| PluginError::Schema { message: "view is not open".to_owned() })?;
        view.first_line = request.line;
        Ok(())
    }

    fn reveal(&self, view: ViewId, line: usize, _column: usize) -> Result<(), PluginError> {
        let mut views = self.views.lock().expect("view lock poisoned");
        let view = views
            .get_mut(&view)
            .ok_or_else(|| PluginError::Schema { message: "view is not open".to_owned() })?;
        view.first_line = line;
        Ok(())
    }

    fn folds(&self, _view: ViewId) -> Result<Vec<FoldRange>, PluginError> {
        Ok(self.folds.read().expect("fold lock poisoned").clone())
    }

    fn symbols(&self, _view: ViewId) -> Result<Vec<SymbolItem>, PluginError> {
        Ok(self.symbols.read().expect("symbol lock poisoned").clone())
    }

    fn scopes(&self, _view: ViewId) -> Result<Vec<SyntaxScope>, PluginError> {
        Ok(self.scopes.read().expect("scope lock poisoned").clone())
    }

    fn grammar_available(&self, language: &str) -> bool {
        self.grammars.read().expect("grammar lock poisoned").contains(language)
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        Some(self.workspace_root.clone())
    }

    fn is_vlf(&self, view: ViewId) -> bool {
        let Some(buffer) = self.view_buffer(view) else {
            return false;
        };
        self.documents
            .lock()
            .expect("document lock poisoned")
            .get(&buffer)
            .is_some_and(|document| document.vlf)
    }
}

impl EffectSink for MemoryEditor {
    fn effect_apply_edit(&mut self, request: &EditRequest) -> Result<EditAck, PluginError> {
        self.apply_edit(request)
    }

    fn effect_set_selection(&mut self, request: &SelectionRequest) -> Result<(), PluginError> {
        self.apply_selection(request)
    }

    fn effect_scroll_to(&mut self, request: &ScrollRequest) -> Result<(), PluginError> {
        self.scroll_to(request)
    }

    fn effect_set_status(
        &mut self,
        owner: Option<&PluginId>,
        item: &StatusItem,
    ) -> Result<(), PluginError> {
        self.statuses.lock().expect("status lock poisoned").push((owner.cloned(), item.clone()));
        Ok(())
    }

    fn effect_remove_status(
        &mut self,
        owner: Option<&PluginId>,
        key: &str,
    ) -> Result<(), PluginError> {
        self.statuses.lock().expect("status lock poisoned").retain(|(entry_owner, item)| {
            !(entry_owner.as_ref() == owner && item.key.as_str() == key)
        });
        Ok(())
    }

    fn effect_set_decorations(
        &mut self,
        owner: Option<&PluginId>,
        patch: &DecorationPatch,
    ) -> Result<(), PluginError> {
        self.decorations
            .lock()
            .expect("decoration lock poisoned")
            .push((owner.cloned(), patch.clone()));
        Ok(())
    }

    fn effect_emit_agent_tool_result(
        &mut self,
        call_id: &str,
        is_error: bool,
        content: &serde_json::Value,
    ) -> Result<(), PluginError> {
        self.agent_results.lock().expect("agent result lock poisoned").push((
            call_id.to_owned(),
            is_error,
            content.clone(),
        ));
        Ok(())
    }
}
