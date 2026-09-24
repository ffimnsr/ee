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
//! Provenance: the endpoint tables of `https://opencode.ai/docs/zen/` and
//! `https://opencode.ai/docs/go/` as published 2026-09-24. Where Zen's endpoint
//! table still lists an id whose Zen deprecation date has passed, the
//! deprecation table wins and the id is listed as retired instead. Model classes
//! EE implements no codec for are listed as unsupported, so they fail with
//! explicit guidance rather than a generic unknown-model error.
//!
//! Maintenance: refresh the catalog from the upstream endpoint tables, move
//! withdrawn ids to the retired table, keep [`endpoint_for`] the only place an
//! endpoint is assembled, and keep this module's tests passing.

use crate::config::ConfigError;
use OpenCodeDialect::{AnthropicMessages, OpenAiChatCompletions, OpenAiResponses};
use OpenCodeSurface::{Go, Zen};

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
/// id.
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

/// Every documented routable `(surface, model)` entry, in table order.
///
/// Ordering is grouped by surface and then by dialect, which keeps diagnostic
/// output stable for auditing and documentation.
pub fn catalog() -> impl Iterator<Item = OpenCodeRoute> {
    CATALOG.iter().map(route_for)
}

/// Resolves the exact route for one `(surface, model)` pair.
///
/// `model_id` is matched by exact string equality against the catalog after
/// trimming surrounding whitespace. Unknown, cross-surface, retired, and
/// unsupported-dialect ids return a [`ConfigError`] without side effects: no
/// request and no credential header is ever built from a rejected id.
///
/// # Errors
///
/// Returns [`ConfigError::MissingModel`] for an empty model id, and the other
/// [`ConfigError`] variants for rejected catalog lookups.
pub fn resolve_route(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Result<OpenCodeRoute, ConfigError> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Err(ConfigError::MissingModel);
    }
    if let Some(entry) = catalog_entry(surface, model_id) {
        return Ok(route_for(entry));
    }
    if let Some(bare_model_id) = tui_alias_target(model_id)
        && catalog_entry(surface, bare_model_id).is_some()
    {
        return Err(ConfigError::TuiAliasModel {
            model_id: model_id.to_string(),
            bare_model_id: bare_model_id.to_string(),
        });
    }
    if let Some(other) = other_surface_entry(surface, model_id) {
        return Err(ConfigError::WrongSurface {
            surface,
            model_id: model_id.to_string(),
            documented_surface: other.surface,
        });
    }
    if let Some(retired) = retired_entry(surface, model_id) {
        return Err(ConfigError::RetiredModel {
            surface,
            model_id: model_id.to_string(),
            deprecated_on: retired.deprecated_on,
        });
    }
    if let Some(unsupported) = unsupported_entry(surface, model_id) {
        return Err(ConfigError::UnsupportedModelDialect {
            surface,
            model_id: model_id.to_string(),
            dialect: unsupported.dialect,
        });
    }
    Err(ConfigError::UnknownModel { surface, model_id: model_id.to_string() })
}

/// One documented, routable `(surface, model)` catalog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CatalogEntry {
    surface: OpenCodeSurface,
    model_id: &'static str,
    dialect: OpenCodeDialect,
}

/// Builds one catalog row; the table below is the only routing truth.
const fn entry(
    surface: OpenCodeSurface,
    model_id: &'static str,
    dialect: OpenCodeDialect,
) -> CatalogEntry {
    CatalogEntry { surface, model_id, dialect }
}

/// Documented model id that its surface has retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetiredEntry {
    surface: OpenCodeSurface,
    model_id: &'static str,
    /// Upstream published deprecation date.
    deprecated_on: &'static str,
}

const fn retired(
    surface: OpenCodeSurface,
    model_id: &'static str,
    deprecated_on: &'static str,
) -> RetiredEntry {
    RetiredEntry { surface, model_id, deprecated_on }
}

/// Documented model id whose protocol class has no codec in EE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnsupportedEntry {
    surface: OpenCodeSurface,
    model_id: &'static str,
    dialect: UnsupportedDialect,
}

const fn unsupported(
    surface: OpenCodeSurface,
    model_id: &'static str,
    dialect: UnsupportedDialect,
) -> UnsupportedEntry {
    UnsupportedEntry { surface, model_id, dialect }
}

/// Every documented, routable `(surface, model)` entry.
///
/// One row per currently documented model, in upstream endpoint-table order
/// within each surface/dialect group. No wildcard, prefix, or alias rows exist
/// here: accept an alias only by adding an explicit row, never by matching one.
const CATALOG: &[CatalogEntry] = &[
    // OpenCode Zen, OpenAI Responses.
    entry(Zen, "gpt-6-astra", OpenAiResponses),
    entry(Zen, "gpt-6-sol", OpenAiResponses),
    entry(Zen, "gpt-6-luna", OpenAiResponses),
    entry(Zen, "gpt-5.6-sol", OpenAiResponses),
    entry(Zen, "gpt-5.6-terra", OpenAiResponses),
    entry(Zen, "gpt-5.6-luna", OpenAiResponses),
    entry(Zen, "gpt-5.5", OpenAiResponses),
    entry(Zen, "gpt-5.5-pro", OpenAiResponses),
    entry(Zen, "gpt-5.4", OpenAiResponses),
    entry(Zen, "gpt-5.4-pro", OpenAiResponses),
    entry(Zen, "gpt-5.4-mini", OpenAiResponses),
    entry(Zen, "gpt-5.4-nano", OpenAiResponses),
    entry(Zen, "gpt-5.3-codex", OpenAiResponses),
    entry(Zen, "gpt-5.3-codex-spark", OpenAiResponses),
    entry(Zen, "gpt-5.2", OpenAiResponses),
    entry(Zen, "gpt-5.1", OpenAiResponses),
    entry(Zen, "gpt-5", OpenAiResponses),
    entry(Zen, "gpt-5-nano", OpenAiResponses),
    entry(Zen, "grok-4.7", OpenAiResponses),
    entry(Zen, "grok-4.6", OpenAiResponses),
    entry(Zen, "grok-4.5", OpenAiResponses),
    entry(Zen, "grok-build-0.1", OpenAiResponses),
    entry(Zen, "muse-spark-1.3", OpenAiResponses),
    entry(Zen, "muse-spark-1.2", OpenAiResponses),
    entry(Zen, "muse-spark-1.3-contributor-free", OpenAiResponses),
    // OpenCode Zen, Anthropic Messages.
    entry(Zen, "claude-fable-5-1", AnthropicMessages),
    entry(Zen, "claude-fable-5", AnthropicMessages),
    entry(Zen, "claude-opus-5-5", AnthropicMessages),
    entry(Zen, "claude-opus-5", AnthropicMessages),
    entry(Zen, "claude-opus-4-8", AnthropicMessages),
    entry(Zen, "claude-opus-4-7", AnthropicMessages),
    entry(Zen, "claude-opus-4-6", AnthropicMessages),
    entry(Zen, "claude-opus-4-5", AnthropicMessages),
    entry(Zen, "claude-sonnet-5", AnthropicMessages),
    entry(Zen, "claude-sonnet-4-6", AnthropicMessages),
    entry(Zen, "claude-sonnet-4-5", AnthropicMessages),
    entry(Zen, "claude-haiku-4-5", AnthropicMessages),
    entry(Zen, "qwen3.8-flash", AnthropicMessages),
    entry(Zen, "qwen3.7-max", AnthropicMessages),
    entry(Zen, "qwen3.7-plus", AnthropicMessages),
    entry(Zen, "qwen3.6-plus", AnthropicMessages),
    entry(Zen, "qwen3.5-plus", AnthropicMessages),
    // OpenCode Zen, OpenAI-compatible Chat Completions.
    entry(Zen, "deepseek-v4.1-flash", OpenAiChatCompletions),
    entry(Zen, "deepseek-v4-pro", OpenAiChatCompletions),
    entry(Zen, "deepseek-v4-flash", OpenAiChatCompletions),
    entry(Zen, "deepseek-v4-flash-vision-exp", OpenAiChatCompletions),
    entry(Zen, "minimax-m3", OpenAiChatCompletions),
    entry(Zen, "minimax-m2.7", OpenAiChatCompletions),
    entry(Zen, "glm-5.3-flash", OpenAiChatCompletions),
    entry(Zen, "glm-5.3", OpenAiChatCompletions),
    entry(Zen, "glm-5.2", OpenAiChatCompletions),
    entry(Zen, "glm-5.1", OpenAiChatCompletions),
    entry(Zen, "kimi-k2.6", OpenAiChatCompletions),
    entry(Zen, "kimi-k2.7-code", OpenAiChatCompletions),
    entry(Zen, "kimi-k3", OpenAiChatCompletions),
    entry(Zen, "big-pickle", OpenAiChatCompletions),
    entry(Zen, "space-bunny-free", OpenAiChatCompletions),
    entry(Zen, "mimo-v2.6-flash-free", OpenAiChatCompletions),
    entry(Zen, "mimo-v2.5-free", OpenAiChatCompletions),
    entry(Zen, "ling-3.0-flash-fin-free", OpenAiChatCompletions),
    entry(Zen, "nemotron-3-ultra-free", OpenAiChatCompletions),
    entry(Zen, "nemotron-3.5-lightning-free", OpenAiChatCompletions),
    // OpenCode Go, OpenAI Responses.
    entry(Go, "grok-4.7", OpenAiResponses),
    entry(Go, "grok-4.6", OpenAiResponses),
    entry(Go, "gpt-6-luna", OpenAiResponses),
    entry(Go, "gpt-5.6-luna", OpenAiResponses),
    entry(Go, "muse-spark-1.3-contributor", OpenAiResponses),
    entry(Go, "muse-spark-1.2-contributor", OpenAiResponses),
    // OpenCode Go, Anthropic Messages.
    entry(Go, "minimax-m3", AnthropicMessages),
    entry(Go, "minimax-m2.7", AnthropicMessages),
    entry(Go, "minimax-m2.5", AnthropicMessages),
    entry(Go, "qwen3.8-max", AnthropicMessages),
    entry(Go, "qwen3.8-flash", AnthropicMessages),
    entry(Go, "qwen3.7-max", AnthropicMessages),
    entry(Go, "qwen3.7-plus", AnthropicMessages),
    entry(Go, "qwen3.6-plus", AnthropicMessages),
    // OpenCode Go, OpenAI-compatible Chat Completions.
    entry(Go, "glm-5.3-flash", OpenAiChatCompletions),
    entry(Go, "glm-5.3", OpenAiChatCompletions),
    entry(Go, "glm-5.2", OpenAiChatCompletions),
    entry(Go, "glm-5.1", OpenAiChatCompletions),
    entry(Go, "kimi-k3", OpenAiChatCompletions),
    entry(Go, "kimi-k2.7-code", OpenAiChatCompletions),
    entry(Go, "kimi-k2.6", OpenAiChatCompletions),
    entry(Go, "longcat-2.0", OpenAiChatCompletions),
    entry(Go, "deepseek-v4.1-flash", OpenAiChatCompletions),
    entry(Go, "deepseek-v4-pro", OpenAiChatCompletions),
    entry(Go, "deepseek-v4-flash", OpenAiChatCompletions),
    entry(Go, "deepseek-v4-flash-vision-exp", OpenAiChatCompletions),
    entry(Go, "mimo-v2.6-flash", OpenAiChatCompletions),
    entry(Go, "mimo-v2.6-pro", OpenAiChatCompletions),
    entry(Go, "mimo-v2.5", OpenAiChatCompletions),
    entry(Go, "mimo-v2.5-pro", OpenAiChatCompletions),
    // `Ox Alpha` was listed for Go on 2026-08-24 but is absent from the current
    // docs; add explicit rows here only if upstream restores it.
    entry(Go, "hy4-preview", OpenAiChatCompletions),
    entry(Go, "hy3", OpenAiChatCompletions),
    entry(Go, "space-bunny-free", OpenAiChatCompletions),
];

/// Model ids documented as retired on their surface.
///
/// The published date is retained so the exclusion stays auditable and the
/// failure can explain itself. Ids that are retired on one surface but still
/// documented on the other fail with cross-surface guidance instead, because the
/// surface mismatch is the actionable part.
const RETIRED: &[RetiredEntry] = &[
    retired(Zen, "gpt-5.2-codex", "2026-07-23"),
    retired(Zen, "gpt-5.1-codex", "2026-07-23"),
    retired(Zen, "gpt-5.1-codex-max", "2026-07-23"),
    retired(Zen, "gpt-5.1-codex-mini", "2026-07-23"),
    retired(Zen, "gpt-5-codex", "2026-07-23"),
    retired(Zen, "claude-opus-4.1", "2026-08-05"),
    retired(Zen, "claude-sonnet-4", "2026-06-15"),
    retired(Zen, "claude-haiku-3.5", "2026-02-16"),
    retired(Zen, "gemini-3-pro", "2026-03-09"),
    retired(Zen, "minimax-m2.5", "2026-08-05"),
    retired(Zen, "minimax-m2.1", "2026-03-15"),
    retired(Zen, "glm-5", "2026-05-14"),
    retired(Zen, "glm-4.7", "2026-03-15"),
    retired(Zen, "glm-4.6", "2026-03-15"),
    retired(Zen, "kimi-k2.5", "2026-08-05"),
    retired(Zen, "kimi-k2-thinking", "2026-03-06"),
    retired(Zen, "kimi-k2", "2026-03-06"),
    retired(Zen, "qwen3-coder-480b", "2026-02-06"),
];

/// Documented model ids whose protocol class EE does not implement yet.
///
/// Google Generative Language support requires a separate protocol and security
/// review, and System One is a decision API rather than a chat transcript, so
/// neither is guessed at through another codec.
const UNSUPPORTED: &[UnsupportedEntry] = &[
    unsupported(Zen, "gemini-3.8-flash", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3.7-flash", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3.6-flash", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3.5-flash", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3.5-flash-lite", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3.1-pro", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "gemini-3-flash", UnsupportedDialect::GoogleGenerativeLanguage),
    unsupported(Zen, "jev-1.13", UnsupportedDialect::SystemOne),
    unsupported(Zen, "jev-1.13-free", UnsupportedDialect::SystemOne),
];

/// TUI-only provider alias prefixes documented by OpenCode for its config files.
///
/// EE routes documented model ids only; these prefixes exist so a copied TUI
/// value fails with the bare id to use instead.
const TUI_ALIAS_PREFIXES: &[&str] = &["opencode-go/", "opencode/"];

fn route_for(entry: &CatalogEntry) -> OpenCodeRoute {
    OpenCodeRoute {
        surface: entry.surface,
        model_id: entry.model_id,
        dialect: entry.dialect,
        endpoint: endpoint_for(entry.surface, entry.dialect),
    }
}

fn catalog_entry(surface: OpenCodeSurface, model_id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.surface == surface && entry.model_id == model_id)
}

fn other_surface_entry(surface: OpenCodeSurface, model_id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.surface != surface && entry.model_id == model_id)
}

fn retired_entry(surface: OpenCodeSurface, model_id: &str) -> Option<&'static RetiredEntry> {
    RETIRED.iter().find(|entry| entry.surface == surface && entry.model_id == model_id)
}

fn unsupported_entry(
    surface: OpenCodeSurface,
    model_id: &str,
) -> Option<&'static UnsupportedEntry> {
    UNSUPPORTED.iter().find(|entry| entry.surface == surface && entry.model_id == model_id)
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
        for entry in RETIRED {
            assert!(
                catalog_entry(entry.surface, entry.model_id).is_none(),
                "retired {} model {} is also routable",
                entry.surface.as_str(),
                entry.model_id
            );
        }
        for entry in UNSUPPORTED {
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
                CATALOG.iter().any(|entry| entry.dialect == dialect),
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
            resolve_route(Zen, "qwen3.8-max").unwrap_err(),
            ConfigError::WrongSurface {
                surface: Zen,
                model_id: String::from("qwen3.8-max"),
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
}
