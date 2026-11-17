//! Catalog-driven route validation and reasoning overlays for OpenRouter.
//!
//! These helpers turn a resolved catalog row plus the configured variant into
//! request decisions before any body exists. They never select an endpoint: the
//! endpoint comes from the session configuration, and a row can only reject a
//! model or route the catalog does not advertise.

use ee_agent_orchestrator::{
    GenerationOptions, ReasoningProtocol, ReasoningVariant, SamplingSupport, find_variant,
    variant_ids,
};
use serde_json::{Value, json};

use crate::catalog::CatalogModel;
use crate::config::{Config, OpenRouterApiKind};
use crate::options::REASONING_EFFORTS;

/// Refuses a catalog row or route the session path cannot serve.
///
/// Unknown ids keep working with explicit configuration; a known non-chat row
/// fails before any request and names the missing feature.
pub(crate) fn validate_route(
    config: &Config,
    entry: Option<&'static CatalogModel>,
) -> Result<(), String> {
    let Some(entry) = entry else {
        return Ok(());
    };
    if !entry.is_chat() {
        let hint = match entry.kind {
            ee_agent_orchestrator::ModelKind::Decision => {
                "the System One decision protocol is not implemented by ee-agent-openrouter"
            }
            ee_agent_orchestrator::ModelKind::Image => {
                "image generation is not exposed by ee-agent-openrouter"
            }
            ee_agent_orchestrator::ModelKind::Embedding => {
                "embeddings are not exposed by ee-agent-openrouter"
            }
            ee_agent_orchestrator::ModelKind::Chat => "",
        };
        return Err(format!(
            "OpenRouter model {:?} is a {} model: {hint}. Select a chat model instead",
            entry.id,
            entry.kind.as_str()
        ));
    }
    match config.api_kind {
        OpenRouterApiKind::Chat if !entry.routes.chat => Err(format!(
            "OpenRouter model {:?} does not advertise the /chat/completions route: set \
             OPENROUTER_API_KIND=responses or select a chat model",
            entry.id
        )),
        OpenRouterApiKind::Responses if !entry.routes.responses => Err(format!(
            "OpenRouter model {:?} does not advertise the /responses route: set \
             OPENROUTER_API_KIND=chat or select a model that lists responses",
            entry.id
        )),
        _ => Ok(()),
    }
}

/// Rejects a sampling knob the route does not document.
///
/// Only the knobs the selected dialect actually lowers are checked, so a knob the
/// route never sends cannot fail the configuration for no reason.
pub(crate) fn validate_generation(
    kind: OpenRouterApiKind,
    model: &str,
    sampling: SamplingSupport,
    generation: &GenerationOptions,
) -> Result<(), String> {
    let applicable = match kind {
        OpenRouterApiKind::Chat => generation.clone(),
        OpenRouterApiKind::Responses => GenerationOptions {
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            ..generation.clone()
        },
    };
    generation.validate().and_then(|()| match applicable.unsupported(sampling) {
        Some(knob) => Err(format!(
            "OpenRouter model {model:?} does not document the {knob} sampling knob: unset it \
                 or select a model that declares {knob} support"
        )),
        None => Ok(()),
    })
}

/// Resolves the requested `model#variant` against the catalog row.
pub(crate) fn resolved_variant(
    config: &Config,
    entry: Option<&'static CatalogModel>,
) -> Result<Option<ReasoningVariant>, String> {
    let Some(id) = config.variant_id() else {
        return Ok(None);
    };
    let protocol = match config.api_kind {
        OpenRouterApiKind::Chat => ReasoningProtocol::OpenAiChat,
        OpenRouterApiKind::Responses => ReasoningProtocol::OpenAiResponses,
    };
    if let Some(entry) = entry {
        let variants = entry.variants(protocol);
        return find_variant(&variants, id).copied().map(Some).ok_or_else(|| {
            format!(
                "model {:?} has no reasoning variant {id:?}: expected one of {}",
                entry.id,
                variant_ids(&variants)
            )
        });
    }
    // Unknown ids cannot be validated against a catalog row; accept only the
    // documented effort names so a typo still fails at startup.
    let effort = REASONING_EFFORTS.iter().copied().find(|value| *value == id).ok_or_else(|| {
        format!(
            "model {:?} is not in the OpenRouter catalog, so only documented effort variants are \
             accepted: expected one of {}",
            config.base_model(),
            REASONING_EFFORTS.join(", ")
        )
    })?;
    Ok(Some(ReasoningVariant {
        id: effort,
        effort: (effort != "none").then_some(effort),
        budget_tokens: None,
        disabled: effort == "none",
    }))
}

/// Lowers one variant into the OpenRouter reasoning object.
pub(crate) fn variant_reasoning(
    variant: ReasoningVariant,
    max_output_tokens: Option<u64>,
) -> Value {
    if variant.disabled {
        return json!({ "enabled": false });
    }
    if let Some(budget) = variant.budget_tokens {
        // The thinking budget must stay below the output bound; 1,024 is the
        // Anthropic floor upstreams enforce.
        let fitted = max_output_tokens.map_or(budget, |output| {
            budget.min(output.saturating_sub(1)).max(1_024).min(output.saturating_sub(1))
        });
        return json!({ "max_tokens": fitted.max(1) });
    }
    json!({ "effort": variant.effort.unwrap_or(variant.id) })
}

/// Merges a variant overlay into the extension object's `reasoning` key.
pub(crate) fn merge_reasoning(extensions: &mut Value, reasoning: Value) {
    let Some(object) = extensions.as_object_mut() else {
        return;
    };
    match object.get_mut("reasoning").and_then(Value::as_object_mut) {
        Some(existing) => {
            if let Some(incoming) = reasoning.as_object() {
                for (key, value) in incoming {
                    existing.insert(key.clone(), value.clone());
                }
            }
        }
        None => {
            object.insert(String::from("reasoning"), reasoning);
        }
    }
}

/// Keeps a configured thinking budget below the catalog output ceiling.
///
/// OpenRouter forwards `reasoning.max_tokens` to the upstream as the thinking
/// budget, and upstreams reject a budget that is not below the output limit. A
/// budget that cannot clear the 1,024-token floor is dropped instead of sending
/// a request the provider would reject.
pub(crate) fn clamp_reasoning_budget(extensions: &mut Value, output_limit: u64) {
    let Some(budget) = extensions
        .get_mut("reasoning")
        .and_then(Value::as_object_mut)
        .and_then(|reasoning| reasoning.get_mut("max_tokens"))
    else {
        return;
    };
    let Some(value) = budget.as_u64() else {
        return;
    };
    let ceiling = output_limit.saturating_sub(1);
    if value <= ceiling {
        return;
    }
    if ceiling < 1_024 {
        *budget = Value::Null;
        if let Some(reasoning) = extensions.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning.remove("max_tokens");
        }
        return;
    }
    *budget = json!(ceiling);
}
