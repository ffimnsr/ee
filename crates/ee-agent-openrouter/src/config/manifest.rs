//! Setup manifest emitted by `ee-agent-openrouter --ee-config`.
//!
//! Kept out of `config.rs` so the runtime configuration module stays below the
//! repository's 1,000-line threshold. Model inputs carry a short curated list in
//! the manifest and advertise [`remote_choices`], so the host can offer the whole
//! active chat catalog through its fuzzy picker without inflating the manifest.
//! Reasoning effort is model-scoped: its values come from the selected model's
//! catalog row, fetched after `OPENROUTER_MODEL` exists.

use ee_agent_orchestrator::ReasoningProtocol;
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

/// Keys that accept the full remotely fetched value set.
const REMOTE_CHOICE_KEYS: [&str; 3] = ["model", "rubber_duck_model", "reasoning_effort"];

/// Builds the setup manifest emitted by `ee-agent-openrouter --ee-config`.
#[must_use]
pub fn setup_manifest() -> SetupManifest {
    let mut model =
        input("model", "Model", Some(DEFAULT_MODEL), true, MODEL_SECTION, "OPENROUTER_MODEL");
    model.choices = curated_model_choices();
    model.remote_choices = true;

    let mut rubber_duck_model = input(
        "rubber_duck_model",
        "Built-in critic model (optional; separate from EE's rubber-duck critic)",
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
                "Built-in critic model family (required with a built-in critic)",
                None,
                false,
                BEHAVIOR_SECTION,
                "OPENROUTER_RUBBER_DUCK_MODEL_FAMILY",
            ),
            {
                let mut classifier = input(
                    "approval_classifier",
                    "Tool approval classifier (off, auto; auto approves low-risk calls)",
                    Some("off"),
                    true,
                    BEHAVIOR_SECTION,
                    "EE_APPROVAL_CLASSIFIER",
                );
                classifier.choices = vec![
                    choice("off", Some("off: always ask (default)")),
                    choice(
                        "auto",
                        Some("auto: auto-approve low-risk tool calls; medium/high still ask"),
                    ),
                ];
                classifier
            },
            {
                let mut classifier_model = input(
                    "approval_classifier_model",
                    "Approval classifier model slot (default: root model; rubber_duck: built-in critic model)",
                    None,
                    false,
                    BEHAVIOR_SECTION,
                    "EE_APPROVAL_CLASSIFIER_MODEL",
                );
                classifier_model.choices = vec![
                    choice("default", Some("default: root model")),
                    choice("rubber_duck", Some("rubber_duck: configured built-in critic model")),
                ];
                classifier_model
            },
            {
                // The built-in critic is independent of EE's rubber-duck critic.
                // Keep its mode behind the advanced gate so normal setup asks
                // about EE's critic first; manual remains the safe default.
                let mut mode = input(
                    "rubber_duck_mode",
                    "Built-in critic mode (off, manual, automatic; separate from EE's rubber-duck critic)",
                    Some("manual"),
                    false,
                    ADVANCED_SECTION,
                    "OPENROUTER_RUBBER_DUCK_MODE",
                );
                mode.advanced = true;
                mode.choices = vec![
                    choice("off", Some("off: no built-in critic")),
                    choice("manual", Some("manual: run only when asked")),
                    choice("automatic", Some("automatic: run on risky turns")),
                ];
                mode
            },
            {
                // Catalog rows declare which efforts each model accepts; the
                // host fetches that list for the selected `OPENROUTER_MODEL`.
                // A model without declared levels keeps the blank default.
                let mut effort = input(
                    "reasoning_effort",
                    "Reasoning effort (blank keeps the model default; choices follow the selected model)",
                    None,
                    false,
                    BEHAVIOR_SECTION,
                    "OPENROUTER_REASONING_EFFORT",
                );
                effort.remote_choices = true;
                effort
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

/// Returns the complete value set for a remotely fetched input key, or `None`
/// when the key does not accept remote choices. Curated ids stay first so the
/// picker opens on a sensible model instead of the alphabetically first
/// third-party entry.
///
/// `model` is the already collected `OPENROUTER_MODEL` value; the
/// `reasoning_effort` list is scoped to that model's catalog-declared levels.
#[must_use]
pub fn remote_choices(key: &str, model: Option<&str>) -> Option<Vec<SetupChoice>> {
    if !REMOTE_CHOICE_KEYS.contains(&key) {
        return None;
    }
    if key == "reasoning_effort" {
        return Some(reasoning_effort_choices(model));
    }
    let mut ids = curated_model_ids();
    for model_id in crate::catalog::chat_model_ids() {
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
    }
    Some(ids.into_iter().map(|model_id| choice(model_id, None)).collect())
}

/// Catalog-declared effort variants for one selected model.
///
/// The same derivation feeds the runtime selector, so setup and `/effort`
/// cannot disagree. A model that declares no reasoning support exposes no
/// values and setup keeps the blank (provider-default) input.
fn reasoning_effort_choices(model: Option<&str>) -> Vec<SetupChoice> {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return Vec::new();
    };
    let base_id = model.rsplit_once('#').map_or(model, |(base, _)| base.trim());
    let Some(entry) = crate::catalog::find(base_id) else {
        return Vec::new();
    };
    entry
        .variants(ReasoningProtocol::OpenAiChat)
        .into_iter()
        .map(|variant| {
            let label = (variant.id == "none").then_some("none: disable reasoning");
            choice(variant.id, label)
        })
        .collect()
}

fn selectable(model_id: &str) -> bool {
    crate::catalog::find(model_id)
        .is_some_and(|entry| entry.is_chat() && entry.status.is_empty() && entry.routes.chat)
}
