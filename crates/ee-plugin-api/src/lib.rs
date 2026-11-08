//! Stable contracts shared by the ee editor host, agent host, native plugins,
//! and Wasm plugins.
//!
//! This crate is intentionally host-agnostic: it must not depend on the editor,
//! frontend, agent, or LSP crates. Read services are plain trait objects;
//! mutations are expressed as [`effect::Effect`] values so plugin callbacks
//! never run inside a core borrow.

#![forbid(unsafe_code)]

pub mod context;
pub mod contribution;
pub mod disposer;
pub mod effect;
pub mod error;
pub mod event;
pub mod host;
pub mod ids;
pub mod manifest;
pub mod model;
pub mod service;
pub mod wire;

pub use context::{CommandRegistry, ContributionSink, Plugin, PluginContext};
pub use disposer::{Disposer, DisposerStack};
pub use error::{ErrorClass, PluginError};
pub use host::{HostAdapter, HostKind};
pub use ids::{CommandId, ContributionId, IdError, PluginId, PluginInstanceId, ScopeId, ServiceId};
pub use manifest::{API_VERSION, MANIFEST_VERSION, ManifestV2, manifest_schema};
pub use model::model_schema;
pub use service::{Service, ServiceKey, ServiceRegistry};
