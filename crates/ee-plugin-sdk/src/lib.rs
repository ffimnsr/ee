//! Rust SDK for native ee plugins.
//!
//! The SDK owns helper types and caches. It does not depend on a host crate: the
//! transport loop is supplied by `ee-plugin-runtime`.

#![deny(unsafe_code)]

pub mod cache;
pub mod client;
pub mod command;
pub mod config;
pub mod run;
pub mod transport;
pub mod view;
#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use cache::{CacheError, DocumentCache};
pub use client::{
    CommandsClient, DocumentClient, DocumentSnapshot, EditorView, LspClient, LspRequest,
    ServiceClient, SyntaxClient, TasksClient, UiClient, ViewConfig, Viewport, ViewsClient,
};
pub use client::{Connection, RuntimeTransport, SdkHost};
pub use command::{CommandResponse, ServiceCall, ServiceCallResult};
pub use config::PluginConfig;
pub use ee_plugin_api::contribution::Contribution;
pub use ee_plugin_api::effect::CommandRequest;
pub use ee_plugin_api::effect::{CommandHandler, CommandSpec, Effect};
pub use ee_plugin_api::model::ViewId;
pub use ee_plugin_api::{
    CommandId, Disposer, Plugin, PluginContext, PluginError, PluginId, ServiceId,
};
pub use run::{run, run_with_config};
pub use view::{SelectionError, ViewState};
#[cfg(target_arch = "wasm32")]
pub use wasm::{WasmPlugin, export_bytes, read_input};
