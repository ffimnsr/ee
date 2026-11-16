//! Exact OpenCode surface/model routing catalog.
//!
//! OpenCode publishes two independent service surfaces whose model ids overlap
//! while their endpoints and protocol dialects differ, so a model id alone never
//! identifies a request shape:
//!
//! - Zen: `https://opencode.ai/zen/v1`
//! - Go: `https://opencode.ai/zen/go/v1`
//!
//! Both roots serve three dialects: OpenAI Responses (`/responses`), Anthropic
//! Messages (`/messages`), and OpenAI-compatible Chat Completions
//! (`/chat/completions`). The catalog in this module is the only routing truth:
//! one exact entry per currently documented model, matched by string equality.
//! There is no prefix match, wildcard, dialect probing, or cross-surface
//! fallback, so [`resolve_route`] fails closed before any HTTP client, request
//! body, or `Authorization` header exists.
//!
//! Provenance: the models.dev catalog snapshot for `opencode` and
//! `opencode-go` (see `crates/ee-agent-opencode/catalog/README.md`), plus the
//! curated retired/unsupported ids in `catalog/sources.json`. Every routable
//! row carries limits, reasoning support, and cost. The base URL stays a
//! trusted surface root, so no catalog or environment value can introduce a new
//! origin. Model classes EE implements no codec for are listed as unsupported,
//! so they fail with explicit guidance rather than a generic unknown-model
//! error.
//!
//! Maintenance: regenerate with `python3 scripts/gen_catalog.py` from
//! `crates/ee-agent-opencode`, keep [`endpoint_for`] the only place an endpoint
//! is assembled, and keep this module's tests passing.

use crate::catalog::{self, CatalogModel, ReasoningVariant, find_variant, variant_ids};
use crate::config::ConfigError;

/// Trusted OpenCode Zen API root.
pub const ZEN_API_ROOT: &str = "https://opencode.ai/zen/v1";
/// Trusted OpenCode Go API root.
pub const GO_API_ROOT: &str = "https://opencode.ai/zen/go/v1";

/// OpenCode service surface a model is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenCodeSurface {
    /// OpenCode Zen (pay-as-you-go).
    Zen,
    /// OpenCode Go (subscription).
    Go,
}

impl OpenCodeSurface {
    /// Canonical surface name used in configuration and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zen => "zen",
            Self::Go => "go",
        }
    }

    /// Trusted API root for this surface; the only source of OpenCode origins.
    #[must_use]
    pub const fn api_root(self) -> &'static str {
        match self {
            Self::Zen => ZEN_API_ROOT,
            Self::Go => GO_API_ROOT,
        }
    }

    /// Parses an exact surface name.
    ///
    /// Matching is case sensitive and alias free: `zen` and `go` only. EE never
    /// selects a surface implicitly, so an unrecognized value is an error rather
    /// than a fallback.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnsupportedSurface`] for any other value.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value {
            "zen" => Ok(Self::Zen),
            "go" => Ok(Self::Go),
            _ => Err(ConfigError::UnsupportedSurface { value: value.to_string() }),
        }
    }
}

/// Wire dialect a model speaks on its surface.
///
/// The three dialects are distinct request, response, and streaming codecs.
/// Routing never infers a dialect from a model name and never retries a request
/// through another dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeDialect {
    /// OpenAI Responses (`@ai-sdk/openai` models).
    OpenAiResponses,
    /// Anthropic Messages (`@ai-sdk/anthropic` models).
    AnthropicMessages,
    /// OpenAI-compatible Chat Completions (`@ai-sdk/openai-compatible` models).
    OpenAiChatCompletions,
}

impl OpenCodeDialect {
    /// Stable dialect name used in diagnostics and catalog output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiChatCompletions => "openai_chat_completions",
        }
    }

    /// Endpoint path appended to a surface root.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "/responses",
            Self::AnthropicMessages => "/messages",
            Self::OpenAiChatCompletions => "/chat/completions",
        }
    }
}

/// Documented model class that EE implements no codec for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedDialect {
    /// Google Generative Language models (`@ai-sdk/google` entries).
    GoogleGenerativeLanguage,
    /// TypeSafe AI System One models served from `/systemone`.
    SystemOne,
}

impl UnsupportedDialect {
    /// Human-readable class name used in error guidance.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::GoogleGenerativeLanguage => "Google Generative Language",
            Self::SystemOne => "System One",
        }
    }
}

/// One resolved `(surface, model)` route.
///
/// The endpoint is one of the trusted constants owned by [`endpoint_for`], and
/// `model_id` is the catalog id rather than the caller-supplied string, so a
/// route that exists can never carry an arbitrary origin or an unverified model
/// id. `variant` is the reasoning variant selected with `model#variant`, already
/// validated against the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCodeRoute {
    /// Service surface that serves the model.
    pub surface: OpenCodeSurface,
    /// Exact documented model id from the catalog.
    pub model_id: &'static str,
    /// Protocol dialect this model speaks on this surface.
    pub dialect: OpenCodeDialect,
    /// Trusted endpoint for the surface/dialect pair.
    pub endpoint: &'static str,
    /// Catalog row this route resolved from.
    pub entry: &'static CatalogModel,
    /// Selected reasoning variant, when the caller asked for one.
    pub variant: Option<ReasoningVariant>,
}

impl OpenCodeRoute {
    /// Catalog-declared context/output limits, when known.
    #[must_use]
    pub const fn limits(&self) -> Option<ee_agent_orchestrator::ModelLimits> {
        self.entry.limits
    }

    /// Catalog-declared reasoning support.
    #[must_use]
    pub const fn reasoning(&self) -> crate::catalog::ReasoningSupport {
        self.entry.reasoning
    }

    /// Catalog-declared prices, when known.
    #[must_use]
    pub const fn cost(&self) -> Option<crate::catalog::ModelCost> {
        self.entry.cost
    }

    /// Wire model id; equals the catalog id unless the row translates it.
    #[must_use]
    pub const fn api_model_id(&self) -> &'static str {
        self.entry.api_id
    }

    /// Whether this row is a legacy route without catalog metadata.
    #[must_use]
    pub const fn is_legacy(&self) -> bool {
        self.entry.legacy
    }

    /// Catalog-declared Chat Completions output-token field, when known.
    #[must_use]
    pub const fn max_tokens_field(&self) -> Option<&'static str> {
        if self.entry.max_tokens_field.is_empty() {
            None
        } else {
            Some(self.entry.max_tokens_field)
        }
    }

    /// Every reasoning variant this route exposes.
    #[must_use]
    pub fn variants(&self) -> Vec<ReasoningVariant> {
        self.entry.variants()
    }
}

/// Returns the trusted endpoint for one surface/dialect pair.
///
/// Every URL is an HTTPS literal under a surface root. EE never accepts an
/// endpoint from the environment or from model metadata, so an OpenCode
/// credential cannot be redirected to an arbitrary origin.
#[must_use]
pub const fn endpoint_for(surface: OpenCodeSurface, dialect: OpenCodeDialect) -> &'static str {
    match (surface, dialect) {
        (OpenCodeSurface::Zen, OpenCodeDialect::OpenAiResponses) => {
            "https://opencode.ai/zen/v1/responses"
        }
        (OpenCodeSurface::Zen, OpenCodeDialect::AnthropicMessages) => {
            "https://opencode.ai/zen/v1/messages"
        }
        (OpenCodeSurface::Zen, OpenCodeDialect::OpenAiChatCompletions) => {
            "https://opencode.ai/zen/v1/chat/completions"
        }
        (OpenCodeSurface::Go, OpenCodeDialect::OpenAiResponses) => {
            "https://opencode.ai/zen/go/v1/responses"
        }
        (OpenCodeSurface::Go, OpenCodeDialect::AnthropicMessages) => {
            "https://opencode.ai/zen/go/v1/messages"
        }
        (OpenCodeSurface::Go, OpenCodeDialect::OpenAiChatCompletions) => {
            "https://opencode.ai/zen/go/v1/chat/completions"
        }
    }
}

/// Every routable `(surface, model)` entry.
///
/// Ordering follows the generated table: surface, then catalog id.
pub fn catalog() -> impl Iterator<Item = OpenCodeRoute> {
    catalog::models().map(|entry| route_for(entry, None))
}

/// Resolves the exact route for one `(surface, model)` pair.
///
/// `model_id` is matched by exact string equality against the catalog after
/// trimming surrounding whitespace. A trailing `#variant` selects one of the
/// route's catalog-derived reasoning variants; an unknown variant fails at
/// startup with the valid ids listed. Unknown, cross-surface, retired, and
/// unsupported-dialect ids return a [`ConfigError`] without side effects: no
/// request and no credential header is ever built from a rejected id.
///
/// # Errors
///
/// Returns [`ConfigError::MissingModel`] for an empty model id, and the other
/// [`ConfigError`] variants for rejected catalog lookups or variants.
pub fn resolve_route(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Result<OpenCodeRoute, ConfigError> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Err(ConfigError::MissingModel);
    }
    let (base_id, variant_id) = split_variant(model_id)?;
    if let Some(entry) = catalog_entry(surface, base_id) {
        let variant = match variant_id {
            Some(variant_id) => Some(resolve_variant(entry, variant_id)?),
            None => None,
        };
        return Ok(route_for(entry, variant));
    }
    if let Some(bare_model_id) = tui_alias_target(base_id)
        && catalog_entry(surface, bare_model_id).is_some()
    {
        return Err(ConfigError::TuiAliasModel {
            model_id: model_id.to_string(),
            bare_model_id: bare_model_id.to_string(),
        });
    }
    if let Some(other) = other_surface_entry(surface, base_id) {
        return Err(ConfigError::WrongSurface {
            surface,
            model_id: base_id.to_string(),
            documented_surface: other.surface,
        });
    }
    if let Some(retired) = retired_entry(surface, base_id) {
        return Err(ConfigError::RetiredModel {
            surface,
            model_id: base_id.to_string(),
            deprecated_on: retired.deprecated_on,
        });
    }
    if let Some(unsupported) = unsupported_entry(surface, base_id) {
        return Err(ConfigError::UnsupportedModelDialect {
            surface,
            model_id: base_id.to_string(),
            dialect: unsupported.dialect,
        });
    }
    Err(ConfigError::UnknownModel { surface, model_id: base_id.to_string() })
}

/// Splits `model#variant` after the last `#`.
fn split_variant(model_id: &str) -> Result<(&str, Option<&str>), ConfigError> {
    let Some((base, variant)) = model_id.rsplit_once('#') else {
        return Ok((model_id, None));
    };
    let base = base.trim();
    let variant = variant.trim();
    if base.is_empty() {
        return Err(ConfigError::MissingModel);
    }
    if variant.is_empty() {
        return Err(ConfigError::MissingVariant { model_id: base.to_string() });
    }
    Ok((base, Some(variant)))
}

/// Validates one variant against the catalog-derived list.
fn resolve_variant(
    entry: &'static CatalogModel,
    variant_id: &str,
) -> Result<ReasoningVariant, ConfigError> {
    let variants = entry.variants();
    find_variant(&variants, variant_id).copied().ok_or_else(|| ConfigError::UnknownVariant {
        model_id: entry.id.to_string(),
        variant: variant_id.to_string(),
        valid: variant_ids(&variants),
    })
}

// Catalog rows live in `crate::catalog`; this module keeps exact resolution,
// trusted endpoint assembly, and the retired/unsupported guidance.

/// TUI-only provider alias prefixes documented by OpenCode for its config files.
///
/// EE routes documented model ids only; these prefixes exist so a copied TUI
/// value fails with the bare id to use instead.
const TUI_ALIAS_PREFIXES: &[&str] = &["opencode-go/", "opencode/"];

fn route_for(entry: &'static CatalogModel, variant: Option<ReasoningVariant>) -> OpenCodeRoute {
    OpenCodeRoute {
        surface: entry.surface,
        model_id: entry.id,
        dialect: entry.dialect,
        endpoint: endpoint_for(entry.surface, entry.dialect),
        entry,
        variant,
    }
}

fn catalog_entry(surface: OpenCodeSurface, model_id: &str) -> Option<&'static CatalogModel> {
    catalog::find(surface, model_id)
}

fn other_surface_entry(surface: OpenCodeSurface, model_id: &str) -> Option<&'static CatalogModel> {
    catalog::find_other_surface(surface, model_id)
}

fn retired_entry(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Option<&'static crate::catalog::RetiredModel> {
    catalog::retired(surface, model_id)
}

fn unsupported_entry(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Option<&'static crate::catalog::UnsupportedModel> {
    catalog::unsupported(surface, model_id)
}

fn tui_alias_target(model_id: &str) -> Option<&str> {
    TUI_ALIAS_PREFIXES
        .iter()
        .find_map(|prefix| model_id.strip_prefix(prefix))
        .filter(|bare_model_id| !bare_model_id.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::routes::OpenCodeDialect::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiResponses,
    };
    use crate::routes::OpenCodeSurface::{Go, Zen};

    fn route(surface: OpenCodeSurface, model_id: &str) -> OpenCodeRoute {
        resolve_route(surface, model_id).expect("documented route")
    }

    #[test]
    fn surface_names_roots_and_paths_are_documented_values() {
        assert_eq!(Zen.as_str(), "zen");
        assert_eq!(Go.as_str(), "go");
        assert_eq!(Zen.api_root(), "https://opencode.ai/zen/v1");
        assert_eq!(Go.api_root(), "https://opencode.ai/zen/go/v1");
        assert_eq!(OpenAiResponses.as_str(), "openai_responses");
        assert_eq!(AnthropicMessages.as_str(), "anthropic_messages");
        assert_eq!(OpenAiChatCompletions.as_str(), "openai_chat_completions");
        assert_eq!(OpenAiResponses.path(), "/responses");
        assert_eq!(AnthropicMessages.path(), "/messages");
        assert_eq!(OpenAiChatCompletions.path(), "/chat/completions");
        assert_eq!(
            UnsupportedDialect::GoogleGenerativeLanguage.label(),
            "Google Generative Language"
        );
        assert_eq!(UnsupportedDialect::SystemOne.label(), "System One");
    }

    #[test]
    fn shared_model_ids_keep_surface_specific_endpoints_and_dialects() {
        // `minimax-m3` is Chat Completions on Zen and Messages on Go.
        let zen = route(Zen, "minimax-m3");
        let go = route(Go, "minimax-m3");
        assert_eq!(zen.endpoint, "https://opencode.ai/zen/v1/chat/completions");
        assert_eq!(zen.dialect, OpenAiChatCompletions);
        assert_eq!(go.endpoint, "https://opencode.ai/zen/go/v1/messages");
        assert_eq!(go.dialect, AnthropicMessages);

        // `deepseek-v4.1-flash` speaks the same dialect on both surfaces, yet the
        // roots stay distinct: same id never means same origin.
        let zen = route(Zen, "deepseek-v4.1-flash");
        let go = route(Go, "deepseek-v4.1-flash");
        assert_eq!(zen.dialect, go.dialect);
        assert_ne!(zen.endpoint, go.endpoint);
        assert!(zen.endpoint.starts_with(ZEN_API_ROOT));
        assert!(go.endpoint.starts_with(GO_API_ROOT));
        assert!(!go.endpoint.starts_with(ZEN_API_ROOT));
    }

    #[test]
    fn one_model_per_dialect_resolves_to_exact_documented_endpoint() {
        assert_eq!(route(Zen, "gpt-5.5").endpoint, "https://opencode.ai/zen/v1/responses");
        assert_eq!(route(Zen, "claude-sonnet-5").endpoint, "https://opencode.ai/zen/v1/messages");
        assert_eq!(
            route(Zen, "big-pickle").endpoint,
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(route(Go, "grok-4.7").endpoint, "https://opencode.ai/zen/go/v1/responses");
        assert_eq!(route(Go, "qwen3.8-flash").endpoint, "https://opencode.ai/zen/go/v1/messages");
        assert_eq!(
            route(Go, "longcat-2.0").endpoint,
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_for_covers_every_surface_dialect_pair() {
        for (surface, root) in [(Zen, ZEN_API_ROOT), (Go, GO_API_ROOT)] {
            for dialect in [OpenAiResponses, AnthropicMessages, OpenAiChatCompletions] {
                let endpoint = endpoint_for(surface, dialect);
                assert_eq!(endpoint, format!("{root}{}", dialect.path()));
                assert!(endpoint.starts_with("https://opencode.ai/zen/"));
            }
        }
        assert_eq!(endpoint_for(Zen, OpenAiResponses), "https://opencode.ai/zen/v1/responses");
        assert_eq!(endpoint_for(Zen, AnthropicMessages), "https://opencode.ai/zen/v1/messages");
        assert_eq!(
            endpoint_for(Zen, OpenAiChatCompletions),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(endpoint_for(Go, OpenAiResponses), "https://opencode.ai/zen/go/v1/responses");
        assert_eq!(endpoint_for(Go, AnthropicMessages), "https://opencode.ai/zen/go/v1/messages");
        assert_eq!(
            endpoint_for(Go, OpenAiChatCompletions),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
    }

    #[test]
    fn every_catalog_entry_matches_its_root_and_path() {
        let entries: Vec<_> = catalog().collect();
        assert!(!entries.is_empty());
        for route in &entries {
            assert_eq!(route.endpoint, endpoint_for(route.surface, route.dialect));
            assert!(route.endpoint.starts_with(route.surface.api_root()));
            assert!(route.endpoint.ends_with(route.dialect.path()));
        }
    }

    #[test]
    fn catalog_covers_documented_model_groups() {
        for model_id in [
            "gpt-6-astra",
            "grok-4.5",
            "muse-spark-1.2",
            "claude-opus-5",
            "qwen3.5-plus",
            "deepseek-v4-pro",
            "minimax-m2.7",
            "glm-5.3",
            "kimi-k3",
            "big-pickle",
            "space-bunny-free",
        ] {
            assert!(resolve_route(Zen, model_id).is_ok(), "{model_id} missing from zen catalog");
        }
        for model_id in [
            "grok-4.7",
            "gpt-6-luna",
            "muse-spark-1.2-contributor",
            "minimax-m2.5",
            "qwen3.8-max",
            "glm-5.3-flash",
            "kimi-k2.6",
            "longcat-2.0",
            "deepseek-v4-flash",
            "mimo-v2.5-pro",
            "hy4-preview",
            "hy3",
        ] {
            assert!(resolve_route(Go, model_id).is_ok(), "{model_id} missing from go catalog");
        }
    }

    #[test]
    fn catalog_has_no_duplicate_or_overlapping_entries() {
        let mut seen = BTreeSet::new();
        for entry in catalog() {
            assert!(
                seen.insert((entry.surface.as_str(), entry.model_id)),
                "duplicate catalog entry for {} {}",
                entry.surface.as_str(),
                entry.model_id
            );
        }
        for entry in crate::catalog::generated::RETIRED {
            assert!(
                catalog_entry(entry.surface, entry.model_id).is_none(),
                "retired {} model {} is also routable",
                entry.surface.as_str(),
                entry.model_id
            );
        }
        for entry in crate::catalog::generated::UNSUPPORTED {
            assert!(
                catalog_entry(entry.surface, entry.model_id).is_none(),
                "unsupported {} model {} is also routable",
                entry.surface.as_str(),
                entry.model_id
            );
        }
    }

    #[test]
    fn every_codec_module_serves_at_least_one_catalog_route() {
        for (module, dialect) in [
            ("responses", crate::responses::DIALECT),
            ("messages", crate::messages::DIALECT),
            ("chat_completions", crate::chat_completions::DIALECT),
        ] {
            assert!(
                crate::catalog::models().any(|entry| entry.dialect == dialect),
                "{module} declares {dialect:?} with no catalog routes"
            );
        }
    }

    #[test]
    fn empty_model_fails_closed() {
        assert_eq!(resolve_route(Zen, "").unwrap_err(), ConfigError::MissingModel);
        assert_eq!(resolve_route(Go, "   ").unwrap_err(), ConfigError::MissingModel);
    }

    #[test]
    fn malformed_surface_fails_closed() {
        assert_eq!(OpenCodeSurface::parse("zen"), Ok(Zen));
        assert_eq!(OpenCodeSurface::parse("go"), Ok(Go));
        assert_eq!(
            OpenCodeSurface::parse("opencode").unwrap_err(),
            ConfigError::UnsupportedSurface { value: String::from("opencode") }
        );
        assert!(OpenCodeSurface::parse("").is_err());
        assert!(OpenCodeSurface::parse("Zen").is_err());
        assert!(OpenCodeSurface::parse(" zen ").is_err());
        assert!(OpenCodeSurface::parse("opencode-go").is_err());
    }

    #[test]
    fn unknown_model_fails_closed() {
        assert_eq!(
            resolve_route(Zen, "codex-mini-latest").unwrap_err(),
            ConfigError::UnknownModel { surface: Zen, model_id: String::from("codex-mini-latest") }
        );
        assert_eq!(
            resolve_route(Go, "kimi-k4").unwrap_err(),
            ConfigError::UnknownModel { surface: Go, model_id: String::from("kimi-k4") }
        );
    }

    #[test]
    fn cross_surface_model_fails_closed_with_surface_guidance() {
        assert_eq!(
            resolve_route(Zen, "longcat-2.0").unwrap_err(),
            ConfigError::WrongSurface {
                surface: Zen,
                model_id: String::from("longcat-2.0"),
                documented_surface: Go,
            }
        );
        // Zen retired `minimax-m2.5` while Go still documents it; the actionable
        // failure is the surface mismatch.
        assert_eq!(
            resolve_route(Zen, "minimax-m2.5").unwrap_err(),
            ConfigError::WrongSurface {
                surface: Zen,
                model_id: String::from("minimax-m2.5"),
                documented_surface: Go,
            }
        );
    }

    #[test]
    fn retired_models_fail_closed() {
        assert_eq!(
            resolve_route(Zen, "gpt-5-codex").unwrap_err(),
            ConfigError::RetiredModel {
                surface: Zen,
                model_id: String::from("gpt-5-codex"),
                deprecated_on: "2026-07-23",
            }
        );
        assert_eq!(
            resolve_route(Zen, "glm-5").unwrap_err(),
            ConfigError::RetiredModel {
                surface: Zen,
                model_id: String::from("glm-5"),
                deprecated_on: "2026-05-14",
            }
        );
    }

    #[test]
    fn google_models_are_rejected_with_dialect_guidance() {
        assert_eq!(
            resolve_route(Zen, "gemini-3.8-flash").unwrap_err(),
            ConfigError::UnsupportedModelDialect {
                surface: Zen,
                model_id: String::from("gemini-3.8-flash"),
                dialect: UnsupportedDialect::GoogleGenerativeLanguage,
            }
        );
        let message = resolve_route(Zen, "gemini-3-flash").unwrap_err().to_string();
        assert!(message.contains("Google Generative Language"), "{message}");
        assert!(message.contains("does not support yet"), "{message}");
    }

    #[test]
    fn system_one_models_are_rejected_with_dialect_guidance() {
        assert_eq!(
            resolve_route(Zen, "jev-1.13").unwrap_err(),
            ConfigError::UnsupportedModelDialect {
                surface: Zen,
                model_id: String::from("jev-1.13"),
                dialect: UnsupportedDialect::SystemOne,
            }
        );
        assert!(resolve_route(Zen, "jev-1.13-free").is_err());
    }

    #[test]
    fn tui_alias_ids_are_rejected_with_bare_id_guidance() {
        assert_eq!(
            resolve_route(Go, "opencode-go/kimi-k3").unwrap_err(),
            ConfigError::TuiAliasModel {
                model_id: String::from("opencode-go/kimi-k3"),
                bare_model_id: String::from("kimi-k3"),
            }
        );
        assert_eq!(
            resolve_route(Zen, "opencode/gpt-5.5").unwrap_err(),
            ConfigError::TuiAliasModel {
                model_id: String::from("opencode/gpt-5.5"),
                bare_model_id: String::from("gpt-5.5"),
            }
        );
        // An alias that wraps nothing routable stays an unknown id.
        assert!(matches!(
            resolve_route(Zen, "opencode/codex-mini-latest").unwrap_err(),
            ConfigError::UnknownModel { .. }
        ));
    }

    #[test]
    fn surrounding_whitespace_in_model_ids_is_trimmed() {
        assert_eq!(route(Zen, " gpt-5.5\n").model_id, "gpt-5.5");
        assert_eq!(route(Go, "\tkimi-k3 ").model_id, "kimi-k3");
    }

    #[test]
    fn variant_suffix_selects_a_catalog_derived_variant() {
        let route = route(Zen, "gpt-5.5#high");
        let variant = route.variant.expect("variant selected");
        assert_eq!(variant.id, "high");
        assert_eq!(variant.effort, Some("high"));
        assert_eq!(route.model_id, "gpt-5.5");
    }

    #[test]
    fn unknown_variant_lists_the_valid_ids() {
        let error = resolve_route(Zen, "gpt-5.5#bogus").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no reasoning variant \"bogus\""), "{message}");
        assert!(message.contains("none"), "{message}");
        assert!(message.contains("high"), "{message}");
    }

    #[test]
    fn empty_variant_suffix_fails_closed() {
        assert_eq!(
            resolve_route(Zen, "gpt-5.5#").unwrap_err(),
            ConfigError::MissingVariant { model_id: String::from("gpt-5.5") }
        );
    }

    #[test]
    fn models_without_reasoning_support_reject_every_variant() {
        let error = resolve_route(Zen, "claude-3-5-haiku#high").unwrap_err();
        assert_eq!(
            error,
            ConfigError::UnknownVariant {
                model_id: String::from("claude-3-5-haiku"),
                variant: String::from("high"),
                valid: String::from("none"),
            }
        );
    }

    #[test]
    fn anthropic_routes_expose_budget_variants_within_the_output_ceiling() {
        let route = route(Zen, "claude-opus-4-5");
        assert_eq!(route.dialect, OpenCodeDialect::AnthropicMessages);
        let variants = route.variants();
        let ids: Vec<_> = variants.iter().map(|variant| variant.id).collect();
        assert_eq!(ids, ["high", "max"]);
        let output = route.limits().expect("limits").output;
        for variant in variants {
            let budget = variant.budget_tokens.expect("budget variant");
            assert!(budget < output, "{budget} must stay below {output}");
            assert!(budget <= 32_000, "{budget} must stay under the Anthropic ceiling");
        }
    }
}
