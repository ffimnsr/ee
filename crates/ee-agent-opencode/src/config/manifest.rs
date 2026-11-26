//! Setup manifest emitted by `ee-agent-opencode --ee-config`.
//!
//! Kept out of `config.rs` so the configuration module stays below the
//! repository's 1,000-line threshold. Every default here is deliberate: a value
//! with no default must be supplied explicitly by the host.
//!
//! Setup clients render `choices` as numbered selections. Model, critic, and
//! approval-classifier choices carry a `when` condition on the `surface` input
//! so the host can show only the ids valid for the surface the user just picked,
//! and they stay curated: the complete catalog is fetched on demand through
//! `--ee-config-choices` (see [`remote_choices`]) so the manifest stays small.
//! Reasoning effort has no static list at all: its values are the selected
//! model's declared variants, fetched the same way after `OPENCODE_MODEL` exists.

use ee_agent_protocol::setup::{SetupChoice, SetupChoiceWhen};

use super::{
    API_KEY_DESCRIPTION, API_KEY_ENV, APPROVAL_CLASSIFIER_ENV, APPROVAL_CLASSIFIER_MODEL_ENV,
    CRITIC_LABEL, CRITIC_MODEL_ENV, DEFAULT_RETRY_BASE_DELAY_MS, DEFAULT_RETRY_MAX_ATTEMPTS,
    DEFAULT_RETRY_MAX_DELAY_MS, DEFAULT_SYSTEM_PROMPT, MODEL_ENV, MODEL_LABEL,
    REASONING_EFFORT_ENV, RUBBER_DUCK_MODE_ENV, SETUP_MANIFEST_SCHEMA_VERSION, SURFACE_ENV,
    SURFACE_LABEL, SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig, SetupManifest,
};
use crate::routes::{self, OpenCodeSurface};

const MODEL_SECTION: &str = "Model selection";
const BEHAVIOR_SECTION: &str = "Agent behavior";
const ADVANCED_SECTION: &str = "Advanced";

/// Curated ids shown by default; the host offers the full catalog as a
/// searchable list through [`remote_choices`].
const CURATED_MODEL_LIMIT: usize = 12;

/// Representative OpenCode Zen models, seeded from the families with the most
/// generous usage allowances. Runtime ordering still sorts by cost so the
/// cheapest (most requests) appear first; ids the binary cannot route are
/// dropped and refilled from the catalog.
const ZEN_CURATED_MODELS: [&str; 12] = [
    "deepseek-v4.1-flash",
    "deepseek-v4.1-flash-free",
    "glm-5.3-flash",
    "glm-5.3-flash-free",
    "glm-5.3",
    "qwen3.8-flash",
    "qwen3.5-plus",
    "minimax-m3",
    "gpt-6-luna",
    "claude-haiku-5-5",
    "kimi-k3",
    "grok-4.7",
];

/// Representative OpenCode Go models, same rules as [`ZEN_CURATED_MODELS`].
const GO_CURATED_MODELS: [&str; 12] = [
    "deepseek-v4.1-flash",
    "glm-5.3-flash",
    "glm-5.3",
    "qwen3.8-flash",
    "qwen3.7-plus",
    "mimo-v2.6-flash",
    "claude-haiku-5-5",
    "gpt-6-luna",
    "minimax-m3.1-flash",
    "hy3",
    "kimi-k2.7-code",
    "grok-4.7",
];

/// Keys that accept a full remotely fetched catalog.
const REMOTE_CHOICE_KEYS: [&str; 3] = ["model", "critic_model", "reasoning_effort"];

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
    model.remote_choices = true;

    let mut critic =
        input("critic_model", CRITIC_LABEL, None, false, MODEL_SECTION, CRITIC_MODEL_ENV);
    critic.choices = model_choices();
    critic.remote_choices = true;

    let mut approval_classifier = input(
        "approval_classifier",
        "Tool approval classifier (off, auto; auto approves low-risk calls)",
        Some("off"),
        true,
        BEHAVIOR_SECTION,
        APPROVAL_CLASSIFIER_ENV,
    );
    approval_classifier.choices = vec![
        choice("off", "off: always ask (default)"),
        choice("auto", "auto: auto-approve low-risk tool calls; medium/high still ask"),
    ];

    let mut approval_classifier_model = input(
        "approval_classifier_model",
        "Approval classifier model slot (default: root model; rubber_duck: built-in critic model)",
        None,
        false,
        BEHAVIOR_SECTION,
        APPROVAL_CLASSIFIER_MODEL_ENV,
    );
    approval_classifier_model.choices = vec![
        choice("default", "default: root model"),
        choice("rubber_duck", "rubber_duck: configured built-in critic model"),
    ];

    // The built-in critic is independent of EE's rubber-duck critic. Keep its
    // mode behind the advanced gate so normal setup asks about EE's critic
    // first; manual remains the safe default.
    let mut rubber_duck_mode = input(
        "rubber_duck_mode",
        "Built-in critic mode (off, manual, automatic; separate from EE's rubber-duck critic)",
        Some("manual"),
        false,
        ADVANCED_SECTION,
        RUBBER_DUCK_MODE_ENV,
    );
    rubber_duck_mode.advanced = true;
    rubber_duck_mode.choices = vec![
        choice("off", "off: no built-in critic"),
        choice("manual", "manual: run only when asked"),
        choice("automatic", "automatic: run on risky turns"),
    ];

    // Effort levels are per model: the host fetches the selected model's
    // catalog variants through `remote_choices` once `OPENCODE_MODEL` is set.
    // A static list would offer values the chosen model cannot express.
    let mut reasoning_effort = input(
        "reasoning_effort",
        "Reasoning effort (blank sends the model default; choices follow the selected model)",
        None,
        false,
        BEHAVIOR_SECTION,
        REASONING_EFFORT_ENV,
    );
    reasoning_effort.remote_choices = true;

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
            approval_classifier,
            approval_classifier_model,
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
                "Durable session-log directory (defaults to user data; EE_SESSION_LOG_DISABLE=1 opts out)",
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

/// Curated catalog ids for both surfaces, each scoped to the matching surface
/// and ordered by estimated usage allowance.
fn model_choices() -> Vec<SetupChoice> {
    let mut choices = Vec::new();
    for surface in [OpenCodeSurface::Zen, OpenCodeSurface::Go] {
        for model_id in curated_model_ids(surface) {
            choices.push(SetupChoice {
                value: String::from(model_id),
                label: None,
                when: Some(SetupChoiceWhen {
                    input: String::from("surface"),
                    value: String::from(surface.as_str()),
                }),
            });
        }
    }
    choices
}

/// Returns the complete value set for a remotely fetched input key, or `None`
/// when the key does not accept remote choices.
///
/// `model` is the already collected `OPENCODE_MODEL` value; the
/// `reasoning_effort` list is scoped to that model's catalog-declared variants.
/// An empty list means the model declares no reasoning control, so the host
/// keeps the blank (provider-default) value.
#[must_use]
pub fn remote_choices(
    surface: OpenCodeSurface,
    key: &str,
    model: Option<&str>,
) -> Option<Vec<SetupChoice>> {
    if !REMOTE_CHOICE_KEYS.contains(&key) {
        return None;
    }
    if key == "reasoning_effort" {
        return Some(reasoning_effort_choices(surface, model));
    }
    Some(full_model_choices(surface))
}

/// Catalog-declared reasoning variants for one selected model.
fn reasoning_effort_choices(surface: OpenCodeSurface, model: Option<&str>) -> Vec<SetupChoice> {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return Vec::new();
    };
    let base_id = model.rsplit_once('#').map_or(model, |(base, _)| base.trim());
    let Ok(route) = routes::resolve_route(surface, base_id) else {
        return Vec::new();
    };
    route
        .variants()
        .iter()
        .map(|variant| match variant.id {
            "none" => choice(variant.id, "none: disable reasoning"),
            "thinking" => choice(variant.id, "thinking: enable the model's thinking budget"),
            id => choice(id, id),
        })
        .collect()
}

/// Curated ids for one surface: routable seed ids first (cheapest first), then
/// the cheapest remaining catalog ids until the limit is reached.
fn curated_model_ids(surface: OpenCodeSurface) -> Vec<&'static str> {
    let preferred = match surface {
        OpenCodeSurface::Zen => &ZEN_CURATED_MODELS[..],
        OpenCodeSurface::Go => &GO_CURATED_MODELS[..],
    };
    let mut ids: Vec<&'static str> =
        preferred.iter().copied().filter(|model_id| routable(surface, model_id)).collect();
    ids.dedup();
    for model_id in catalog_by_allowance(surface) {
        if ids.len() >= CURATED_MODEL_LIMIT {
            break;
        }
        if !ids.contains(&model_id) {
            ids.push(model_id);
        }
    }
    ids.sort_by_cached_key(|model_id| (allowance_rank(surface, model_id), *model_id));
    ids.truncate(CURATED_MODEL_LIMIT);
    ids
}

/// Every routable catalog id on one surface, cheapest first.
fn catalog_by_allowance(surface: OpenCodeSurface) -> Vec<&'static str> {
    let mut ids = routes::chat_model_ids(surface)
        .into_iter()
        .filter(|model_id| routable(surface, model_id))
        .collect::<Vec<_>>();
    ids.sort_by_cached_key(|model_id| (allowance_rank(surface, model_id), *model_id));
    ids
}

fn full_model_choices(surface: OpenCodeSurface) -> Vec<SetupChoice> {
    catalog_by_allowance(surface)
        .into_iter()
        .map(|model_id| SetupChoice { value: String::from(model_id), label: None, when: None })
        .collect()
}

fn routable(surface: OpenCodeSurface, model_id: &str) -> bool {
    // Offer only ids this binary can actually route; unsupported dialects
    // return a startup error and must not appear selectable.
    routes::resolve_route(surface, model_id).is_ok()
}

/// Lower is better: `None` cost ranks last. Usage allowance tracks price
/// because subscription and pay-as-you-go limits scale with model cost.
fn allowance_rank(surface: OpenCodeSurface, model_id: &str) -> u128 {
    let route = routes::resolve_route(surface, model_id).ok();
    route.and_then(|route| route.cost()).map_or(u128::MAX, |cost| {
        u128::from(cost.input_micros) + 3 * u128::from(cost.output_micros)
    })
}
