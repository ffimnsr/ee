//! Setup manifest emitted by `ee-agent-opencode --ee-config`.
//!
//! Kept out of `config.rs` so the configuration module stays below the
//! repository's 1,000-line threshold. Every default here is deliberate: a value
//! with no default must be supplied explicitly by the host.

use super::{
    API_KEY_DESCRIPTION, API_KEY_ENV, CRITIC_LABEL, CRITIC_MODEL_ENV, DEFAULT_RETRY_BASE_DELAY_MS,
    DEFAULT_RETRY_MAX_ATTEMPTS, DEFAULT_RETRY_MAX_DELAY_MS, DEFAULT_SYSTEM_PROMPT, MODEL_ENV,
    MODEL_LABEL, REASONING_EFFORT_ENV, RUBBER_DUCK_MODE_ENV, SETUP_MANIFEST_SCHEMA_VERSION,
    SURFACE_ENV, SURFACE_LABEL, SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig,
    SetupManifest,
};

/// Builds the setup manifest emitted by `ee-agent-opencode --ee-config`.
///
/// Neither surface nor model carries a default: the manifest requires an
/// explicit choice so no billable model is selected implicitly.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    SetupManifest {
        schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
        agent: SetupAgent { id: String::from("opencode"), display_name: String::from("OpenCode") },
        env_vars: vec![
            SetupEnvVar {
                name: String::from(API_KEY_ENV),
                required: true,
                secret: true,
                description: String::from(API_KEY_DESCRIPTION),
            },
            SetupEnvVar {
                name: String::from("OPENCODE_ORG_ID"),
                required: false,
                secret: false,
                description: String::from(
                    "Optional OpenCode workspace organization id used only by --usage to read subscription budgets and the Go meter; never inferred from the API key.",
                ),
            },
        ],
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
                label: String::from(
                    "Whole-exchange timeout in milliseconds (blank = unbounded; headers 300s, \
                     chunk idle 900s)",
                ),
                default: None,
                config: SetupInputConfig { env: String::from("OPENCODE_TIMEOUT_MS") },
            },
            SetupInput {
                key: String::from("max_iterations"),
                label: String::from(
                    "Maximum tool-loop iterations per prompt turn (blank = unlimited)",
                ),
                default: None,
                config: SetupInputConfig { env: String::from("OPENCODE_MAX_ITERATIONS") },
            },
            SetupInput {
                key: String::from("context_window"),
                label: String::from(
                    "Model context-window size in tokens (blank = catalog limit or default)",
                ),
                default: None,
                config: SetupInputConfig { env: String::from("OPENCODE_CONTEXT_WINDOW") },
            },
            SetupInput {
                key: String::from("max_output_tokens"),
                label: String::from("Maximum output tokens per model call (blank = catalog limit)"),
                default: None,
                config: SetupInputConfig { env: String::from("OPENCODE_MAX_OUTPUT_TOKENS") },
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
            SetupInput {
                key: String::from("session_log_dir"),
                label: String::from("Durable session-log directory (optional)"),
                default: None,
                config: SetupInputConfig { env: String::from("EE_SESSION_LOG_DIR") },
            },
        ],
    }
}
