//! Typed v2 service clients for native plugins.
//!
//! Every client is a thin, typed wrapper over [`ee_plugin_api::context::ServiceCaller`].
//! The SDK runtime owns the wire protocol; plugin code only sees these types.

mod commands;
mod document;
mod editor_view;
mod lsp;
mod runtime;
mod service;
mod syntax;
mod tasks;
mod ui;
mod views;

pub use commands::CommandsClient;
pub use document::{DocumentClient, DocumentSnapshot};
pub use editor_view::{EditorView, ViewConfig};
pub use ee_plugin_api::contribution::StatusAlignment;
pub use ee_plugin_api::effect::StatusItem;
pub use lsp::{LspClient, LspRequest};
pub use runtime::{Connection, RuntimeTransport, SdkHost, run_stdio};
pub use service::ServiceClient;
pub use syntax::SyntaxClient;
pub use tasks::TasksClient;
pub use ui::UiClient;
pub use views::{Viewport, ViewsClient};

#[cfg(test)]
mod tests;
