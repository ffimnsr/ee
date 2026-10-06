//! One config value with its source layer, shared by agent-env and MCP
//! env/header secret-reference resolution.
//!
//! An exact `secret://<name>` value is kept as raw text through parsing,
//! merging, schema generation, and display. It is resolved from the
//! host-bound secrets store only at use time (agent launch, MCP session
//! forward, MCP host start). User layers resolve unconditionally; `Ancestor`
//! (workspace) layers resolve only when the host-local workspace trust
//! decision allows it; system layers never resolve.

use super::discovery::ConfigLayerKind;

/// One configured value with config-layer provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigSecretValue {
    /// The config layer this value came from.
    pub(crate) layer: ConfigLayerKind,
    /// Raw text exactly as written in config: a literal, an exact
    /// `secret://<name>` reference, or for MCP headers a template embedding
    /// exactly one reference. Never resolved at merge time.
    pub(crate) raw: String,
}

impl ConfigSecretValue {
    pub(crate) fn new(layer: ConfigLayerKind, raw: impl Into<String>) -> Self {
        Self { layer, raw: raw.into() }
    }
}
