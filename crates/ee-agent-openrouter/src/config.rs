//! CLI arguments and runtime configuration for the OpenRouter agent.
//!
//! Every knob keeps its historical `OPENROUTER_*` environment variable and
//! default.  Values that are neither CLI flags nor environment variables can
//! come from the local `.env` file via [`Config::from_args_and_dotenv`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, builder::BoolishValueParser};
use ee_agent_orchestrator::{
    DEFAULT_RUBBER_DUCK_CONTEXT_BYTES, DEFAULT_RUBBER_DUCK_MAX_CALLS,
    DEFAULT_RUBBER_DUCK_OUTPUT_BYTES, DEFAULT_RUBBER_DUCK_TIMEOUT, MAX_RUBBER_DUCK_CONTEXT_BYTES,
    MAX_RUBBER_DUCK_MAX_CALLS, MAX_RUBBER_DUCK_OUTPUT_BYTES, MAX_RUBBER_DUCK_TIMEOUT,
    RubberDuckBackend, RubberDuckConfig, RubberDuckMode,
};
use ee_agent_protocol::setup::{
    SETUP_MANIFEST_SCHEMA_VERSION, SetupAgent, SetupChoice, SetupEnvVar, SetupInput,
    SetupInputConfig, SetupManifest,
};
use ee_chat_completions::Timeouts;

pub use crate::config_file::ConfigError;

/// Default OpenRouter chat-completions endpoint.
pub const DEFAULT_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Default OpenRouter Responses endpoint.
pub const DEFAULT_RESPONSES_URL: &str = "https://openrouter.ai/api/v1/responses";
/// Default OpenRouter model id.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
/// Default system prompt sent with each session history.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an agent running inside ee editor. Answer concisely and help with software engineering tasks. Use available tools for workspace file reads; never print tool-call syntax as prose.";
/// Default minimum stored messages before `/compact` triggers a model summary.
pub const DEFAULT_COMPACT_MIN_MESSAGES: usize = 16;
/// Default stored messages kept verbatim at the tail after `/compact`.
pub const DEFAULT_COMPACT_RETAINED_TAIL: usize = 8;
/// Default maximum serialized bytes of history included in one compaction
/// request (system prompt and compaction prompt included).
pub const DEFAULT_COMPACT_MAX_INPUT_BYTES: usize = 64 * 1024;
/// Default model context-window size in tokens, used for the ACP
/// `usage_update` context-window denominator.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
/// Default percentage of the context window that triggers automatic session
/// compaction after OpenRouter reports a near-limit request.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT: u8 = 80;
/// Default maximum transient/429 retries per model call.
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 10;
/// Default initial retry backoff: 500 ms.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 500;
/// Default retry backoff cap: 30 seconds.
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 30_000;
/// OpenRouter `/compact` strategy: `local` (default), `native`, or `auto`.
pub const COMPACTION_MODE_ENV: &str = "OPENROUTER_COMPACTION_MODE";

/// Builds the setup manifest emitted by `ee-agent-openrouter --ee-config`.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    SetupManifest {
        schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
        agent: SetupAgent {
            id: String::from("openrouter"),
            display_name: String::from("OpenRouter"),
        },
        env_vars: vec![SetupEnvVar {
            name: String::from("OPENROUTER_API_KEY"),
            required: true,
            secret: true,
            description: String::from("OpenRouter API key."),
        }],
        inputs: vec![
            SetupInput {
                key: String::from("model"),
                label: String::from("Model"),
                default: Some(String::from(DEFAULT_MODEL)),
                required: true,
                section: Some(String::from("Model selection")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: false,
                config: SetupInputConfig { env: String::from("OPENROUTER_MODEL") },
            },
            SetupInput {
                key: String::from("model_family"),
                label: String::from("Model family"),
                default: None,
                required: false,
                section: Some(String::from("Model selection")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: false,
                config: SetupInputConfig { env: String::from("OPENROUTER_MODEL_FAMILY") },
            },
            SetupInput {
                key: String::from("rubber_duck_model"),
                label: String::from("Rubber duck model"),
                default: None,
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: false,
                config: SetupInputConfig { env: String::from("OPENROUTER_RUBBER_DUCK_MODEL") },
            },
            SetupInput {
                key: String::from("rubber_duck_model_family"),
                label: String::from("Rubber duck model family"),
                default: None,
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: false,
                config: SetupInputConfig {
                    env: String::from("OPENROUTER_RUBBER_DUCK_MODEL_FAMILY"),
                },
            },
            SetupInput {
                key: String::from("rubber_duck_mode"),
                label: String::from(
                    "OpenRouter's own rubber-duck mode (off, manual, automatic; independent of ee's critic)",
                ),
                default: Some(String::from("manual")),
                required: true,
                section: Some(String::from("Agent behavior")),
                choices: vec![
                    SetupChoice {
                        value: String::from("off"),
                        label: Some(String::from("off: no agent-internal critic")),
                        when: None,
                    },
                    SetupChoice {
                        value: String::from("manual"),
                        label: Some(String::from("manual: run only when you ask")),
                        when: None,
                    },
                    SetupChoice {
                        value: String::from("automatic"),
                        label: Some(String::from("automatic: run on risky turns")),
                        when: None,
                    },
                ],
                remote_choices: false,
                advanced: false,
                config: SetupInputConfig { env: String::from("OPENROUTER_RUBBER_DUCK_MODE") },
            },
            SetupInput {
                key: String::from("rubber_duck_max_calls"),
                label: String::from("Rubber duck maximum calls per session"),
                default: Some(DEFAULT_RUBBER_DUCK_MAX_CALLS.to_string()),
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig { env: String::from("OPENROUTER_RUBBER_DUCK_MAX_CALLS") },
            },
            SetupInput {
                key: String::from("rubber_duck_context_bytes"),
                label: String::from("Rubber duck context byte limit"),
                default: Some(DEFAULT_RUBBER_DUCK_CONTEXT_BYTES.to_string()),
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig {
                    env: String::from("OPENROUTER_RUBBER_DUCK_CONTEXT_BYTES"),
                },
            },
            SetupInput {
                key: String::from("rubber_duck_output_bytes"),
                label: String::from("Rubber duck output byte limit"),
                default: Some(DEFAULT_RUBBER_DUCK_OUTPUT_BYTES.to_string()),
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig {
                    env: String::from("OPENROUTER_RUBBER_DUCK_OUTPUT_BYTES"),
                },
            },
            SetupInput {
                key: String::from("rubber_duck_timeout_ms"),
                label: String::from("Rubber duck timeout in milliseconds"),
                default: Some(DEFAULT_RUBBER_DUCK_TIMEOUT.as_millis().to_string()),
                required: false,
                section: Some(String::from("Agent behavior")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig { env: String::from("OPENROUTER_RUBBER_DUCK_TIMEOUT_MS") },
            },
            SetupInput {
                key: String::from("max_iterations"),
                label: String::from("Maximum iterations (blank = unlimited)"),
                default: None,
                required: false,
                section: Some(String::from("Advanced")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig { env: String::from("OPENROUTER_MAX_ITERATIONS") },
            },
            SetupInput {
                key: String::from("session_log_dir"),
                label: String::from("Durable session-log directory (optional)"),
                default: None,
                required: false,
                section: Some(String::from("Advanced")),
                choices: Vec::new(),
                remote_choices: false,
                advanced: true,
                config: SetupInputConfig { env: String::from("EE_SESSION_LOG_DIR") },
            },
        ],
    }
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|_| String::from("must be a positive integer"))?;
    if value == 0 {
        return Err(String::from("must be at least 1"));
    }
    Ok(value)
}

fn parse_positive_u64(value: &str) -> Result<u64, String> {
    let value = value.parse::<u64>().map_err(|_| String::from("must be a positive integer"))?;
    if value == 0 {
        return Err(String::from("must be at least 1"));
    }
    Ok(value)
}

/// Builds the per-phase HTTP budgets from the parsed arguments.
///
/// The historical `--timeout-ms` / `OPENROUTER_TIMEOUT_MS` value is the
/// whole-exchange budget; the header and chunk budgets fall back to the shared
/// reference defaults when unset.
fn timeouts_from_args(args: &Args) -> Timeouts {
    let defaults = Timeouts::new();
    let total = args.timeout_total_ms.or(args.timeout_ms);
    Timeouts {
        total: total.map(Duration::from_millis),
        headers: args.timeout_headers_ms.map_or(defaults.headers, Duration::from_millis),
        chunk: args.timeout_chunk_ms.map_or(defaults.chunk, Duration::from_millis),
    }
}

fn parse_rubber_duck_mode(value: &str) -> Result<RubberDuckMode, String> {
    match value {
        "off" => Ok(RubberDuckMode::Off),
        "manual" => Ok(RubberDuckMode::Manual),
        "automatic" => Ok(RubberDuckMode::Automatic),
        _ => Err(String::from("must be off, manual, or automatic")),
    }
}

fn parse_bounded_usize(value: &str, field: &str, max: usize) -> Result<usize, String> {
    let value = parse_positive_usize(value)?;
    (value <= max).then_some(value).ok_or_else(|| format!("{field} must be at most {max}"))
}

fn parse_rubber_duck_max_calls(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, "rubber duck max calls", MAX_RUBBER_DUCK_MAX_CALLS)
}

fn parse_rubber_duck_context_bytes(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, "rubber duck context bytes", MAX_RUBBER_DUCK_CONTEXT_BYTES)
}

fn parse_rubber_duck_output_bytes(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, "rubber duck output bytes", MAX_RUBBER_DUCK_OUTPUT_BYTES)
}

fn parse_rubber_duck_timeout_ms(value: &str) -> Result<u64, String> {
    let value = value.parse::<u64>().map_err(|_| String::from("must be a positive integer"))?;
    let max = u64::try_from(MAX_RUBBER_DUCK_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
    if value == 0 || value > max {
        return Err(format!("rubber duck timeout must be between 1 and {max} milliseconds"));
    }
    Ok(value)
}

fn parse_percentage(value: &str) -> Result<u8, String> {
    let value =
        value.parse::<u8>().map_err(|_| String::from("must be an integer from 0 through 100"))?;
    if value > 100 {
        return Err(String::from("must be an integer from 0 through 100"));
    }
    Ok(value)
}

/// Protocol route a session uses on OpenRouter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenRouterApiKind {
    /// OpenAI-compatible `/chat/completions` (default).
    #[default]
    Chat,
    /// OpenAI `/responses`.
    Responses,
}

impl OpenRouterApiKind {
    /// Stable lowercase label used by configuration and listings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }

    /// Parses the configured route name.
    ///
    /// # Errors
    ///
    /// Returns guidance naming the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" | "chat-completions" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            other => Err(format!(
                "unsupported OpenRouter API kind {other:?}: expected \"chat\" or \"responses\""
            )),
        }
    }
}

/// Command-line arguments; every field also reads its `OPENROUTER_*`
/// environment variable.
#[derive(Debug, Parser)]
#[command(version, about = "ACP stdio bridge for OpenRouter chat completions")]
pub struct Args {
    /// Print ee setup manifest JSON and exit without starting the agent.
    #[arg(long)]
    pub ee_config: bool,
    /// List catalog models with limits, modalities, reasoning variants, and
    /// cost, then exit without reading a credential or contacting the network.
    #[arg(long, conflicts_with = "ee_config")]
    pub list_models: bool,
    /// Emit machine-readable JSON for `--list-models`.
    #[arg(long, requires = "list_models")]
    pub json: bool,
    /// OpenRouter model id, e.g. deepseek/deepseek-v4-flash-0731.
    #[arg(long, env = "OPENROUTER_MODEL")]
    model: Option<String>,
    /// Declared root model family; required for trustworthy critic contrast.
    #[arg(long, env = "OPENROUTER_MODEL_FAMILY")]
    model_family: Option<String>,
    /// Optional independent rubber-duck critic model id.
    #[arg(long, env = "OPENROUTER_RUBBER_DUCK_MODEL")]
    rubber_duck_model: Option<String>,
    /// Declared critic family; required with `--rubber-duck-model`.
    #[arg(long, env = "OPENROUTER_RUBBER_DUCK_MODEL_FAMILY")]
    rubber_duck_model_family: Option<String>,
    /// Rubber-duck execution mode.
    #[arg(
        long,
        env = "OPENROUTER_RUBBER_DUCK_MODE",
        default_value = "manual",
        value_parser = parse_rubber_duck_mode,
    )]
    rubber_duck_mode: RubberDuckMode,
    /// Maximum critic calls retained per session.
    #[arg(
        long,
        env = "OPENROUTER_RUBBER_DUCK_MAX_CALLS",
        default_value_t = DEFAULT_RUBBER_DUCK_MAX_CALLS,
        value_parser = parse_rubber_duck_max_calls,
    )]
    rubber_duck_max_calls: usize,
    /// Maximum bounded context bytes sent to critic.
    #[arg(
        long,
        env = "OPENROUTER_RUBBER_DUCK_CONTEXT_BYTES",
        default_value_t = DEFAULT_RUBBER_DUCK_CONTEXT_BYTES,
        value_parser = parse_rubber_duck_context_bytes,
    )]
    rubber_duck_context_bytes: usize,
    /// Maximum critic output bytes accepted for verification.
    #[arg(
        long,
        env = "OPENROUTER_RUBBER_DUCK_OUTPUT_BYTES",
        default_value_t = DEFAULT_RUBBER_DUCK_OUTPUT_BYTES,
        value_parser = parse_rubber_duck_output_bytes,
    )]
    rubber_duck_output_bytes: usize,
    /// Per-critique timeout in milliseconds.
    #[arg(
        long,
        env = "OPENROUTER_RUBBER_DUCK_TIMEOUT_MS",
        default_value_t = u64::try_from(DEFAULT_RUBBER_DUCK_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        value_parser = parse_rubber_duck_timeout_ms,
    )]
    rubber_duck_timeout_ms: u64,
    /// Chat completions endpoint.
    #[arg(long, env = "OPENROUTER_API_URL")]
    api_url: Option<String>,
    /// Protocol route: `chat` (default) or `responses`.
    #[arg(
        long,
        env = "OPENROUTER_API_KIND",
        default_value = "chat",
        value_parser = OpenRouterApiKind::parse,
    )]
    api_kind: OpenRouterApiKind,
    /// Responses endpoint used when `--api-kind responses`.
    #[arg(long, env = "OPENROUTER_RESPONSES_URL")]
    responses_url: Option<String>,
    /// Optional HTTP-Referer value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_SITE_URL")]
    site_url: Option<String>,
    /// Optional X-Title value recommended by OpenRouter.
    #[arg(long, env = "OPENROUTER_APP_TITLE", default_value = "ee")]
    app_title: String,
    /// Whole-exchange timeout in milliseconds (historical alias; unset is
    /// unbounded and the per-phase budgets apply).
    #[arg(long, env = "OPENROUTER_TIMEOUT_MS", hide = true)]
    timeout_ms: Option<u64>,
    /// Whole-exchange timeout in milliseconds (unset = unbounded).
    #[arg(long, env = "OPENROUTER_TIMEOUT_TOTAL_MS")]
    timeout_total_ms: Option<u64>,
    /// Budget until response headers arrive, in milliseconds.
    #[arg(long, env = "OPENROUTER_TIMEOUT_HEADERS_MS")]
    timeout_headers_ms: Option<u64>,
    /// Idle budget between streamed chunks, in milliseconds.
    #[arg(long, env = "OPENROUTER_TIMEOUT_CHUNK_MS")]
    timeout_chunk_ms: Option<u64>,
    /// Emit provider session-affinity headers (`OPENROUTER_SESSION_HEADERS`).
    #[arg(long, env = "OPENROUTER_SESSION_HEADERS", default_value_t = true, action = ArgAction::Set)]
    session_headers: bool,
    /// Optional OpenRouter reasoning effort (`low`, `medium`, or `high`).
    #[arg(long, env = "OPENROUTER_REASONING_EFFORT")]
    reasoning_effort: Option<String>,
    /// System prompt sent with each session history.
    #[arg(long, env = "OPENROUTER_SYSTEM_PROMPT", default_value = DEFAULT_SYSTEM_PROMPT)]
    system_prompt: String,
    /// Minimum stored messages before `/compact` triggers a model summary.
    #[arg(long, env = "OPENROUTER_COMPACT_MIN_MESSAGES", default_value_t = DEFAULT_COMPACT_MIN_MESSAGES)]
    compact_min_messages: usize,
    /// Stored messages kept verbatim at the tail after `/compact`.
    #[arg(long, env = "OPENROUTER_COMPACT_RETAINED_TAIL", default_value_t = DEFAULT_COMPACT_RETAINED_TAIL)]
    compact_retained_tail: usize,
    /// Maximum serialized bytes of history included in one compaction request.
    #[arg(long, env = "OPENROUTER_COMPACT_MAX_INPUT_BYTES", default_value_t = DEFAULT_COMPACT_MAX_INPUT_BYTES)]
    compact_max_input_bytes: usize,
    /// Model context-window size in tokens (reported as `usage_update.size`).
    /// Blank uses the catalog limit, then the default.
    #[arg(long, env = "OPENROUTER_CONTEXT_WINDOW", value_parser = parse_positive_u64)]
    context_window: Option<u64>,
    /// Percentage of the context window that triggers automatic compaction after
    /// a reported near-limit request. Zero disables automatic compaction.
    #[arg(
        long,
        env = "OPENROUTER_AUTO_COMPACT_THRESHOLD_PERCENT",
        default_value_t = DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
        value_parser = parse_percentage,
    )]
    auto_compact_threshold_percent: u8,
    /// Maximum model–tool iterations and model calls per orchestrated turn.
    /// Omit for unlimited.
    #[arg(long, env = "OPENROUTER_MAX_ITERATIONS", value_parser = parse_positive_usize)]
    max_iterations: Option<usize>,
    /// Run in orchestrated mode: ee-agent-orchestrator owns the model–tool
    /// loop and OpenRouter acts as the model adapter instead of the simple
    /// provider mode. Default after parity; opt out for fallback diagnostics.
    #[arg(
        long,
        env = "OPENROUTER_ORCHESTRATED",
        default_value_t = true,
        action = ArgAction::Set,
        value_parser = BoolishValueParser::new(),
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
    )]
    orchestrated: bool,
    /// Maximum transient/429 retries per model call before failing the turn.
    #[arg(long, env = "OPENROUTER_RETRY_MAX_ATTEMPTS", default_value_t = DEFAULT_RETRY_MAX_ATTEMPTS)]
    retry_max_attempts: u32,
    /// Initial retry backoff in milliseconds (doubles per attempt, capped).
    #[arg(long, env = "OPENROUTER_RETRY_BASE_DELAY_MS", default_value_t = DEFAULT_RETRY_BASE_DELAY_MS)]
    retry_base_delay_ms: u64,
    /// Retry backoff cap in milliseconds (and cap for server Retry-After hints).
    #[arg(long, env = "OPENROUTER_RETRY_MAX_DELAY_MS", default_value_t = DEFAULT_RETRY_MAX_DELAY_MS)]
    retry_max_delay_ms: u64,
    /// Directory for durable turn checkpoints (recovery); unset keeps
    /// checkpoints in memory only.
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

/// Resolved agent configuration.
#[derive(Clone)]
pub struct Config {
    /// OpenRouter model id.
    pub model: String,
    /// Explicit root model family identity.
    pub model_family: Option<String>,
    /// Optional independent rubber-duck critic model id.
    pub rubber_duck_model: Option<String>,
    /// Explicit critic model family identity.
    pub rubber_duck_model_family: Option<String>,
    /// Resolved internal critic mode and resource policy.
    pub rubber_duck: RubberDuckConfig,
    /// Chat completions endpoint.
    pub api_url: String,
    /// Protocol route: chat completions or OpenAI Responses.
    pub api_kind: OpenRouterApiKind,
    /// Responses endpoint used when `api_kind` is `Responses`.
    pub responses_url: String,
    /// OpenRouter API key; never logged, used only for the Authorization
    /// header.
    pub api_key: Option<String>,
    /// Optional HTTP-Referer value.
    pub site_url: Option<String>,
    /// X-Title value.
    pub app_title: String,
    /// Per-phase HTTP budgets.
    pub timeouts: Timeouts,
    /// System prompt sent with each session history.
    pub system_prompt: String,
    /// Optional reasoning effort passed as `reasoning.effort`.
    pub reasoning_effort: Option<String>,
    /// Whether to run through the orchestrator (model–tool loop owned by
    /// ee-agent-orchestrator) instead of the simple provider mode.
    pub orchestrated: bool,
    /// Minimum stored messages before `/compact` triggers a model summary.
    pub compact_min_messages: usize,
    /// Stored messages kept verbatim at the tail after `/compact`.
    pub compact_retained_tail: usize,
    /// Maximum serialized bytes of history included in one compaction
    /// request (system prompt and compaction prompt included).
    pub compact_max_input_bytes: usize,
    /// Model context-window size in tokens; the ACP `usage_update.size`.
    pub context_window: u64,
    /// Percentage of the context window that triggers automatic compaction after
    /// a reported near-limit request. Zero disables automatic compaction.
    pub auto_compact_threshold_percent: u8,
    /// Shared maximum for model–tool iterations and model calls per turn.
    pub max_iterations: Option<usize>,
    /// Maximum transient/429 retries per model call before failing the turn.
    pub retry_max_attempts: u32,
    /// Initial retry backoff (doubles per attempt, capped at
    /// `retry_max_delay_ms`).
    pub retry_base_delay: Duration,
    /// Retry backoff cap and cap for server Retry-After hints.
    pub retry_max_delay: Duration,
    /// Durable checkpoint directory for orchestrator recovery.
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
            .field("model", &self.model)
            .field("model_family", &self.model_family)
            .field("rubber_duck_model", &self.rubber_duck_model)
            .field("rubber_duck_model_family", &self.rubber_duck_model_family)
            .field("rubber_duck", &self.rubber_duck)
            .field("api_url", &self.api_url)
            .field("api_kind", &self.api_kind)
            .field("responses_url", &self.responses_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("site_url", &self.site_url)
            .field("app_title", &self.app_title)
            .field("timeouts", &self.timeouts)
            .field("orchestrated", &self.orchestrated)
            .field("compaction_mode", &self.compaction_mode)
            .field("file_headers", &self.file.headers.len())
            .field("file_body", &self.file.body.is_some())
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Resolves a configuration from parsed arguments plus `.env` fallbacks
    /// for the variables that are not CLI flags (`OPENROUTER_API_KEY`,
    /// `OPENROUTER_SITE_URL`), then applies the optional agent config file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for a malformed configuration file, a disabled
    /// model, an invalid compaction mode, or provider compaction forced on a
    /// route that does not advertise it.
    pub fn from_args_and_dotenv(
        args: Args,
        dotenv: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let mut lookup = |name: &str| Self::env_or_dotenv(name, dotenv);
        let file = crate::config_file::load(&mut lookup)
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
    /// Returns [`ConfigError`] for a disabled model, an invalid compaction
    /// mode, or provider compaction forced on a route that does not advertise
    /// it.
    pub fn from_args_and_dotenv_with_file(
        args: Args,
        dotenv: &BTreeMap<String, String>,
        file: crate::config_file::FileConfig,
    ) -> Result<Self, ConfigError> {
        let selected = args
            .model
            .clone()
            .or_else(|| Self::env_or_dotenv("OPENROUTER_MODEL", dotenv))
            .filter(|value| !value.trim().is_empty())
            .or_else(|| file.default_model.clone())
            .unwrap_or_else(|| default_model_for(&file));
        let base = base_model_id(&selected).to_string();
        if file.resolve(&base).disabled {
            return Err(ConfigError::DisabledModel { model_id: base });
        }
        // An empty `model#` selector is a configuration mistake, not "no
        // variant": OpenCode already fails closed on it and both agents must
        // agree.
        if selected.contains('#')
            && selected.rsplit_once('#').is_some_and(|(_, variant)| variant.trim().is_empty())
        {
            return Err(ConfigError::MissingVariant { model_id: base });
        }
        let model = match file.resolve(&base).variant {
            Some(variant) if !selected.contains('#') => {
                format!("{}#{variant}", selected.trim())
            }
            _ => selected,
        };
        let api_kind = args.api_kind;
        let compaction_mode = match args
            .compaction_mode
            .clone()
            .or_else(|| file.compaction_mode.clone())
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) => {
                ee_agent_orchestrator::CompactionMode::parse(&value).map_err(|detail| {
                    ConfigError::InvalidOption {
                        variable: String::from(COMPACTION_MODE_ENV),
                        detail,
                    }
                })?
            }
            None => ee_agent_orchestrator::CompactionMode::Local,
        };
        if compaction_mode == ee_agent_orchestrator::CompactionMode::Native
            && !crate::catalog::find(base_model_id(&model)).is_some_and(|entry| entry.compaction)
        {
            return Err(ConfigError::UnsupportedCompaction {
                model_id: model.clone(),
                route: match api_kind {
                    OpenRouterApiKind::Chat => "chat-completions",
                    OpenRouterApiKind::Responses => "responses",
                },
            });
        }
        let timeouts = timeouts_from_args(&args);
        let catalog_limits =
            crate::catalog::find(base_model_id(&model)).and_then(|entry| entry.limits);
        let context_window = args
            .context_window
            .or_else(|| catalog_limits.map(|limits| limits.context))
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        // A disabled small model never becomes the critic: skip it here so the
        // provider reports "no critic configured" instead of routing to a model
        // the caller removed from service.
        let small_model = args
            .rubber_duck_model
            .clone()
            .or_else(|| file.small_model.clone())
            .filter(|id| !file.resolve(base_model_id(id)).disabled);
        let has_small_model = small_model.is_some();
        let resolved = file.resolve(base_model_id(&model));
        Ok(Self {
            model,
            model_family: args.model_family,
            rubber_duck: RubberDuckConfig {
                mode: args.rubber_duck_mode,
                backend: has_small_model.then(|| RubberDuckBackend::InternalModel {
                    model_id: ee_agent_orchestrator::RUBBER_DUCK_ROLE.to_string(),
                }),

                max_calls: args.rubber_duck_max_calls,

                max_context_bytes: args.rubber_duck_context_bytes,

                max_output_bytes: args.rubber_duck_output_bytes,

                timeout: Duration::from_millis(args.rubber_duck_timeout_ms),
            },

            rubber_duck_model: small_model,

            rubber_duck_model_family: args.rubber_duck_model_family,

            api_url: args
                .api_url
                .or_else(|| file.base_url.clone())
                .unwrap_or_else(|| String::from(DEFAULT_API_URL)),
            api_kind,

            responses_url: args
                .responses_url
                .or_else(|| file.responses_url.clone())
                .unwrap_or_else(|| String::from(DEFAULT_RESPONSES_URL)),

            api_key: Self::env_or_dotenv("OPENROUTER_API_KEY", dotenv),

            site_url: args.site_url.or_else(|| Self::env_or_dotenv("OPENROUTER_SITE_URL", dotenv)),

            app_title: args.app_title,

            timeouts,

            system_prompt: args.system_prompt,

            reasoning_effort: args.reasoning_effort,

            orchestrated: args.orchestrated,

            compact_min_messages: args.compact_min_messages,

            compact_retained_tail: args.compact_retained_tail,

            compact_max_input_bytes: args.compact_max_input_bytes,

            context_window,

            auto_compact_threshold_percent: args.auto_compact_threshold_percent,

            max_iterations: args.max_iterations,

            retry_max_attempts: args.retry_max_attempts,

            retry_base_delay: Duration::from_millis(args.retry_base_delay_ms),

            retry_max_delay: Duration::from_millis(args.retry_max_delay_ms),

            checkpoint_dir: args.checkpoint_dir,

            session_log_dir: args.session_log_dir,

            session_log_durable: args.session_log_durable,

            session_headers: args.session_headers,

            compaction_mode,
            file: resolved,
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

    /// Returns the reported input-token threshold for automatic compaction.
    ///
    /// The threshold leaves room for the compaction summary output. Zero-percent
    /// configuration disables the feature.
    #[must_use]
    pub fn auto_compact_threshold_tokens(&self) -> Option<u64> {
        ee_agent_orchestrator::compaction_threshold_tokens(
            self.context_window,
            self.auto_compact_threshold_percent,
        )
    }

    /// Looks a variable up in the process environment first, then in the
    /// parsed `.env` map; empty values count as unset.
    pub fn env_or_dotenv(name: &str, dotenv: &BTreeMap<String, String>) -> Option<String> {
        ee_agent_acp_server::env_or_dotenv(name, dotenv)
    }

    /// Model id without the optional `#variant` selector.
    #[must_use]
    pub fn base_model(&self) -> &str {
        base_model_id(&self.model)
    }

    /// Variant id requested by `OPENROUTER_MODEL` after `#`, when present.
    #[must_use]
    pub fn variant_id(&self) -> Option<&str> {
        self.model
            .rsplit_once('#')
            .map(|(_, variant)| variant.trim())
            .filter(|variant| !variant.is_empty())
    }

    /// Catalog row for the base model id, when models.dev lists it.
    #[must_use]
    pub fn catalog_entry(&self) -> Option<&'static crate::catalog::CatalogModel> {
        crate::catalog::find(self.base_model())
    }
}

/// Strips a trailing `#variant` and trims the model id.
pub(crate) fn base_model_id(model: &str) -> &str {
    match model.rsplit_once('#') {
        Some((base, _)) => base.trim(),
        None => model.trim(),
    }
}

/// Picks a deterministic default model when none was configured.
///
/// Walks the catalog's active chat models in id order and takes the first the
/// configuration file has not disabled. Falls back to [`DEFAULT_MODEL`] when the
/// catalog is empty, which keeps a generated-table defect from making startup
/// impossible.
fn default_model_for(file: &crate::config_file::FileConfig) -> String {
    crate::catalog::chat_model_ids()
        .into_iter()
        .find(|id| !file.resolve(id).disabled)
        .map(str::to_string)
        .unwrap_or_else(|| String::from(DEFAULT_MODEL))
}

#[cfg(test)]
mod tests;
