//! Setup manifest emitted by `ee-agent-openrouter --ee-config`.
//!
//! Kept out of `config.rs` so the runtime configuration module stays below the
//! repository's 1,000-line threshold. Model inputs carry a short curated list in
//! the manifest and advertise [`remote_choices`], so the host can offer the whole
//! active chat catalog through its fuzzy picker without inflating the manifest.

use ee_agent_orchestrator::{
    DEFAULT_RUBBER_DUCK_CONTEXT_BYTES, DEFAULT_RUBBER_DUCK_MAX_CALLS,
    DEFAULT_RUBBER_DUCK_OUTPUT_BYTES, DEFAULT_RUBBER_DUCK_TIMEOUT,
};
use ee_agent_protocol::setup::{
    SETUP_MANIFEST_SCHEMA_VERSION, SetupAgent, SetupChoice, SetupEnvVar, SetupInput,
    SetupInputConfig, SetupManifest,
};

use super::DEFAULT_MODEL;

const MODEL_SECTION: &str = "Model selection";
const BEHAVIOR_SECTION: &str = "Agent behavior";
const ADVANCED_SECTION: &str = "Advanced";

/// Curated ids shown by default; the host offers the full chat catalog through
/// [`remote_choices`] when a fuzzy finder is installed.
const CURATED_MODEL_LIMIT: usize = 12;

/// Representative OpenRouter chat models: current flagships plus cheaper
/// defaults across the vendors the committed catalog tracks. Ids missing from
/// the generated table are dropped and backfilled, so a catalog refresh cannot
/// make setup fail or hide the picker.
const CURATED_MODELS: [&str; CURATED_MODEL_LIMIT] = [
    "anthropic/claude-sonnet-5.5",
    "anthropic/claude-opus-5.5",
    "anthropic/claude-haiku-5.5",
    "openai/gpt-6.1-sol",
    "openai/gpt-6-luna",
    "openai/gpt-5.5",
    "google/gemini-4-argon",
    "google/gemini-3.8-flash",
    "deepseek/deepseek-v4.1-flash",
    "x-ai/grok-4.7",
    "qwen/qwen3.8-flash",
    "z-ai/glm-5.3",
];

/// Keys that accept the full remotely fetched model catalog.
const REMOTE_CHOICE_KEYS: [&str; 2] = ["model", "rubber_duck_model"];

/// Builds the setup manifest emitted by `ee-agent-openrouter --ee-config`.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    let mut model =
        input("model", "Model", Some(DEFAULT_MODEL), true, MODEL_SECTION, "OPENROUTER_MODEL");
    model.choices = curated_model_choices();
    model.remote_choices = true;

    let mut rubber_duck_model = input(
        "rubber_duck_model",
        "Rubber duck model",
        None,
        false,
        BEHAVIOR_SECTION,
        "OPENROUTER_RUBBER_DUCK_MODEL",
    );
    rubber_duck_model.choices = curated_model_choices();
    rubber_duck_model.remote_choices = true;

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
            model,
            input(
                "model_family",
                "Model family",
                None,
                false,
                MODEL_SECTION,
                "OPENROUTER_MODEL_FAMILY",
            ),
            rubber_duck_model,
            input(
                "rubber_duck_model_family",
                "Rubber duck model family",
                None,
                false,
                BEHAVIOR_SECTION,
                "OPENROUTER_RUBBER_DUCK_MODEL_FAMILY",
            ),
            {
                let mut mode = input(
                    "rubber_duck_mode",
                    "OpenRouter's own rubber-duck mode (off, manual, automatic; independent of ee's critic)",
                    Some("manual"),
                    true,
                    BEHAVIOR_SECTION,
                    "OPENROUTER_RUBBER_DUCK_MODE",
                );
                mode.choices = vec![
                    choice("off", Some("off: no agent-internal critic")),
                    choice("manual", Some("manual: run only when you ask")),
                    choice("automatic", Some("automatic: run on risky turns")),
                ];
                mode
            },
            advanced_input(
                "rubber_duck_max_calls",
                "Rubber duck maximum calls per session",
                Some(DEFAULT_RUBBER_DUCK_MAX_CALLS.to_string()),
                "OPENROUTER_RUBBER_DUCK_MAX_CALLS",
            ),
            advanced_input(
                "rubber_duck_context_bytes",
                "Rubber duck context byte limit",
                Some(DEFAULT_RUBBER_DUCK_CONTEXT_BYTES.to_string()),
                "OPENROUTER_RUBBER_DUCK_CONTEXT_BYTES",
            ),
            advanced_input(
                "rubber_duck_output_bytes",
                "Rubber duck output byte limit",
                Some(DEFAULT_RUBBER_DUCK_OUTPUT_BYTES.to_string()),
                "OPENROUTER_RUBBER_DUCK_OUTPUT_BYTES",
            ),
            advanced_input(
                "rubber_duck_timeout_ms",
                "Rubber duck timeout in milliseconds",
                Some(DEFAULT_RUBBER_DUCK_TIMEOUT.as_millis().to_string()),
                "OPENROUTER_RUBBER_DUCK_TIMEOUT_MS",
            ),
            advanced_input(
                "max_iterations",
                "Maximum iterations (blank = unlimited)",
                None,
                "OPENROUTER_MAX_ITERATIONS",
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
        remote_choices: false,
        advanced: false,
        config: SetupInputConfig { env: String::from(env) },
    }
}

fn advanced_input(key: &str, label: &str, default: Option<String>, env: &str) -> SetupInput {
    let mut input = input(key, label, default.as_deref(), false, ADVANCED_SECTION, env);
    input.advanced = true;
    input
}

fn choice(value: &str, label: Option<&str>) -> SetupChoice {
    SetupChoice { value: String::from(value), label: label.map(String::from), when: None }
}

/// Curated active chat models, backfilled from the catalog when a curated id
/// disappears so the host still sees enough entries to offer its picker.
fn curated_model_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> =
        CURATED_MODELS.iter().copied().filter(|model_id| selectable(model_id)).collect();
    if ids.len() < CURATED_MODEL_LIMIT {
        for model_id in crate::catalog::chat_model_ids() {
            if ids.len() >= CURATED_MODEL_LIMIT {
                break;
            }
            if !ids.contains(&model_id) {
                ids.push(model_id);
            }
        }
    }
    ids
}

fn curated_model_choices() -> Vec<SetupChoice> {
    curated_model_ids().into_iter().map(|model_id| choice(model_id, None)).collect()
}

/// Returns the complete active chat catalog for a remotely fetched input key,
/// or `None` when the key does not accept remote choices. Curated ids stay first
/// so the picker opens on a sensible model instead of the alphabetically first
/// third-party entry.
#[must_use]
pub fn remote_choices(key: &str) -> Option<Vec<SetupChoice>> {
    if !REMOTE_CHOICE_KEYS.contains(&key) {
        return None;
    }
    let mut ids = curated_model_ids();
    for model_id in crate::catalog::chat_model_ids() {
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
    }
    Some(ids.into_iter().map(|model_id| choice(model_id, None)).collect())
}

fn selectable(model_id: &str) -> bool {
    crate::catalog::find(model_id)
        .is_some_and(|entry| entry.is_chat() && entry.status.is_empty() && entry.routes.chat)
}
