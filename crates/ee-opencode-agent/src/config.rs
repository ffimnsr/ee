//! Configuration for the OpenCode agent binary.
//!
//! Route selection is explicit and small: `OPENCODE_SURFACE` (`zen` or `go`) and
//! `OPENCODE_MODEL` must both be set and must match exactly one catalog entry.
//! EE never picks a surface, model, or protocol by default, and route resolution
//! performs no I/O, so a rejected model cannot cause an outbound request or an
//! `Authorization` header.
//!
//! `OPENCODE_API_KEY` is a secret: it arrives through the environment or the
//! non-mutating `.env` parser, never as a command-line flag, and it never appears
//! in [`Config`]'s debug output, the setup manifest, a transcript, a checkpoint,
//! or a diagnostic.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use ee_acp_agent_server::env_or_dotenv;
use ee_agent_orchestrator::{RubberDuckConfig, RubberDuckMode};
use ee_agent_protocol::setup::{
    SETUP_MANIFEST_SCHEMA_VERSION, SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig,
    SetupManifest,
};
use ee_chat_completions::{EndpointProfile, RetryPolicy, TokenSource, TrustedEndpoint};

use crate::reasoning::{self, ReasoningEffort};
use crate::routes::{self, OpenCodeRoute, OpenCodeSurface};

/// Secret used only for `Authorization: Bearer` on routed OpenCode requests.
///
/// Route resolution never reads it. Later phases must keep it out of setup
/// output, transcripts, checkpoints, diagnostics, and error details.
pub const API_KEY_ENV: &str = "OPENCODE_API_KEY";
/// Explicit surface selector; must be `zen` or `go`.
pub const SURFACE_ENV: &str = "OPENCODE_SURFACE";
/// Explicit model selector; must be an exact documented model id.
pub const MODEL_ENV: &str = "OPENCODE_MODEL";
/// Optional critic model id for the rubber-duck second opinion.
pub const CRITIC_MODEL_ENV: &str = "OPENCODE_CRITIC_MODEL";
/// Rubber-duck mode selector: `off`, `manual`, or `automatic`.
pub const RUBBER_DUCK_MODE_ENV: &str = "OPENCODE_RUBBER_DUCK_MODE";
/// Optional reasoning-effort selector: `low`, `medium`, or `high`.
pub const REASONING_EFFORT_ENV: &str = "OPENCODE_REASONING_EFFORT";

/// Default system prompt sent with each session transcript.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";
/// API-key description shown during setup.
///
/// OpenCode owns billing, balances, usage limits, and account state; ee only
/// holds the key and presents it to the selected surface.
pub const API_KEY_DESCRIPTION: &str = "OpenCode API key for the selected surface. ee stores it in the encrypted secret store and sends it only as a bearer token to the selected surface; billing, balances, usage limits, and account management stay with OpenCode.";
/// Surface input label shown during setup.
pub const SURFACE_LABEL: &str = "OpenCode surface (required): \"zen\" for pay-as-you-go at https://opencode.ai/zen/v1, \"go\" for subscription plans at https://opencode.ai/zen/go/v1";
/// Model input label shown during setup.
pub const MODEL_LABEL: &str = "Exact model id for the selected surface (required), as documented, e.g. gpt-5.5 on zen or kimi-k3 on go; pass the bare id, not an OpenCode TUI alias such as opencode/gpt-5.5";
/// Critic input label shown during setup.
pub const CRITIC_LABEL: &str = "Optional second-opinion critic model id on the same surface, from a different vendor, e.g. grok-4.5 while the root model is gpt-5.5; leave empty to run without a critic";
/// Default request timeout: 120 seconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Default model context-window size in tokens, used for the ACP `usage_update`
/// context-window denominator.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
/// Default maximum transient/429 retries per model call.
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 2;
/// Default initial retry backoff: 500 ms.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 500;
/// Default retry backoff cap: 30 seconds.
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 30_000;
/// Startup diagnostic used whenever no usable OpenCode key is configured.
pub const MISSING_API_KEY: &str = "OPENCODE_API_KEY is not set; export it before starting ee";

/// Builds the setup manifest emitted by `ee-opencode-agent --ee-config`.
///
/// Neither surface nor model carries a default: the manifest requires an
/// explicit choice so no billable model is selected implicitly.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    SetupManifest {
        schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
        agent: SetupAgent { id: String::from("opencode"), display_name: String::from("OpenCode") },
        env_vars: vec![SetupEnvVar {
            name: String::from(API_KEY_ENV),
            required: true,
            secret: true,
            description: String::from(API_KEY_DESCRIPTION),
        }],
        inputs: vec![
            SetupInput {
                key: String::from("surface"),
                label: String::from(SURFACE_LABEL),
                default: None,
                config: SetupInputConfig { env: String::from(SURFACE_ENV) },
            },
            SetupInput {
                key: String::from("model"),
                label: String::from(MODEL_LABEL),
                default: None,
                config: SetupInputConfig { env: String::from(MODEL_ENV) },
            },
            SetupInput {
                key: String::from("critic_model"),
                label: String::from(CRITIC_LABEL),
                default: None,
                config: SetupInputConfig { env: String::from(CRITIC_MODEL_ENV) },
            },
            SetupInput {
                key: String::from("rubber_duck_mode"),
                label: String::from("Rubber duck mode (off, manual, automatic)"),
                default: Some(String::from("manual")),
                config: SetupInputConfig { env: String::from(RUBBER_DUCK_MODE_ENV) },
            },
            SetupInput {
                key: String::from("reasoning_effort"),
                label: String::from(
                    "Reasoning effort (low, medium, high; leave empty to send the dialect default)",
                ),
                default: None,
                config: SetupInputConfig { env: String::from(REASONING_EFFORT_ENV) },
            },
            SetupInput {
                key: String::from("system_prompt"),
                label: String::from("System prompt"),
                default: Some(String::from(DEFAULT_SYSTEM_PROMPT)),
                config: SetupInputConfig { env: String::from("OPENCODE_SYSTEM_PROMPT") },
            },
            SetupInput {
                key: String::from("timeout_ms"),
                label: String::from("Request timeout in milliseconds"),
                default: Some(DEFAULT_TIMEOUT_MS.to_string()),
                config: SetupInputConfig { env: String::from("OPENCODE_TIMEOUT_MS") },
            },
            SetupInput {
                key: String::from("max_iterations"),
                label: String::from("Maximum tool-loop iterations per prompt turn"),
                default: Some(
                    ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS.to_string(),
                ),
                config: SetupInputConfig { env: String::from("OPENCODE_MAX_ITERATIONS") },
            },
            SetupInput {
                key: String::from("context_window"),
                label: String::from("Model context-window size in tokens"),
                default: Some(DEFAULT_CONTEXT_WINDOW_TOKENS.to_string()),
                config: SetupInputConfig { env: String::from("OPENCODE_CONTEXT_WINDOW") },
            },
            SetupInput {
                key: String::from("retry_max_attempts"),
                label: String::from("Maximum transient retries per model call"),
                default: Some(DEFAULT_RETRY_MAX_ATTEMPTS.to_string()),
                config: SetupInputConfig { env: String::from("OPENCODE_RETRY_MAX_ATTEMPTS") },
            },
            SetupInput {
                key: String::from("retry_base_delay_ms"),
                label: String::from("Initial retry backoff in milliseconds"),
                default: Some(DEFAULT_RETRY_BASE_DELAY_MS.to_string()),
                config: SetupInputConfig { env: String::from("OPENCODE_RETRY_BASE_DELAY_MS") },
            },
            SetupInput {
                key: String::from("retry_max_delay_ms"),
                label: String::from("Retry backoff cap in milliseconds"),
                default: Some(DEFAULT_RETRY_MAX_DELAY_MS.to_string()),
                config: SetupInputConfig { env: String::from("OPENCODE_RETRY_MAX_DELAY_MS") },
            },
            SetupInput {
                key: String::from("checkpoint_dir"),
                label: String::from("Durable checkpoint directory for recovery (optional)"),
                default: None,
                config: SetupInputConfig { env: String::from("EE_CHECKPOINT_DIR") },
            },
        ],
    }
}

fn parse_positive_u64(value: &str) -> Result<u64, String> {
    let value = value.parse::<u64>().map_err(|_| String::from("must be a positive integer"))?;
    if value == 0 {
        return Err(String::from("must be at least 1"));
    }
    Ok(value)
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|_| String::from("must be a positive integer"))?;
    if value == 0 {
        return Err(String::from("must be at least 1"));
    }
    Ok(value)
}

/// Parses the rubber-duck mode selector.
fn parse_rubber_duck_mode(value: &str) -> Result<RubberDuckMode, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(RubberDuckMode::Off),
        "manual" => Ok(RubberDuckMode::Manual),
        "automatic" => Ok(RubberDuckMode::Automatic),
        _ => Err(ConfigError::UnsupportedRubberDuckMode { value: value.trim().to_string() }),
    }
}

/// Command-line arguments; every field also reads its `OPENCODE_*` variable.
#[derive(Debug, Parser)]
#[command(
    name = "ee-opencode-agent",
    version,
    about = "ACP stdio bridge for OpenCode Zen and OpenCode Go"
)]
pub struct Args {
    /// Print the ee setup manifest JSON and exit without starting the agent.
    #[arg(long)]
    pub ee_config: bool,
    /// Resolve and print the exact route for OPENCODE_SURFACE + OPENCODE_MODEL, then exit.
    #[arg(long, conflicts_with_all = ["print_catalog", "ee_config", "discover_models"])]
    pub print_route: bool,
    /// Print every documented routable (surface, model) entry, then exit.
    #[arg(long, conflicts_with = "ee_config")]
    pub print_catalog: bool,
    /// Fetch the selected surface's `/models` metadata (one network call) and print it as
    /// display-only JSON; live metadata never changes routing.
    #[arg(long, conflicts_with = "ee_config")]
    pub discover_models: bool,
    /// OpenCode surface: `zen` (pay-as-you-go) or `go` (subscription).
    #[arg(long, env = SURFACE_ENV)]
    surface: Option<String>,
    /// Exact documented model id for the selected surface.
    #[arg(long, env = MODEL_ENV)]
    model: Option<String>,
    /// Optional critic model id for the rubber-duck second opinion; must be another
    /// documented model on the same surface, from a different vendor.
    #[arg(long, env = CRITIC_MODEL_ENV)]
    critic_model: Option<String>,
    /// Rubber-duck mode: `off`, `manual` (default), or `automatic`.
    #[arg(long, env = RUBBER_DUCK_MODE_ENV)]
    rubber_duck_mode: Option<String>,
    /// Reasoning effort sent through the routed dialect's documented field: `low`,
    /// `medium`, or `high`; empty leaves the dialect default in place.
    #[arg(long, env = REASONING_EFFORT_ENV)]
    reasoning_effort: Option<String>,
    /// System prompt sent with each session history.
    #[arg(long, env = "OPENCODE_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,
    /// Request timeout in milliseconds.
    #[arg(long, env = "OPENCODE_TIMEOUT_MS", default_value_t = DEFAULT_TIMEOUT_MS, value_parser = parse_positive_u64)]
    timeout_ms: u64,
    /// Maximum tool-loop iterations, and model calls, per prompt turn.
    #[arg(
        long,
        env = "OPENCODE_MAX_ITERATIONS",
        default_value_t = ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        value_parser = parse_positive_usize
    )]
    max_iterations: usize,
    /// Model context-window size in tokens, used for the ACP `usage_update`.
    #[arg(long, env = "OPENCODE_CONTEXT_WINDOW", default_value_t = DEFAULT_CONTEXT_WINDOW_TOKENS, value_parser = parse_positive_u64)]
    context_window: u64,
    /// Maximum transient/429 retries per model call.
    #[arg(long, env = "OPENCODE_RETRY_MAX_ATTEMPTS", default_value_t = DEFAULT_RETRY_MAX_ATTEMPTS)]
    retry_max_attempts: u32,
    /// Initial retry backoff in milliseconds.
    #[arg(long, env = "OPENCODE_RETRY_BASE_DELAY_MS", default_value_t = DEFAULT_RETRY_BASE_DELAY_MS)]
    retry_base_delay_ms: u64,
    /// Retry backoff cap in milliseconds.
    #[arg(long, env = "OPENCODE_RETRY_MAX_DELAY_MS", default_value_t = DEFAULT_RETRY_MAX_DELAY_MS)]
    retry_max_delay_ms: u64,
    /// Durable checkpoint directory for recovery across processes.
    #[arg(long, env = "EE_CHECKPOINT_DIR")]
    checkpoint_dir: Option<PathBuf>,
}

#[cfg(feature = "test-utils")]
impl Args {
    /// Builds arguments the way the parser would, so the binary can exercise
    /// its startup path with explicit selectors instead of a process
    /// environment.
    ///
    /// Only the two route selectors are parameterized: every other field keeps
    /// the same default the parser would apply.
    #[must_use]
    pub fn for_tests(surface: Option<&str>, model: Option<&str>) -> Self {
        Self {
            ee_config: false,
            print_route: false,
            print_catalog: false,
            discover_models: false,
            surface: surface.map(String::from),
            model: model.map(String::from),
            critic_model: None,
            rubber_duck_mode: None,
            reasoning_effort: None,
            system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
            context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
            retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
            checkpoint_dir: None,
        }
    }
}

/// Everything the production path needs, with the route already resolved.
///
/// `Debug` redacts the API key, so a diagnostic can print the whole
/// configuration without disclosing a credential.
#[derive(Clone)]
pub struct Config {
    /// Exact catalog route selected by `OPENCODE_SURFACE` + `OPENCODE_MODEL`.
    pub route: OpenCodeRoute,
    /// OpenCode API key; `None` when unset, never printed.
    pub api_key: Option<String>,
    /// Critic model id for the rubber-duck second opinion, when configured.
    pub critic_model: Option<String>,
    /// Rubber-duck mode; `manual` keeps the critic behind the slash command.
    pub rubber_duck: RubberDuckConfig,
    /// Reasoning effort sent through the routed dialect's documented field.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// System prompt sent with each session transcript.
    pub system_prompt: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Maximum tool-loop iterations, and model calls, per prompt turn.
    pub max_iterations: usize,
    /// Context-window size reported to the client.
    pub context_window: u64,
    /// Retry policy for rate-limited and transient failures.
    pub retry: RetryPolicy,
    /// Durable checkpoint directory, when configured.
    pub checkpoint_dir: Option<PathBuf>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("route", &self.route)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("critic_model", &self.critic_model)
            .field("rubber_duck", &self.rubber_duck)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("timeout", &self.timeout)
            .field("max_iterations", &self.max_iterations)
            .field("context_window", &self.context_window)
            .field("retry", &self.retry)
            .field("checkpoint_dir", &self.checkpoint_dir)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Resolves a configuration from parsed arguments plus `.env` fallbacks.
    ///
    /// The process environment wins over `.env` (clap already reads it for the
    /// flagged fields) and empty values count as unset.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when either selector is missing, empty, or does
    /// not match exactly one catalog entry.
    pub fn from_args_and_dotenv(
        args: Args,
        dotenv: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let surface = match args
            .surface
            .or_else(|| env_or_dotenv(SURFACE_ENV, dotenv))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            Some(value) => OpenCodeSurface::parse(&value)?,
            None => return Err(ConfigError::MissingSurface),
        };
        let model = args
            .model
            .or_else(|| env_or_dotenv(MODEL_ENV, dotenv))
            .filter(|value| !value.trim().is_empty())
            .ok_or(ConfigError::MissingModel)?;
        let route = routes::resolve_route(surface, &model)?;
        Ok(Self {
            route,
            api_key: env_or_dotenv(API_KEY_ENV, dotenv),
            critic_model: args
                .critic_model
                .or_else(|| env_or_dotenv(CRITIC_MODEL_ENV, dotenv))
                .filter(|value| !value.trim().is_empty()),
            rubber_duck: RubberDuckConfig {
                mode: match args
                    .rubber_duck_mode
                    .or_else(|| env_or_dotenv(RUBBER_DUCK_MODE_ENV, dotenv))
                    .filter(|value| !value.trim().is_empty())
                {
                    Some(value) => parse_rubber_duck_mode(&value)?,
                    None => RubberDuckMode::Manual,
                },
                ..RubberDuckConfig::default()
            },
            reasoning_effort: match args
                .reasoning_effort
                .or_else(|| env_or_dotenv(REASONING_EFFORT_ENV, dotenv))
                .filter(|value| !value.trim().is_empty())
            {
                Some(value) => Some(ReasoningEffort::parse(&value).map_err(|detail| {
                    ConfigError::UnsupportedReasoningEffort {
                        value: value.trim().to_string(),
                        detail,
                    }
                })?),
                None => None,
            },
            system_prompt: args.system_prompt,
            timeout: Duration::from_millis(args.timeout_ms),
            max_iterations: args.max_iterations,
            context_window: args.context_window,
            retry: RetryPolicy {
                max_attempts: args.retry_max_attempts,
                base_delay: Duration::from_millis(args.retry_base_delay_ms),
                max_delay: Duration::from_millis(args.retry_max_delay_ms),
            },
            checkpoint_dir: args.checkpoint_dir,
        })
    }

    /// Returns whether a usable API key is configured.
    #[must_use]
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_deref().is_some_and(|key| !key.is_empty())
    }

    /// Resolves the bearer token for each round trip.
    ///
    /// The key is never copied into a transcript, checkpoint, or error message;
    /// a missing key surfaces the provider-worded startup diagnostic.
    #[must_use]
    pub fn token_source(&self) -> TokenSource {
        let api_key = self.api_key.clone();
        std::sync::Arc::new(move || {
            api_key
                .clone()
                .filter(|key| !key.is_empty())
                .map(ee_chat_completions::BearerToken::new)
                .ok_or_else(|| MISSING_API_KEY.to_string())
        })
    }

    /// Builds the endpoint profile for the resolved route.
    ///
    /// The endpoint is the trusted catalog constant — no environment override can
    /// replace it — and the label names the surface, model, and dialect, so codec
    /// errors identify the routed endpoint without ever exposing a credential.
    ///
    /// A configured reasoning effort adds exactly one field, and only when the
    /// routed dialect documents it ([`crate::reasoning`]).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UntrustedRouteEndpoint`] when a catalog endpoint is
    /// not a credential-safe origin, which is a catalog defect rather than a user
    /// error.
    pub fn profile(&self) -> Result<EndpointProfile, ConfigError> {
        let mut profile =
            route_profile(&self.route, &self.system_prompt, self.timeout, self.retry)?;
        profile.extensions = self
            .reasoning_effort
            .and_then(|effort| reasoning::request_extensions(self.route.dialect, effort));
        Ok(profile)
    }

    /// Bounded operator note when a configured reasoning effort cannot reach the
    /// routed dialect, or `None` when nothing needs explaining.
    #[must_use]
    pub fn reasoning_effort_note(&self) -> Option<String> {
        if self.reasoning_effort.is_none() || reasoning::supports_effort(self.route.dialect) {
            return None;
        }
        Some(reasoning::unsupported_note(self.route.dialect))
    }
}

/// Builds the endpoint profile shared by one route's dialect codec.
///
/// # Errors
///
/// Returns [`ConfigError::UntrustedRouteEndpoint`] when a catalog endpoint is not
/// a credential-safe origin, which is a catalog defect rather than a user error.
pub fn route_profile(
    route: &OpenCodeRoute,
    system_prompt: impl Into<String>,
    timeout: Duration,
    retry: RetryPolicy,
) -> Result<EndpointProfile, ConfigError> {
    let endpoint = TrustedEndpoint::parse(route.endpoint).map_err(|error| {
        ConfigError::UntrustedRouteEndpoint { endpoint: route.endpoint, detail: error.to_string() }
    })?;
    Ok(EndpointProfile::new(
        format!(
            "OpenCode {} {} ({})",
            route.surface.as_str(),
            route.model_id,
            route.dialect.as_str()
        ),
        API_KEY_ENV,
        endpoint,
        route.model_id,
    )
    .with_system_prompt(system_prompt)
    .with_timeout(timeout)
    .with_retry(retry))
}

/// Why an OpenCode surface/model selection could not be routed.
///
/// Every variant is produced before any HTTP client or credential header is
/// constructed, and no variant ever carries the API key or a raw authorization
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
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
    /// A catalog endpoint is not a credential-safe origin.
    UntrustedRouteEndpoint {
        /// The rejected catalog endpoint.
        endpoint: &'static str,
        /// Why the endpoint cannot carry a credential.
        detail: String,
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
                "{MODEL_ENV} is required: set an exact documented model id for the selected surface"
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
                "model {model_id:?} on OpenCode {} uses the {} dialect, which ee-opencode-agent \
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
            Self::UntrustedRouteEndpoint { endpoint, detail } => write!(
                formatter,
                "route endpoint {endpoint:?} is not a credential-safe origin: {detail}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Resolves the exact route for `OPENCODE_SURFACE` and `OPENCODE_MODEL`.
///
/// # Errors
///
/// Returns [`ConfigError`] when either selector is missing, empty, or does not
/// match exactly one catalog entry.
pub fn resolve_route_from_env() -> Result<OpenCodeRoute, ConfigError> {
    resolve_route_from(|name| std::env::var(name).ok())
}

/// Resolves the route from an explicit variable lookup.
///
/// Callers pass the process environment or a parsed `.env` map, which keeps
/// resolution testable without mutating process state. Surrounding whitespace is
/// trimmed and empty values count as unset, matching how ee treats other
/// environment values. `OPENCODE_SURFACE` must then be exactly `zen` or `go`, and
/// `OPENCODE_MODEL` must be an exact catalog id. No default is ever applied.
///
/// # Errors
///
/// Returns [`ConfigError`] when either selector is missing, empty, or the pair
/// does not match exactly one catalog entry.
pub fn resolve_route_from<F>(lookup: F) -> Result<OpenCodeRoute, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let surface = match lookup(SURFACE_ENV).filter(|value| !value.trim().is_empty()) {
        Some(value) => OpenCodeSurface::parse(value.trim())?,
        None => return Err(ConfigError::MissingSurface),
    };
    let model_id = lookup(MODEL_ENV)
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::MissingModel)?;
    routes::resolve_route(surface, &model_id)
}

#[cfg(test)]
mod tests;
