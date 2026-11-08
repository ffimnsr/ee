//! Editor host services registered as `editor.*` services.

pub mod config;
pub mod decorations;
pub mod document;
pub mod fs;
pub mod lsp;
pub mod router;
pub mod selection;
pub mod syntax;
pub mod tasks;
pub mod ui;
pub mod views;

/// Stable service identifiers.
pub mod ids {
    pub const DOCUMENT: &str = "editor.document@1";
    pub const DOCUMENT_EDIT: &str = "editor.document.edit@1";
    pub const VIEWS: &str = "editor.views@1";
    pub const SELECTION: &str = "editor.selection@1";
    pub const COMMANDS: &str = "editor.commands@1";
    pub const SYNTAX: &str = "editor.syntax@1";
    pub const LSP: &str = "editor.lsp@1";
    pub const DECORATIONS: &str = "editor.decorations@1";
    pub const UI: &str = "editor.ui@1";
    pub const TASKS: &str = "editor.tasks@1";
    pub const FS: &str = "editor.fs@1";
    pub const CONFIG: &str = "editor.config@1";
}

pub use config::ConfigService;
pub use decorations::DecorationService;
pub use document::{DocumentEditService, DocumentService};
pub use fs::FsService;
pub use lsp::LspService;
pub use selection::SelectionService;
pub use syntax::SyntaxService;
pub use tasks::TaskService;
pub use ui::UiService;
pub use views::ViewsService;
