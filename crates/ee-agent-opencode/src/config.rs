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

use clap::{ArgAction, Parser, builder::BoolishValueParser};
use ee_agent_acp_server::env_or_dotenv;
use ee_agent_orchestrator::{GenerationOptions, RubberDuckConfig, RubberDuckMode};
use ee_agent_protocol::setup::{
    SETUP_MANIFEST_SCHEMA_VERSION, SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig,
    SetupManifest,
};
use ee_chat_completions::{EndpointProfile, RetryPolicy, Timeouts, TokenSource, TrustedEndpoint};
use reqwest::header::{HeaderName, HeaderValue};

use crate::reasoning::{self, ReasoningEffort};
use crate::routes::{self, OpenCodeRoute, OpenCodeSurface};

mod error;
mod manifest;
mod options;

pub use error::ConfigError;

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
/// Approval classifier mode selector: `off` or `auto`.
pub const APPROVAL_CLASSIFIER_ENV: &str = "EE_APPROVAL_CLASSIFIER";
/// Optional approval classifier model id; the root model is used when unset.
pub const APPROVAL_CLASSIFIER_MODEL_ENV: &str = "EE_APPROVAL_CLASSIFIER_MODEL";
/// Optional reasoning-effort selector: `low`, `medium`, or `high`.
pub const REASONING_EFFORT_ENV: &str = "OPENCODE_REASONING_EFFORT";

/// Prompt-cache policy: `auto`, `none`, or a JSON object.
pub const CACHE_POLICY_ENV: &str = "OPENCODE_CACHE_POLICY";

/// Prompt-cache lifetime, in the models.dev duration form.
pub const CACHE_TTL_ENV: &str = "OPENCODE_CACHE_TTL";

/// `/compact` strategy: `local` (default), `native`, or `auto`.
pub const COMPACTION_MODE_ENV: &str = "OPENCODE_COMPACTION_MODE";

/// Hide free catalog models from selection when enabled.
pub const DISABLE_FREE_MODELS_ENV: &str = "OPENCODE_DISABLE_FREE_MODELS";

/// Default system prompt sent with each session transcript.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";
/// API-key description shown during setup.
///
/// OpenCode owns billing, balances, usage limits, and account state; ee only
/// holds the key and presents it to the selected surface.
pub const API_KEY_DESCRIPTION: &str = "OpenCode API key for the selected surface. ee stores it in the encrypted secret store and sends it only as a bearer token to the selected surface; billing, balances, usage limits, and account management stay with OpenCode.";
/// Surface input label shown during setup.
pub const SURFACE_LABEL: &str = "OpenCode surface";
/// Model input label shown during setup.
pub const MODEL_LABEL: &str = "Model for the selected surface (bare documented id; not an OpenCode TUI alias such as opencode/gpt-5.5)";
/// Critic input label shown during setup.
pub const CRITIC_LABEL: &str = "Optional built-in critic model on the same surface (a different vendor is recommended; leave empty to run without a built-in critic)";
/// Default request timeout: 120 seconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Default model context-window size in tokens, used when the catalog carries
/// no limit for the selected model.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
/// Default maximum transient/429 retries per model call.
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 10;
/// Default initial retry backoff: 500 ms.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 500;
/// Default retry backoff cap: 30 seconds.
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 30_000;
/// Startup diagnostic used whenever no usable OpenCode key is configured.
pub const MISSING_API_KEY: &str = "OPENCODE_API_KEY is not set; export it before starting ee";

/// Full per-surface catalog for `--ee-config-choices`.
pub use manifest::remote_choices;
/// Builds the setup manifest emitted by `ee-agent-opencode --ee-config`.
pub use manifest::setup_manifest;

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

/// Whether a catalog cost declares the model free.
fn is_free_model(cost: Option<crate::catalog::ModelCost>) -> bool {
    cost.is_some_and(|cost| cost.input_micros == 0 && cost.output_micros == 0)
}

/// Picks a deterministic default model when none was configured.
///
/// Walks the surface's active chat models in id order and takes the first the
/// caller has not disabled (and that is not free when the free-model filter is
/// on), so two runs never disagree about the default.
fn default_model_for(
    surface: &OpenCodeSurface,
    file: &crate::config_file::FileConfig,
    disable_free_models: bool,
) -> Result<String, ConfigError> {
    for id in routes::chat_model_ids(*surface) {
        if file.resolve(id).disabled {
            continue;
        }
        let Ok(route) = routes::resolve_route(*surface, id) else {
            continue;
        };
        if disable_free_models && is_free_model(route.cost()) {
            continue;
        }
        return Ok(id.to_string());
    }
    Err(ConfigError::MissingModel)
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
    name = "ee-agent-opencode",
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
    /// List catalog models with limits, modalities, reasoning variants, and
    /// cost, then exit without reading a credential or contacting the network.
    #[arg(long, conflicts_with_all = ["ee_config", "print_route", "discover_models"])]
    pub list_models: bool,
    /// Emit machine-readable JSON for `--list-models`.
    #[arg(long, requires = "list_models")]
    pub json: bool,
    /// Fetch the selected surface's `/models` metadata (one network call) and print it as
    /// display-only JSON; live metadata never changes routing.
    #[arg(long, conflicts_with = "ee_config")]
    pub discover_models: bool,
    /// Query OpenCode workspace budgets and the Go allowance, then exit. No
    /// network call happens on any other path.
    #[arg(
        long,
        conflicts_with_all = ["print_route", "print_catalog", "list_models", "discover_models"]
    )]
    pub usage: bool,
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
    ///
    /// Historical alias for the whole-exchange budget; the per-phase budgets are
    /// `OPENCODE_TIMEOUT_HEADERS_MS` and `OPENCODE_TIMEOUT_CHUNK_MS`.
    #[arg(long, env = "OPENCODE_TIMEOUT_MS", value_parser = parse_positive_u64, hide = true)]
    timeout_ms: Option<u64>,
    /// Whole-exchange timeout in milliseconds (unset = unbounded).
    #[arg(long, env = "OPENCODE_TIMEOUT_TOTAL_MS", value_parser = parse_positive_u64)]
    timeout_total_ms: Option<u64>,
    /// Budget until response headers arrive, in milliseconds.
    #[arg(long, env = "OPENCODE_TIMEOUT_HEADERS_MS", value_parser = parse_positive_u64)]
    timeout_headers_ms: Option<u64>,
    /// Idle budget between streamed chunks, in milliseconds.
    #[arg(long, env = "OPENCODE_TIMEOUT_CHUNK_MS", value_parser = parse_positive_u64)]
    timeout_chunk_ms: Option<u64>,
    /// Emit provider session-affinity headers (`OPENCODE_SESSION_HEADERS`).
    #[arg(long, env = "OPENCODE_SESSION_HEADERS", default_value_t = true, action = clap::ArgAction::Set)]
    session_headers: bool,
    /// Maximum tool-loop iterations, and model calls, per prompt turn.
    /// Omit for unlimited.
    #[arg(long, env = "OPENCODE_MAX_ITERATIONS", value_parser = parse_positive_usize)]
    max_iterations: Option<usize>,
    /// Model context-window size in tokens, used for the ACP `usage_update`.
    /// Blank falls back to the catalog limit, then to the default.
    #[arg(long, env = "OPENCODE_CONTEXT_WINDOW", value_parser = parse_positive_u64)]
    context_window: Option<u64>,
    /// Maximum output tokens per model call; blank uses the catalog limit fitted
    /// to the context window.
    #[arg(long, env = "OPENCODE_MAX_OUTPUT_TOKENS", value_parser = parse_positive_u64)]
    max_output_tokens: Option<u64>,
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
    /// Opt-in durable session-log root. Records and content-addressed message
    /// bodies are written under `<dir>/session-log`; unset keeps the log in
    /// memory for this process only.
    #[arg(long, env = "EE_SESSION_LOG_DIR")]
    session_log_dir: Option<PathBuf>,
    /// Opt-in durable session log under `<session-state-dir>/session-log`.
    /// Ignored when `EE_SESSION_LOG_DIR` is set.
    #[arg(
        long,
        env = "EE_SESSION_LOG_DURABLE",
        default_value_t = false,
        action = ArgAction::Set,
        value_parser = BoolishValueParser::new(),
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
    )]
    session_log_durable: bool,
    /// `/compact` strategy: `local` (default), `native`, or `auto`.
    #[arg(long, env = COMPACTION_MODE_ENV)]
    compaction_mode: Option<String>,
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
            list_models: false,
            json: false,
            discover_models: false,
            usage: false,
            surface: surface.map(String::from),
            model: model.map(String::from),
            critic_model: None,
            rubber_duck_mode: None,
            reasoning_effort: None,
            system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
            timeout_ms: None,
            timeout_total_ms: None,
            timeout_headers_ms: None,
            timeout_chunk_ms: None,
            session_headers: true,
            max_iterations: None,
            context_window: None,
            max_output_tokens: None,
            retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            retry_max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
            checkpoint_dir: None,
            session_log_dir: None,
            session_log_durable: false,
            compaction_mode: None,
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
    /// Per-phase HTTP budgets for every model call.
    pub timeouts: Timeouts,
    /// Session-default sampling knobs; a per-request value wins knob by knob.
    pub generation: GenerationOptions,
    /// Typed OpenAI/Responses provider options merged into every body.
    pub provider_options: ee_chat_completions::ProviderOptions,
    /// Request-body compression for the routed endpoint.
    pub encoding: ee_chat_completions::RequestEncoding,
    /// Extra endpoint query parameters, percent-encoded once by the transport.
    pub query: Vec<(String, String)>,
    /// Maximum tool-loop iterations, and model calls, per prompt turn.
    pub max_iterations: Option<usize>,
    /// Context-window size reported to the client.
    pub context_window: u64,
    /// Explicit output-token bound; validated against the catalog ceiling.
    pub max_output_tokens: Option<u64>,
    /// Prompt-cache policy override; `None` uses the route default.
    pub cache_policy: Option<ee_chat_completions::CachePolicy>,
    /// Prompt-cache lifetime override.
    pub cache_ttl: Option<String>,
    /// Retry policy for rate-limited and transient failures.
    pub retry: RetryPolicy,
    /// Durable checkpoint directory, when configured.
    pub checkpoint_dir: Option<PathBuf>,
    /// Opt-in durable session-log root; `None` keeps the log in memory.
    pub session_log_dir: Option<PathBuf>,
    /// Opt-in durable session log under the normal session-state directory.
    pub session_log_durable: bool,
    /// Whether provider session-affinity headers are emitted.
    pub session_headers: bool,
    /// `/compact` strategy; `local` keeps the summary path that works on every
    /// route.
    pub compaction_mode: ee_agent_orchestrator::CompactionMode,
    /// Overlay resolved from the optional agent configuration file.
    pub file: crate::config_file::ResolvedConfig,
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
            .field("timeouts", &self.timeouts)
            .field("generation", &self.generation)
            .field("provider_options", &self.provider_options)
            .field("encoding", &self.encoding)
            .field("query", &self.query)
            .field("max_iterations", &self.max_iterations)
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("cache_policy", &self.cache_policy)
            .field("retry", &self.retry)
            .field("checkpoint_dir", &self.checkpoint_dir)
            .field("compaction_mode", &self.compaction_mode)
            .field("file_headers", &self.file.headers.len())
            .field("file_body", &self.file.body.is_some())
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
        let file = crate::config_file::load(&mut |name| env_or_dotenv(name, dotenv))
            .map_err(|detail| ConfigError::ConfigFile { detail })?;
        Self::from_args_and_dotenv_with_file(args, dotenv, file)
    }

    /// Resolves a configuration with an explicit configuration file.
    ///
    /// Test fixtures pass a parsed file here; production code goes through
    /// [`Config::from_args_and_dotenv`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when either selector is missing, empty, or does
    /// not match exactly one catalog entry.
    pub fn from_args_and_dotenv_with_file(
        args: Args,
        dotenv: &BTreeMap<String, String>,
        file: crate::config_file::FileConfig,
    ) -> Result<Self, ConfigError> {
        let disable_free_models = file.disable_free_models
            || env_or_dotenv(DISABLE_FREE_MODELS_ENV, dotenv).is_some_and(|value| {
                matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
            });
        let surface = match args
            .surface
            .or_else(|| env_or_dotenv(SURFACE_ENV, dotenv))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| file.default_surface.clone())
        {
            Some(value) => OpenCodeSurface::parse(&value)?,
            None => return Err(ConfigError::MissingSurface),
        };
        let model = match args
            .model
            .or_else(|| env_or_dotenv(MODEL_ENV, dotenv))
            .filter(|value| !value.trim().is_empty())
            .or_else(|| file.default_model.clone())
        {
            Some(model) => model,
            // Deterministic fallback: the first active chat model the caller has
            // not disabled, in catalog id order.
            None => default_model_for(&surface, &file, disable_free_models)?,
        };
        let base_id = model.rsplit_once('#').map_or(model.trim(), |(base, _)| base.trim());
        if file.resolve(base_id).disabled {
            return Err(ConfigError::DisabledModel { model_id: base_id.to_string() });
        }
        // A configured variant only fills in when the selector carried none.
        let model = match file.resolve(base_id).variant {
            Some(variant) if !model.contains('#') => format!("{}{}{variant}", model.trim(), '#'),
            _ => model,
        };
        let route = routes::resolve_route(surface, &model)?;
        if disable_free_models && is_free_model(route.cost()) {
            return Err(ConfigError::FreeModelDisabled { model_id: route.model_id.to_string() });
        }
        let compaction_mode = match args
            .compaction_mode
            .or_else(|| env_or_dotenv(COMPACTION_MODE_ENV, dotenv))
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) => {
                ee_agent_orchestrator::CompactionMode::parse(&value).map_err(|detail| {
                    ConfigError::UnsupportedCompactionMode {
                        value: value.trim().to_string(),
                        detail,
                    }
                })?
            }
            None => ee_agent_orchestrator::CompactionMode::Local,
        };
        // Forcing the provider path on an unsupported route is a startup error,
        // never a request-time surprise. `Auto` falls back to local instead.
        if compaction_mode == ee_agent_orchestrator::CompactionMode::Native
            && !route.supports_compaction()
        {
            return Err(ConfigError::UnsupportedCompaction {
                surface,
                model_id: route.model_id.to_string(),
                route: route.dialect.as_str(),
            });
        }
        let reasoning_effort = match args
            .reasoning_effort
            .or_else(|| env_or_dotenv(REASONING_EFFORT_ENV, dotenv))
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) => Some(ReasoningEffort::parse(&value).map_err(|detail| {
                ConfigError::UnsupportedReasoningEffort { value: value.trim().to_string(), detail }
            })?),
            None => None,
        };
        // A model that declares no reasoning support has no variants to select,
        // so the shorthand fails at startup instead of at request time.
        if let Some(effort) = reasoning_effort
            && route.variants().is_empty()
        {
            return Err(ConfigError::UnknownVariant {
                model_id: route.model_id.to_string(),
                variant: effort.as_str().to_string(),
                valid: String::from("none"),
            });
        }
        let context_window = args
            .context_window
            .or_else(|| route.limits().map(|limits| limits.context))
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        if let (Some(requested), Some(limits)) = (args.max_output_tokens, route.limits())
            && requested > limits.output
        {
            return Err(ConfigError::OutputLimitExceeded {
                model_id: route.model_id.to_string(),
                requested,
                maximum: limits.output,
            });
        }
        // A disabled small model never becomes the critic; the critic builder
        // then reports "no critic model configured" instead of routing to it.
        let critic_model = args
            .critic_model
            .or_else(|| env_or_dotenv(CRITIC_MODEL_ENV, dotenv))
            .filter(|value| !value.trim().is_empty())
            .or_else(|| file.small_model.clone())
            .filter(|id| {
                let base = id.rsplit_once('#').map_or(id.trim(), |(base, _)| base.trim());
                !file.resolve(base).disabled
            });
        Ok(Self {
            route,
            api_key: env_or_dotenv(API_KEY_ENV, dotenv),
            critic_model,
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
            reasoning_effort,
            system_prompt: args.system_prompt,
            timeouts: options::timeouts_from(
                |name| env_or_dotenv(name, dotenv),
                args.timeout_total_ms.or(args.timeout_ms),
                args.timeout_headers_ms,
                args.timeout_chunk_ms,
            )?,
            generation: {
                let generation = options::generation_from(|name| env_or_dotenv(name, dotenv))?;
                options::validate_generation(route.model_id, route.entry.sampling, &generation)?;
                generation
            },
            provider_options: options::provider_options_from(|name| env_or_dotenv(name, dotenv))?,
            encoding: options::encoding_from(|name| env_or_dotenv(name, dotenv))?,
            query: options::query_from(|name| env_or_dotenv(name, dotenv))?,
            max_iterations: args.max_iterations,
            context_window,
            max_output_tokens: args.max_output_tokens,
            cache_policy: env_or_dotenv(CACHE_POLICY_ENV, dotenv)
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    ee_chat_completions::parse_policy(&value).map_err(|detail| {
                        ConfigError::InvalidCacheOption { variable: CACHE_POLICY_ENV, detail }
                    })
                })
                .transpose()?,
            cache_ttl: env_or_dotenv(CACHE_TTL_ENV, dotenv)
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    ee_chat_completions::parse_ttl(&value).map_err(|detail| {
                        ConfigError::InvalidCacheOption { variable: CACHE_TTL_ENV, detail }
                    })
                })
                .transpose()?,
            retry: RetryPolicy::new(
                args.retry_max_attempts,
                Duration::from_millis(args.retry_base_delay_ms),
                Duration::from_millis(args.retry_max_delay_ms),
            ),
            checkpoint_dir: args.checkpoint_dir,
            session_log_dir: args.session_log_dir,
            session_log_durable: args.session_log_durable,
            session_headers: args.session_headers,
            compaction_mode,
            file: file.resolve(route.model_id),
        })
    }

    /// Resolves the durable session-log root for `session_state_dir`.
    ///
    /// An explicit `EE_SESSION_LOG_DIR` wins; `EE_SESSION_LOG_DURABLE=1`
    /// defaults under the normal session-state directory. `None` keeps the log
    /// in memory, so no raw message bodies reach disk implicitly.
    #[must_use]
    pub fn session_log_root(&self, session_state_dir: &std::path::Path) -> Option<PathBuf> {
        self.session_log_dir
            .clone()
            .or_else(|| self.session_log_durable.then(|| session_state_dir.to_path_buf()))
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

    /// Returns the workspace organization id for the usage probe.
    ///
    /// The value comes from configuration only; it is never inferred from the
    /// API key.
    #[must_use]
    pub fn org_id(&self) -> Option<&str> {
        self.file.org_id.as_deref()
    }

    /// Validates configured request headers.
    ///
    /// Credential and transport-controlled headers are rejected: the config file
    /// may add provider metadata, never replace authentication or framing.
    fn config_headers(&self) -> Result<Vec<(HeaderName, HeaderValue)>, ConfigError> {
        const RESERVED: [&str; 8] = [
            "authorization",
            "proxy-authorization",
            "cookie",
            "host",
            "content-length",
            "transfer-encoding",
            "connection",
            "x-api-key",
        ];
        let mut headers = Vec::with_capacity(self.file.headers.len());
        for (name, value) in &self.file.headers {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ConfigError::ConfigFile { detail: format!("header name {name:?} is not valid") }
            })?;
            if RESERVED.iter().any(|reserved| *reserved == header_name.as_str()) {
                return Err(ConfigError::ConfigFile {
                    detail: format!("header {name:?} is reserved and cannot be configured"),
                });
            }
            let header_value = HeaderValue::from_str(value).map_err(|_| {
                ConfigError::ConfigFile { detail: format!("header {name:?} has an invalid value") }
            })?;
            headers.push((header_name, header_value));
        }
        Ok(headers)
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
            route_profile(&self.route, &self.system_prompt, self.timeouts, self.retry)?;
        let mut headers = profile.headers.clone();
        headers.extend(self.config_headers()?);
        profile = profile.with_headers(headers);
        let mut extensions = self.reasoning_extensions().unwrap_or_else(|| serde_json::json!({}));
        self.provider_options.merge_into(
            &mut extensions,
            matches!(self.route.dialect, crate::routes::OpenCodeDialect::OpenAiResponses),
        );
        if let Some(overlay) = self.file.body.as_ref() {
            if !extensions.is_object() {
                extensions = serde_json::json!({});
            }
            ee_chat_completions::deep_merge(&mut extensions, overlay);
        }
        profile.extensions =
            extensions.as_object().is_some_and(|object| !object.is_empty()).then_some(extensions);
        profile =
            profile.with_limits(self.route.limits()).with_max_output_tokens(self.max_output_tokens);
        profile = profile
            .with_cache(self.cache_directive())
            .with_sampling(self.route.entry.sampling)
            .with_generation(self.generation.clone())
            .with_query(self.query.clone())
            .with_encoding(self.encoding);
        profile = profile
            .with_session_headers(self.session_headers)
            .with_session_header_style(ee_chat_completions::SessionHeaderStyle::OpenCode)
            .with_user_agent(format!("ee-agent-opencode/{}", env!("CARGO_PKG_VERSION")))
            .with_compaction(self.route.supports_compaction());
        if let Some(field) = self.route.max_tokens_field() {
            let parsed = ee_chat_completions::MaxTokensField::parse(field).map_err(|_| {
                ConfigError::InvalidCatalogMetadata {
                    model_id: self.route.model_id.to_string(),
                    field: "max_tokens_field",
                    value: field,
                }
            })?;
            profile = profile.with_max_tokens_field(parsed);
        }
        Ok(profile)
    }

    /// Request-body extension carrying the selected reasoning variant.
    ///
    /// An explicit `#variant` wins. Otherwise the effort shorthand is matched
    /// against the route's derived variants first, so `OPENCODE_REASONING_EFFORT=
    /// high` selects the `high` budget variant on an Anthropic route. When no
    /// variant matches, the historical effort field is sent on the dialects that
    /// document one.
    #[must_use]
    pub fn reasoning_extensions(&self) -> Option<serde_json::Value> {
        if let Some(variant) = self.route.variant {
            return reasoning::variant_extensions(self.route.dialect, variant);
        }
        let effort = self.reasoning_effort?;
        let variants = self.route.variants();
        if let Some(variant) = crate::catalog::find_variant(&variants, effort.as_str()) {
            return reasoning::variant_extensions(self.route.dialect, *variant);
        }
        reasoning::request_extensions(self.route.dialect, effort)
    }

    /// Resolves the prompt-cache directive for the routed dialect.
    ///
    /// The catalog row owns every decision: whether the route accepts inline
    /// markers, the marker format, the breakpoint cap, and the planes that
    /// accept them. Anthropic Messages rows advertise explicit caching and
    /// Responses rows advertise provider-managed caching; a Chat Completions
    /// row that declares implicit-only caching gets no markers. An operator
    /// override still wins over the catalog's default placement.
    #[must_use]
    pub fn cache_directive(&self) -> ee_chat_completions::CacheDirective {
        use ee_chat_completions::{CacheDirective, CacheFormat, CachePolicy, CacheSupport};
        let capability = self.route.entry.capabilities.cache;
        let policy =
            self.cache_policy.clone().unwrap_or_else(|| CachePolicy::from(capability.placement));
        CacheDirective {
            policy: policy.with_ttl(self.cache_ttl.clone()),
            support: CacheSupport::from(capability),
            format: CacheFormat::from(capability.format),
        }
    }

    /// Bounded operator note when a configured reasoning effort cannot reach the
    /// routed dialect, or `None` when nothing needs explaining.
    #[must_use]
    pub fn reasoning_effort_note(&self) -> Option<String> {
        if self.route.variant.is_some() {
            return None;
        }
        let effort = self.reasoning_effort?;
        if self.route.variants().iter().any(|variant| variant.id == effort.as_str()) {
            return None;
        }
        if reasoning::supports_effort(self.route.dialect) {
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
    timeouts: Timeouts,
    retry: RetryPolicy,
) -> Result<EndpointProfile, ConfigError> {
    let endpoint = TrustedEndpoint::parse(route.endpoint).map_err(|error| {
        ConfigError::UntrustedRouteEndpoint { endpoint: route.endpoint, detail: error.to_string() }
    })?;
    Ok(EndpointProfile::new(route.label(), API_KEY_ENV, endpoint, route.model_id)
        .with_system_prompt(system_prompt)
        .with_timeouts(timeouts)
        .with_retry(retry)
        .with_limits(route.limits())
        .with_cost(route.cost())
        .with_modalities(route.entry.modalities)
        .with_tools_strict(route.entry.tools_strict)
        // A route whose catalog entry does not declare structured output rejects a
        // `response_format` request before it is built.
        .with_structured_output(route.entry.structured_output)
        .with_freeform(route.entry.freeform.iter().map(|value| (*value).to_string()).collect())
        // Catalog-declared `tool_choice` values; an empty list keeps the protocol
        // default, so a row that declares nothing sends today's body.
        .with_tool_choices(route.entry.capabilities.tool_choices)
        // Reference opt-in: only sent when the request actually carries an image,
        // so text-only request bytes stay identical.
        .with_image_opt_in_header(Some(HeaderName::from_static("typesafe-danger-zone-images")))
        // Continuation is a capability: only rows that advertise server-side state
        // may receive a `previous_response_id`. The Responses codec enforces the
        // rest, so a chat or Messages route ignores the flag.
        .with_store_responses(route.supports_store())
        .with_stream_usage(route.entry.capabilities.stream_usage))
}

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
