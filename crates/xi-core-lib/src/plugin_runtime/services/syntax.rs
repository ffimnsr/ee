//! `editor.syntax` read service.
//!
//! Tree-sitter stays backend infrastructure; plugins only query scopes, folds,
//! symbols, and grammar availability.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::{SymbolItem, ViewId};
use ee_plugin_api::service::Service;

use crate::plugin_runtime::core_api::{FoldRange, SharedEditorAccess, SyntaxScope};

/// Maximum scopes returned by one query.
pub const MAX_SCOPES: usize = 20_000;
/// Maximum symbols returned by one query.
pub const MAX_SYMBOLS: usize = 4_096;

/// Read-only syntax queries.
#[derive(Clone)]
pub struct SyntaxService {
    access: SharedEditorAccess,
}

impl SyntaxService {
    pub fn new(access: SharedEditorAccess) -> Self {
        Self { access }
    }

    pub fn scopes(&self, view: ViewId) -> Result<Vec<SyntaxScope>, PluginError> {
        let mut scopes = self.access.scopes(view)?;
        scopes.truncate(MAX_SCOPES);
        Ok(scopes)
    }

    pub fn folds(&self, view: ViewId) -> Result<Vec<FoldRange>, PluginError> {
        self.access.folds(view)
    }

    pub fn symbols(&self, view: ViewId) -> Result<Vec<SymbolItem>, PluginError> {
        let mut symbols = self.access.symbols(view)?;
        symbols.truncate(MAX_SYMBOLS);
        Ok(symbols)
    }

    pub fn grammar_available(&self, language: &str) -> bool {
        self.access.grammar_available(language)
    }
}

impl Service for SyntaxService {
    const ID: &'static str = "editor.syntax@1";
}
