//! `editor.selection` service.

use ee_plugin_api::effect::SelectionRequest;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{SelectionRange, ViewId};
use ee_plugin_api::service::Service;

use crate::plugin_runtime::core_api::SharedEditorAccess;

/// Maximum selections accepted from one plugin request.
pub const MAX_SELECTIONS: usize = 10_000;

/// Selection read and write operations.
#[derive(Clone)]
pub struct SelectionService {
    access: SharedEditorAccess,
}

impl SelectionService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self { access }
    }

    pub fn get(&self, view: ViewId) -> Result<Vec<SelectionRange>, PluginError> {
        self.access.selection(view)
    }

    pub fn set(&self, request: SelectionRequest) -> Result<(), PluginError> {
        if request.selections.len() > MAX_SELECTIONS {
            return Err(PluginError::Resource {
                message: format!("selection count exceeds {MAX_SELECTIONS}"),
            });
        }
        if request.selections.iter().any(|range| range.start > range.end) {
            return Err(PluginError::Schema { message: "selection start exceeds end".to_owned() });
        }
        self.access.apply_selection(&request)
    }
}

impl Service for SelectionService {
    const ID: &'static str = "editor.selection@1";
}
