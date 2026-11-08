//! Cordis-style plugin host for the editor.
//!
//! The module is a sibling of `plugins/` and `event_context/` so core files
//! stay small. It owns discovery/activation, editor services, typed events,
//! declarative contributions, and effect application boundaries.

pub mod bridge;
pub mod commands;
pub mod contributions;
pub mod core_api;
pub mod effects;
pub mod events;
pub mod host;
pub mod services;

#[cfg(test)]
pub mod test_support;

#[cfg(test)]
mod tests;

pub use commands::EditorCommandRegistry;
pub use contributions::EditorContributionRegistry;
pub use core_api::{DocumentSnapshot, EditorAccess, EffectSink, SharedEditorAccess, Viewport};
pub use host::{EditorPluginHost, HostNotifier, QueuedEffect};
