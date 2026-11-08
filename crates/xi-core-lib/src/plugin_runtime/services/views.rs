//! `editor.views` service.

use ee_plugin_api::effect::ScrollRequest;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{BufferId, LanguageId, ViewId};
use ee_plugin_api::service::Service;

use crate::plugin_runtime::core_api::{SharedEditorAccess, Viewport};

/// View lookup and scroll/reveal requests.
#[derive(Clone)]
pub struct ViewsService {
    access: SharedEditorAccess,
}

impl ViewsService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self { access }
    }

    pub fn views_for_buffer(&self, buffer: BufferId) -> Vec<ViewId> {
        self.access.views_for_buffer(buffer)
    }

    pub fn active_view(&self) -> Option<ViewId> {
        self.access.active_view()
    }

    pub fn viewport(&self, view: ViewId) -> Result<Viewport, PluginError> {
        self.access.viewport(view)
    }

    pub fn view_buffer(&self, view: ViewId) -> Option<BufferId> {
        self.access.view_buffer(view)
    }

    pub fn view_language(&self, view: ViewId) -> Option<LanguageId> {
        self.access.view_language(view)
    }

    /// Scroll is a request; the frontend owns final placement.
    pub fn scroll_to(&self, request: ScrollRequest) -> Result<(), PluginError> {
        self.access.scroll_to(&request)
    }

    /// Reveal a position without changing selection.
    pub fn reveal(&self, view: ViewId, line: usize, column: usize) -> Result<(), PluginError> {
        self.access.reveal(view, line, column)
    }
}

impl Service for ViewsService {
    const ID: &'static str = "editor.views@1";
}
