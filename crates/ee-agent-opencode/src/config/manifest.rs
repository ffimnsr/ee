//! Setup manifest emitted by `ee-agent-opencode --ee-config`.
//!
//! Kept out of `config.rs` so the configuration module stays below the
//! repository's 1,000-line threshold. Every default here is deliberate: a value
//! with no default must be supplied explicitly by the host.
//!
//! Setup clients render `choices` as numbered selections. Model and critic
//! choices carry a `when` condition on the `surface` input so the host can show
//! only the catalog ids valid for the surface the user just picked.

use ee_agent_protocol::setup::{SetupChoice, SetupChoiceWhen};

use super::{
    API_KEY_DESCRIPTION, API_KEY_ENV, CRITIC_LABEL, CRITIC_MODEL_ENV, DEFAULT_RETRY_BASE_DELAY_MS,
    DEFAULT_RETRY_MAX_ATTEMPTS, DEFAULT_RETRY_MAX_DELAY_MS, DEFAULT_SYSTEM_PROMPT, MODEL_ENV,
    MODEL_LABEL, REASONING_EFFORT_ENV, RUBBER_DUCK_MODE_ENV, SETUP_MANIFEST_SCHEMA_VERSION,
    SURFACE_ENV, SURFACE_LABEL, SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig,
    SetupManifest,
};
use crate::routes::{self, OpenCodeSurface};

const MODEL_SECTION: &str = "Model selection";
const BEHAVIOR_SECTION: &str = "Agent behavior";
const ADVANCED_SECTION: &str = "Advanced";

/// Builds the setup manifest emitted by `ee-agent-opencode --ee-config`.
///
/// Neither surface nor model carries a default: the manifest requires an
/// explicit choice so no billable model is selected implicitly.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    let mut surface = input("surface", SURFACE_LABEL, None, true, MODEL_SECTION, SURFACE_ENV);
    surface.choices = surface_choices();

    let mut model = input("model", MODEL_LABEL, None, true, MODEL_SECTION, MODEL_ENV);
    model.choices = model_choices();

    let mut critic =
        input("critic_model", CRITIC_LABEL, None, false, MODEL_SECTION, CRITIC_MODEL_ENV);
    critic.choices = model_choices();

    let mut rubber_duck_mode = input(
        "rubber_duck_mode",
        "OpenCode's own rubber-duck mode (off, manual, automatic; independent of ee's critic)",
        Some("manual"),
        true,
        BEHAVIOR_SECTION,
        RUBBER_DUCK_MODE_ENV,
    );
    rubber_duck_mode.choices = vec![
        choice("off", "off: no OpenCode-internal critic"),
        choice("manual", "manual: run only when you ask"),
        choice("automatic", "automatic: run on risky turns"),
    ];

    let mut reasoning_effort = input(
        "reasoning_effort",
        "Reasoning effort (blank sends the dialect default)",
        None,
        false,
        BEHAVIOR_SECTION,
        REASONING_EFFORT_ENV,
    );
    reasoning_effort.choices =
        vec![choice("low", "low"), choice("medium", "medium"), choice("high", "high")];

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
            surface,
            model,
            critic,
            rubber_duck_mode,
            reasoning_effort,
            advanced_input(
                "system_prompt",
                "System prompt",
                Some(String::from(DEFAULT_SYSTEM_PROMPT)),
                "OPENCODE_SYSTEM_PROMPT",
            ),
            advanced_input(
                "timeout_ms",
                "Whole-exchange timeout in milliseconds (blank = unbounded; headers 300s, \
                 chunk idle 900s)",
                None,
                "OPENCODE_TIMEOUT_MS",
            ),
            advanced_input(
                "max_iterations",
                "Maximum tool-loop iterations per prompt turn (blank = unlimited)",
                None,
                "OPENCODE_MAX_ITERATIONS",
            ),
            advanced_input(
                "context_window",
                "Model context-window size in tokens (blank = catalog limit or default)",
                None,
                "OPENCODE_CONTEXT_WINDOW",
            ),
            advanced_input(
                "max_output_tokens",
                "Maximum output tokens per model call (blank = catalog limit)",
                None,
                "OPENCODE_MAX_OUTPUT_TOKENS",
            ),
            advanced_input(
                "retry_max_attempts",
                "Maximum transient retries per model call",
                Some(DEFAULT_RETRY_MAX_ATTEMPTS.to_string()),
                "OPENCODE_RETRY_MAX_ATTEMPTS",
            ),
            advanced_input(
                "retry_base_delay_ms",
                "Initial retry backoff in milliseconds",
                Some(DEFAULT_RETRY_BASE_DELAY_MS.to_string()),
                "OPENCODE_RETRY_BASE_DELAY_MS",
            ),
            advanced_input(
                "retry_max_delay_ms",
                "Retry backoff cap in milliseconds",
                Some(DEFAULT_RETRY_MAX_DELAY_MS.to_string()),
                "OPENCODE_RETRY_MAX_DELAY_MS",
            ),
            advanced_input(
                "checkpoint_dir",
                "Durable checkpoint directory for recovery (optional)",
                None,
                "EE_CHECKPOINT_DIR",
            ),
            advanced_input(
                "session_log_dir",
                "Durable session-log directory (optional)",
                None,
                "EE_SESSION_LOG_DIR",
            ),
        ],
    }
}

fn input(
    key: &str,
    label: &str,
    default: Option<&str>,
    required: bool,
    section: &str,
    env: &str,
) -> SetupInput {
    SetupInput {
        key: String::from(key),
        label: String::from(label),
        default: default.map(String::from),
        required,
        section: Some(String::from(section)),
        choices: Vec::new(),
        advanced: false,
        config: SetupInputConfig { env: String::from(env) },
    }
}

fn advanced_input(key: &str, label: &str, default: Option<String>, env: &str) -> SetupInput {
    let mut input = input(key, label, default.as_deref(), false, ADVANCED_SECTION, env);
    input.advanced = true;
    input
}

fn choice(value: &str, label: &str) -> SetupChoice {
    SetupChoice { value: String::from(value), label: Some(String::from(label)), when: None }
}

fn surface_choices() -> Vec<SetupChoice> {
    vec![
        SetupChoice {
            value: String::from(OpenCodeSurface::Zen.as_str()),
            label: Some(format!("zen: pay-as-you-go at {}", routes::ZEN_API_ROOT)),
            when: None,
        },
        SetupChoice {
            value: String::from(OpenCodeSurface::Go.as_str()),
            label: Some(format!("go: subscription plans at {}", routes::GO_API_ROOT)),
            when: None,
        },
    ]
}

/// Catalog model ids for both surfaces, each scoped to the matching surface.
fn model_choices() -> Vec<SetupChoice> {
    let mut choices = Vec::new();
    for surface in [OpenCodeSurface::Zen, OpenCodeSurface::Go] {
        for model_id in routes::chat_model_ids(surface) {
            // Offer only ids this binary can actually route; unsupported
            // dialects return a startup error and must not appear selectable.
            if routes::resolve_route(surface, model_id).is_err() {
                continue;
            }
            choices.push(SetupChoice {
                value: String::from(model_id),
                label: Some(format!("{model_id} ({})", surface.as_str())),
                when: Some(SetupChoiceWhen {
                    input: String::from("surface"),
                    value: String::from(surface.as_str()),
                }),
            });
        }
    }
    choices
}
