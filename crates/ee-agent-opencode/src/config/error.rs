//! Route and configuration failures.
//!
//! Every variant is produced before any HTTP client or credential header is
//! constructed, and no variant ever carries the API key or a raw authorization
//! value. The type lives in its own module so `config.rs` stays below the
//! repository's 1,000-line threshold.

use crate::routes::{self, OpenCodeSurface};

use super::{MODEL_ENV, REASONING_EFFORT_ENV, RUBBER_DUCK_MODE_ENV, SURFACE_ENV};

/// Why an OpenCode surface/model selection could not be routed.
///
/// Every variant is produced before any HTTP client or credential header is
/// constructed, and no variant ever carries the API key or a raw authorization
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `OPENCODE_CACHE_POLICY` / `OPENCODE_CACHE_TTL` is malformed.
    InvalidCacheOption {
        /// Environment variable that was rejected.
        variable: &'static str,
        /// Parser diagnostic.
        detail: String,
    },
    /// `OPENCODE_SURFACE` was unset or empty.
    MissingSurface,
    /// `OPENCODE_SURFACE` was neither `zen` nor `go`.
    UnsupportedSurface {
        /// The rejected value, after trimming.
        value: String,
    },
    /// `OPENCODE_MODEL` was unset or empty.
    MissingModel,
    /// The id is not a documented model on the selected surface.
    UnknownModel {
        /// Surface the id was requested on.
        surface: OpenCodeSurface,
        /// The rejected model id.
        model_id: String,
    },
    /// The id is documented, but only on the other surface.
    WrongSurface {
        /// Surface the id was requested on.
        surface: OpenCodeSurface,
        /// The rejected model id.
        model_id: String,
        /// Surface that documents the id.
        documented_surface: OpenCodeSurface,
    },
    /// The id is documented as deprecated on the selected surface.
    RetiredModel {
        /// Surface that retired the id.
        surface: OpenCodeSurface,
        /// The rejected model id.
        model_id: String,
        /// Upstream published deprecation date.
        deprecated_on: &'static str,
    },
    /// The id is documented, but its protocol class has no codec in EE.
    UnsupportedModelDialect {
        /// Surface that documents the id.
        surface: OpenCodeSurface,
        /// The rejected model id.
        model_id: String,
        /// Model class EE cannot encode or decode yet.
        dialect: routes::UnsupportedDialect,
    },
    /// `OPENCODE_RUBBER_DUCK_MODE` was not one of the documented modes.
    UnsupportedRubberDuckMode {
        /// The rejected value, after trimming.
        value: String,
    },
    /// `OPENCODE_REASONING_EFFORT` was not one of the documented levels.
    UnsupportedReasoningEffort {
        /// The rejected value, after trimming.
        value: String,
        /// Guidance naming the accepted values.
        detail: String,
    },
    /// The id carries an OpenCode TUI-only provider alias prefix.
    TuiAliasModel {
        /// The rejected id, including its alias prefix.
        model_id: String,
        /// Catalog id the alias prefix wraps.
        bare_model_id: String,
    },
    /// `model#variant` carried an empty variant after the separator.
    MissingVariant {
        /// Model id before the `#`.
        model_id: String,
    },
    /// The selected variant is not one the model's catalog row exposes.
    UnknownVariant {
        /// Catalog model id.
        model_id: String,
        /// Rejected variant id.
        variant: String,
        /// Comma-separated valid variant ids, or `none` when the model exposes
        /// no reasoning variants at all.
        valid: String,
    },
    /// A catalog endpoint is not a credential-safe origin.
    UntrustedRouteEndpoint {
        /// The rejected catalog endpoint.
        endpoint: &'static str,
        /// Why the endpoint cannot carry a credential.
        detail: String,
    },
    /// `OPENCODE_MAX_OUTPUT_TOKENS` exceeds the catalog output limit.
    OutputLimitExceeded {
        /// Catalog model id.
        model_id: String,
        /// Requested output bound.
        requested: u64,
        /// Catalog maximum output bound.
        maximum: u64,
    },
    /// A generated catalog value is not one the codec understands.
    InvalidCatalogMetadata {
        /// Catalog model id.
        model_id: String,
        /// Field name.
        field: &'static str,
        /// Rejected value.
        value: &'static str,
    },
    /// A sampling or transport option is malformed.
    InvalidGenerationOption {
        /// Environment variable or option family that was rejected.
        variable: String,
        /// Parser diagnostic.
        detail: String,
    },
    /// The model does not document a configured sampling knob.
    UnsupportedSamplingKnob {
        /// Catalog model id.
        model_id: String,
        /// Rejected knob name.
        knob: &'static str,
    },
    /// `OPENCODE_COMPACTION_MODE` was not one of the documented modes.
    UnsupportedCompactionMode {
        /// The rejected value, after trimming.
        value: String,
        /// Guidance naming the accepted values.
        detail: String,
    },
    /// Provider-native compaction was forced on a route that does not advertise it.
    UnsupportedCompaction {
        /// Surface serving the route.
        surface: OpenCodeSurface,
        /// Catalog model id.
        model_id: String,
        /// Protocol route the model speaks.
        route: &'static str,
    },
    /// The agent configuration file is malformed or references a rejected value.
    ConfigFile {
        /// Bounded parser diagnostic naming the key or variable, never a value.
        detail: String,
    },
    /// The selected model is disabled in the configuration file.
    DisabledModel {
        /// Catalog model id.
        model_id: String,
    },
    /// Free models are disabled and the selected model has no cost.
    FreeModelDisabled {
        /// Catalog model id.
        model_id: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSurface => write!(
                formatter,
                "{SURFACE_ENV} is required and must be \"zen\" or \"go\": no OpenCode surface is \
                 selected by default"
            ),
            Self::UnsupportedSurface { value } => write!(
                formatter,
                "unsupported OpenCode surface {value:?}: expected \"zen\" or \"go\""
            ),
            Self::MissingModel => write!(
                formatter,
                "no model is configured and the selected surface has no enabled chat model: set \
                 {MODEL_ENV} to an exact documented model id"
            ),
            Self::UnknownModel { surface, model_id } => write!(
                formatter,
                "model {model_id:?} is not a documented OpenCode {} model: pass an exact model id \
                 from the current OpenCode {} model list",
                surface.as_str(),
                surface.as_str()
            ),
            Self::WrongSurface { surface, model_id, documented_surface } => write!(
                formatter,
                "model {model_id:?} is documented for OpenCode {}, not {}: set {SURFACE_ENV}={} or \
                 pick a {} model id",
                documented_surface.as_str(),
                surface.as_str(),
                documented_surface.as_str(),
                surface.as_str()
            ),
            Self::RetiredModel { surface, model_id, deprecated_on } => write!(
                formatter,
                "model {model_id:?} was deprecated on OpenCode {} on {deprecated_on} and is not \
                 routed: pick a current model id",
                surface.as_str()
            ),
            Self::UnsupportedModelDialect { surface, model_id, dialect } => write!(
                formatter,
                "model {model_id:?} on OpenCode {} uses the {} dialect, which ee-agent-opencode \
                 does not support yet: no request was attempted",
                surface.as_str(),
                dialect.label()
            ),
            Self::UnsupportedRubberDuckMode { value } => write!(
                formatter,
                "unsupported rubber duck mode {value:?}: {RUBBER_DUCK_MODE_ENV} accepts \"off\", \
                 \"manual\", or \"automatic\""
            ),
            Self::UnsupportedReasoningEffort { value, detail } => write!(
                formatter,
                "unsupported reasoning effort {value:?} from {REASONING_EFFORT_ENV}: {detail}"
            ),
            Self::TuiAliasModel { model_id, bare_model_id } => write!(
                formatter,
                "model id {model_id:?} is an OpenCode TUI alias: pass the documented model id \
                 {bare_model_id:?} instead"
            ),
            Self::MissingVariant { model_id } => write!(
                formatter,
                "model id {model_id:?} ends with an empty #variant: append a variant id or drop the \
                 trailing '#'"
            ),
            Self::UnknownVariant { model_id, variant, valid } => write!(
                formatter,
                "model {model_id:?} has no reasoning variant {variant:?}: expected one of {valid}"
            ),
            Self::UntrustedRouteEndpoint { endpoint, detail } => write!(
                formatter,
                "route endpoint {endpoint:?} is not a credential-safe origin: {detail}"
            ),
            Self::OutputLimitExceeded { model_id, requested, maximum } => write!(
                formatter,
                "max output tokens {requested} exceeds the documented ceiling {maximum} for model \
                 {model_id:?}: lower OPENCODE_MAX_OUTPUT_TOKENS or leave it unset"
            ),
            Self::InvalidCatalogMetadata { model_id, field, value } => write!(
                formatter,
                "catalog metadata {field}={value:?} for model {model_id:?} is not supported by this \
                 ee release: regenerate the catalog with scripts/gen_catalog.py"
            ),
            Self::InvalidCacheOption { variable, detail } => {
                write!(formatter, "invalid {variable}: {detail}")
            }
            Self::InvalidGenerationOption { variable, detail } => {
                write!(formatter, "invalid {variable}: {detail}")
            }
            Self::UnsupportedSamplingKnob { model_id, knob } => write!(
                formatter,
                "model {model_id:?} does not document the {knob} sampling knob: unset it or pick a \
                 model that declares {knob} support"
            ),
            Self::UnsupportedCompactionMode { value, detail } => {
                write!(formatter, "unsupported compaction mode {value:?}: {detail}")
            }
            Self::UnsupportedCompaction { surface, model_id, route } => write!(
                formatter,
                "Provider compaction is not supported by OpenCode {}/{model_id} ({route}): use \
                 OPENCODE_COMPACTION_MODE=local or auto",
                surface.as_str()
            ),
            Self::ConfigFile { detail } => {
                write!(formatter, "invalid agent configuration: {detail}")
            }
            Self::DisabledModel { model_id } => write!(
                formatter,
                "model {model_id:?} is disabled in the agent configuration: enable it or pick \
                 another model"
            ),
            Self::FreeModelDisabled { model_id } => write!(
                formatter,
                "model {model_id:?} is free and free models are disabled: unset \
                 OPENCODE_DISABLE_FREE_MODELS or pick a paid model"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
