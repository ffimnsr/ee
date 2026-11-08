//! Host-agnostic plugin runtime.
//!
//! This crate owns discovery, manifest validation, configuration layering,
//! service resolution, activation, native transport, health, and lifecycle. It
//! deliberately has no dependency on the editor, frontend, agent, or LSP
//! crates.

#![forbid(unsafe_code)]

pub mod activation;
pub mod builtin;
pub mod config;
pub mod contributions;
pub mod diagnostics;
pub mod discovery;
pub mod health;
pub mod host_impl;
pub mod manager;
pub mod native;
pub mod resolver;

pub use activation::{ActivationTrigger, ScopeLifetime, activates_at_startup, should_activate};
pub use builtin::{BuiltinCatalog, BuiltinDescriptor, builtin_manifest};
pub use config::{
    ConfigLayers, InterpolationContext, interpolate, resolve_plugin_config, validate_config,
};
pub use contributions::{ContributionSession, collect_manifest_contributions, contribution_kind};
pub use diagnostics::{Diagnostic, DiagnosticPhase, MAX_DIAGNOSTIC_MESSAGE};
pub use discovery::{
    DiscoveredPlugin, DiscoveryOptions, DiscoveryResult, PluginRoot, RootKind, discover,
    origin_hash,
};
pub use health::{HealthPolicy, HealthReason, HealthSnapshot, HealthState, HealthTracker};
pub use host_impl::{MemoryCommandRegistry, MemoryContributionSink};
pub use manager::{ManagerStatus, PluginManager, PluginManagerConfig, PluginStatus};
pub use native::{
    INITIALIZE_REQUEST_ID, MAX_FRAME_BYTES, MAX_STDERR_BYTES, NativeClient, NativeSpec,
};
