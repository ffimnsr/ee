//! Per-model OpenCode catalog.
//!
//! The generated table in [`generated`] is the only routing truth for
//! `(surface, model)` pairs. Each row carries the protocol dialect, the trusted
//! surface base URL, catalog limits, reasoning support, cost, and capability
//! flags, so callers never substitute constants for model metadata.
//!
//! Regeneration and provenance live in `catalog/README.md` (kept with the
//! crate but untracked by the repository's `*.md` ignore rule); the committed
//! models.dev subset and the curated retired/unsupported/legacy inputs live next
//! to it.
//!
//! # Base-root probe
//!
//! On 2026-11-16 both documented root families answered live requests, so ee
//! keeps the `/zen` roots this crate's release gate verifies:
//!
//! - `GET /zen/v1/models`, `/inference/openai/v1/models`,
//!   `/inference/anthropic/v1/models`, `/zen/go/v1/models`, and
//!   `/inference/go/openai/v1/models` all returned `200`.
//! - `POST {}` to `/zen/v1/{responses,messages,chat/completions}`,
//!   `/inference/openai/v1/{responses,chat/completions}`, and
//!   `/inference/anthropic/v1/messages` all returned `400` from the dialect
//!   handlers, proving the paths are served.
//!
//! A future release that serves only one family should switch to that family and
//! regenerate the table; routing stays exact-match against generated HTTPS
//! literals either way.

pub(crate) mod generated;

use std::fmt;

use crate::routes::{OpenCodeDialect, OpenCodeSurface, UnsupportedDialect};

/// What a catalog entry produces. Only chat models are selectable by a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Text chat model routable through one of the three implemented dialects.
    Chat,
    /// Typed decision model (System One).
    Decision,
    /// Image generation model.
    Image,
}

/// Reasoning controls a model documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningSupport {
    /// Whether the model accepts any reasoning control.
    pub supported: bool,
    /// Whether a plain on/off toggle exists.
    pub toggle: bool,
    /// Effort values the model accepts, in catalog order.
    pub efforts: &'static [&'static str],
    /// Minimum thinking budget in tokens, when budgets are supported.
    pub budget_min: Option<u64>,
    /// Maximum thinking budget in tokens, when budgets are supported.
    pub budget_max: Option<u64>,
}

impl ReasoningSupport {
    /// No reasoning control is available or known.
    pub const UNSUPPORTED: Self =
        Self { supported: false, toggle: false, efforts: &[], budget_min: None, budget_max: None };
}

/// Monetary cost per million tokens, in integer micro-dollars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCost {
    /// Input price per million tokens.
    pub input_micros: u64,
    /// Output price per million tokens.
    pub output_micros: u64,
    /// Cached-input read price per million tokens, when priced separately.
    pub cache_read_micros: Option<u64>,
    /// Cache-write price per million tokens, when priced separately.
    pub cache_write_micros: Option<u64>,
}

/// Input/output modalities a model documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modalities {
    /// Accepted input MIME types (for example `text`, `image/png`, `application/pdf`).
    pub input: &'static [&'static str],
    /// Produced output MIME types.
    pub output: &'static [&'static str],
}

impl Modalities {
    /// No modality metadata (legacy rows).
    pub const UNKNOWN: Self = Self { input: &[], output: &[] };
}

/// One routable `(surface, model)` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogModel {
    /// Service surface that serves the model.
    pub surface: OpenCodeSurface,
    /// Catalog id used for lookups and diagnostics.
    pub id: &'static str,
    /// Model id sent on the wire; equal to `id` unless the catalog translates it.
    pub api_id: &'static str,
    /// What the entry produces.
    pub kind: ModelKind,
    /// Protocol dialect the model speaks on this surface.
    pub dialect: OpenCodeDialect,
    /// Upstream-declared model family slug; empty for legacy rows.
    pub family: &'static str,
    /// Trusted base URL for this surface; paths are appended by the route.
    pub base_url: &'static str,
    /// Upstream provider package id (`@opencode/ai/providers/...`); empty for legacy rows.
    pub sdk: &'static str,
    /// Catalog limits; `None` for legacy rows retained from the docs-era table.
    pub limits: Option<ee_agent_orchestrator::ModelLimits>,
    /// Documented input/output modalities.
    pub modalities: Modalities,
    /// Reasoning controls the model documents.
    pub reasoning: ReasoningSupport,
    /// Catalog prices, when known.
    pub cost: Option<ModelCost>,
    /// Whether the model advertises structured output.
    pub structured_output: bool,
    /// Whether the model supports tool calling.
    pub tools_supported: bool,
    /// Whether tool schemas accept `strict`.
    pub tools_strict: bool,
    /// Free-form tool input formats the model advertises.
    pub freeform: &'static [&'static str],
    /// Chat Completions output-token field the model documents; empty means unset.
    pub max_tokens_field: &'static str,
    /// Chat Completions assistant field carrying reasoning, when declared.
    pub reasoning_field: Option<&'static str>,
    /// Whether this row was retained from the pre-catalog table because the
    /// upstream snapshot no longer lists it.
    pub legacy: bool,
}

impl CatalogModel {
    /// Trusted endpoint for this entry: catalog base URL plus dialect path.
    #[must_use]
    pub fn endpoint(&self) -> &'static str {
        crate::routes::endpoint_for(self.surface, self.dialect)
    }
}

impl fmt::Display for CatalogModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {} ({})", self.surface.as_str(), self.id, self.dialect.as_str())
    }
}

/// One model id documented as retired on its surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredModel {
    /// Surface that retired the id.
    pub surface: OpenCodeSurface,
    /// Retired catalog id.
    pub model_id: &'static str,
    /// Upstream published deprecation date.
    pub deprecated_on: &'static str,
}

/// One model id whose protocol class has no codec yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedModel {
    /// Surface that documents the id.
    pub surface: OpenCodeSurface,
    /// Documented catalog id.
    pub model_id: &'static str,
    /// Protocol class without a codec.
    pub dialect: UnsupportedDialect,
}

/// Every routable model entry: snapshot rows first, then legacy rows.
pub(crate) fn models() -> impl Iterator<Item = &'static CatalogModel> {
    generated::MODELS.iter().chain(generated::LEGACY.iter())
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

/// Finds one exact `(surface, model)` entry.
pub(crate) fn find(surface: OpenCodeSurface, model_id: &str) -> Option<&'static CatalogModel> {
    models().find(|entry| entry.surface == surface && entry.id == model_id)
}

/// Finds the first entry for `model_id` on a different surface.
pub(crate) fn find_other_surface(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Option<&'static CatalogModel> {
    models().find(|entry| entry.surface != surface && entry.id == model_id)
}

/// Finds one retired id on its surface.
pub(crate) fn retired(surface: OpenCodeSurface, model_id: &str) -> Option<&'static RetiredModel> {
    generated::RETIRED.iter().find(|entry| entry.surface == surface && entry.model_id == model_id)
}

/// Finds one unsupported id on its surface.
pub(crate) fn unsupported(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Option<&'static UnsupportedModel> {
    generated::UNSUPPORTED
        .iter()
        .find(|entry| entry.surface == surface && entry.model_id == model_id)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_generated_entry_is_unique_and_carries_limits() {
        let mut seen = BTreeSet::new();
        for entry in models() {
            assert!(seen.insert((entry.surface.as_str(), entry.id)), "duplicate entry {entry}");
            if entry.legacy {
                continue;
            }
            let limits = entry.limits.unwrap_or_else(|| panic!("{entry} has no limits"));
            assert!(limits.context > 0, "{entry} context");
            assert!(limits.output > 0, "{entry} output");
            assert_eq!(entry.kind, ModelKind::Chat);
        }
    }

    #[test]
    fn generated_endpoints_are_trusted_surface_origins() {
        for entry in models() {
            let endpoint = entry.endpoint();
            assert!(endpoint.starts_with("https://opencode.ai/"), "{endpoint}");
            assert_eq!(entry.base_url, entry.surface.api_root(), "{entry}");
            assert!(endpoint.starts_with(entry.base_url), "{entry}");
            assert!(endpoint.ends_with(entry.dialect.path()), "{entry}");
        }
    }

    #[test]
    fn retired_and_unsupported_ids_are_never_routable() {
        for entry in generated::RETIRED {
            assert!(find(entry.surface, entry.model_id).is_none(), "{}", entry.model_id);
        }
        for entry in generated::UNSUPPORTED {
            assert!(find(entry.surface, entry.model_id).is_none(), "{}", entry.model_id);
        }
    }

    #[test]
    fn legacy_rows_are_explicit_and_have_no_invented_metadata() {
        let legacy: Vec<_> = models().filter(|entry| entry.legacy).collect();
        assert!(!legacy.is_empty());
        for entry in legacy {
            assert!(entry.limits.is_none(), "{entry}");
            assert!(entry.cost.is_none(), "{entry}");
            assert!(!entry.reasoning.supported, "{entry}");
        }
    }

    #[test]
    fn snapshot_digest_is_recorded() {
        assert_eq!(snapshot_sha256().len(), 64);
        assert!(snapshot_source().starts_with("https://"));
    }

    #[test]
    fn representative_models_carry_the_expected_dialect_and_metadata() {
        let gpt = find(OpenCodeSurface::Zen, "gpt-5.5").expect("gpt-5.5");
        assert_eq!(gpt.dialect, OpenCodeDialect::OpenAiResponses);
        assert!(gpt.limits.expect("limits").context > 0);
        assert!(gpt.limits.expect("limits").output > 0);
        assert!(gpt.reasoning.supported);

        let claude = find(OpenCodeSurface::Zen, "claude-sonnet-5").expect("claude-sonnet-5");
        assert_eq!(claude.dialect, OpenCodeDialect::AnthropicMessages);

        let pickle = find(OpenCodeSurface::Zen, "big-pickle").expect("big-pickle");
        assert_eq!(pickle.dialect, OpenCodeDialect::OpenAiChatCompletions);
    }
}
