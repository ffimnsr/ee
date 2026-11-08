//! Host-neutral editor access used by plugin services and effect application.
//!
//! Services run on the core thread or on a native plugin thread. They never
//! borrow [`crate::tabs::CoreState`] directly; the production adapter locks the
//! core mutex with `try_lock` so a service call can never re-enter a core
//! borrow. Hosts that cannot answer fail closed with a typed error instead of
//! blocking.

use std::path::PathBuf;
use std::sync::Arc;

use ee_plugin_api::effect::{DecorationPatch, ScrollRequest, SelectionRequest, StatusItem};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::model::{
    BufferId, EditAck, EditRequest, LanguageId, SelectionRange, SymbolItem, ViewId,
};
use serde::Serialize;

/// Immutable document metadata returned to plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentSnapshot {
    pub buffer_id: BufferId,
    pub rev: u64,
    pub len: usize,
    pub line_count: usize,
    pub path: Option<String>,
    pub language: LanguageId,
    pub vlf: bool,
}

/// Visible viewport for one view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Viewport {
    pub view_id: ViewId,
    pub first_line: usize,
    pub height: usize,
}

/// Fold region expressed in logical lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FoldRange {
    pub header_line: usize,
    pub body_start: usize,
    pub body_end: usize,
}

/// One syntax scope within a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyntaxScope {
    pub start: usize,
    pub end: usize,
    pub scope: String,
}

/// Read and mutation surface exposed to plugins.
///
/// Implementations must be cheap and must never hold a core borrow across a
/// plugin callback.
pub trait EditorAccess: Send + Sync + 'static {
    fn document_snapshot(&self, buffer: BufferId) -> Result<DocumentSnapshot, PluginError>;

    fn read_range(&self, buffer: BufferId, start: usize, end: usize)
    -> Result<String, PluginError>;

    fn read_document(&self, buffer: BufferId, max_bytes: usize) -> Result<String, PluginError>;

    fn apply_edit(&self, request: &EditRequest) -> Result<EditAck, PluginError>;

    fn views_for_buffer(&self, buffer: BufferId) -> Vec<ViewId>;

    fn active_view(&self) -> Option<ViewId>;

    fn viewport(&self, view: ViewId) -> Result<Viewport, PluginError>;

    fn view_buffer(&self, view: ViewId) -> Option<BufferId>;

    fn view_language(&self, view: ViewId) -> Option<LanguageId>;

    fn selection(&self, view: ViewId) -> Result<Vec<SelectionRange>, PluginError>;

    fn apply_selection(&self, request: &SelectionRequest) -> Result<(), PluginError>;

    fn scroll_to(&self, request: &ScrollRequest) -> Result<(), PluginError>;

    fn reveal(&self, view: ViewId, line: usize, column: usize) -> Result<(), PluginError>;

    fn folds(&self, view: ViewId) -> Result<Vec<FoldRange>, PluginError>;

    fn symbols(&self, view: ViewId) -> Result<Vec<SymbolItem>, PluginError>;

    fn scopes(&self, view: ViewId) -> Result<Vec<SyntaxScope>, PluginError>;

    fn grammar_available(&self, language: &str) -> bool;

    fn workspace_root(&self) -> Option<PathBuf>;

    fn is_vlf(&self, view: ViewId) -> bool;
}

/// Mutation surface used when the host applies effects after the core borrow
/// ends. This trait deliberately has no `Send`/`Sync` bound: the editor core is
/// single-threaded and applies effects on the core thread.
pub trait EffectSink {
    fn effect_apply_edit(&mut self, request: &EditRequest) -> Result<EditAck, PluginError>;
    fn effect_set_selection(&mut self, request: &SelectionRequest) -> Result<(), PluginError>;
    fn effect_scroll_to(&mut self, request: &ScrollRequest) -> Result<(), PluginError>;
    fn effect_set_status(
        &mut self,
        owner: Option<&PluginId>,
        item: &StatusItem,
    ) -> Result<(), PluginError>;
    fn effect_remove_status(
        &mut self,
        owner: Option<&PluginId>,
        key: &str,
    ) -> Result<(), PluginError>;
    fn effect_set_decorations(
        &mut self,
        owner: Option<&PluginId>,
        patch: &DecorationPatch,
    ) -> Result<(), PluginError>;
    fn effect_emit_agent_tool_result(
        &mut self,
        call_id: &str,
        is_error: bool,
        content: &serde_json::Value,
    ) -> Result<(), PluginError>;
}

/// Shared access handle installed in every editor service.
pub type SharedEditorAccess = Arc<dyn EditorAccess>;
