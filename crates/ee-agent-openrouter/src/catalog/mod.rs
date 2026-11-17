//! Per-model OpenRouter catalog.
//!
//! The generated table in [`generated`] is metadata, not routing: OpenRouter
//! serves every model through the same base URL, so a catalog row can never
//! change the endpoint, the credential, or the selected protocol on its own.
//! Rows supply the values the request path needs — limits, modalities, reasoning
//! support, cost, and the routes the model advertises upstream — so a known
//! model is configured from data instead of from name sniffing.
//!
//! Unknown ids keep working with the caller's explicit configuration (custom
//! models, private deployments); only catalog rows can be listed by
//! `--list-models`. Provenance and regeneration live in `catalog/README.md`.

pub(crate) mod generated;

use std::fmt;

use ee_agent_orchestrator::{
    Modalities, ModelCost, ModelKind, ModelLimits, ModelRoutes, ReasoningProtocol,
    ReasoningSupport, ReasoningVariant, SamplingSupport, reasoning_variants,
};

/// One catalog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogModel {
    /// Exact OpenRouter model id (`vendor/model`).
    pub id: &'static str,
    /// What the entry produces.
    pub kind: ModelKind,
    /// Protocol routes the model advertises upstream.
    pub routes: ModelRoutes,
    /// Upstream status (`""` when active, `"deprecated"`, ...).
    pub status: &'static str,
    /// Upstream model family slug.
    pub family: &'static str,
    /// Base URL the catalog declares; empty for non-chat rows.
    pub base_url: &'static str,
    /// Context and output limits, when both are declared.
    pub limits: Option<ModelLimits>,
    /// Declared input/output modalities.
    pub modalities: Modalities,
    /// Declared reasoning controls.
    pub reasoning: ReasoningSupport,
    /// Declared text prices.
    pub cost: Option<ModelCost>,
    /// Whether the model advertises structured output.
    pub structured_output: bool,
    /// Whether the model supports tool calling.
    pub tools_supported: bool,
    /// Whether tool schemas accept `strict`.
    pub tools_strict: bool,
    /// Free-form tool input formats the model advertises.
    pub freeform: &'static [&'static str],
    /// Sampling knobs the model documents; an absent upstream flag means the
    /// protocol default applies, so the knob is accepted.
    pub sampling: SamplingSupport,
    /// Chat Completions output-token field the model documents; empty means
    /// unset.
    pub max_tokens_field: &'static str,
    /// Chat Completions assistant field carrying reasoning, when declared.
    pub reasoning_field: Option<&'static str>,
}

impl CatalogModel {
    /// Whether the entry can serve a chat session.
    #[must_use]
    pub const fn is_chat(&self) -> bool {
        matches!(self.kind, ModelKind::Chat)
    }

    /// Selectable reasoning variants for one protocol.
    #[must_use]
    pub fn variants(&self, protocol: ReasoningProtocol) -> Vec<ReasoningVariant> {
        reasoning_variants(self.reasoning, protocol, self.limits.map(|limits| limits.output))
    }
}

impl fmt::Display for CatalogModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.id, self.kind.as_str())
    }
}

/// Every catalog row, sorted by id.
pub fn models() -> impl Iterator<Item = &'static CatalogModel> {
    generated::MODELS.iter()
}

/// Finds one exact model id.
#[must_use]
pub fn find(model_id: &str) -> Option<&'static CatalogModel> {
    generated::MODELS.iter().find(|entry| entry.id == model_id)
}

/// Upstream source URL the generated table came from.
#[must_use]
pub fn snapshot_source() -> &'static str {
    generated::SNAPSHOT_SOURCE
}

/// SHA-256 of the committed models.dev subset the table was generated from.
#[must_use]
pub fn snapshot_sha256() -> &'static str {
    generated::SNAPSHOT_SHA256
}

#[cfg(test)]
mod tests;
