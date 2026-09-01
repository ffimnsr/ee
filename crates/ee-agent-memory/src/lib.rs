//! Durable, workspace-scoped fact memory.
//!
//! Storage is disabled by default. Every persistent mutation additionally
//! requires [`MutationApproval::Approved`]. Recalled facts remain untrusted
//! data regardless of authority.

mod identity;
mod migrations;
mod model;
mod store;
mod validation;

pub use identity::{WorkspaceIdentity, WorkspaceRootSet};
pub use model::*;
pub use store::WorkspaceMemory;

/// Current export and fact schema version.
pub const SCHEMA_VERSION: u32 = 1;
