//! Critic (rubber-duck) model resolution and the production multi-model provider.
//!
//! One OpenCode process serves exactly one root route. A second opinion needs a
//! second model, so `OPENCODE_CRITIC_MODEL` names one more catalog id **on the
//! same surface**; the critic gets its own adapter for its own route and its own
//! dialect, and the orchestrator registry pairs the two only when their
//! declared vendor families differ ([`crate::family`]).
//!
//! Every rejection here is a bounded, credential-free diagnostic and never a
//! startup failure: an unusable critic degrades to root-only operation with one
//! warning, exactly like the OpenRouter agent. Routing, credentials, and the
//! root model are unaffected by anything in this module.

use std::path::PathBuf;
use std::sync::Arc;

use ee_agent_orchestrator::{
    DEFAULT_MODEL_ID, ModelAdapter, ModelCapability, ModelFamily, ModelIdentity, ModelRegistration,
    ModelRegistry, ModelTier, OrchestratorProvider, RUBBER_DUCK_ROLE,
};

use crate::adapter::{OpenCodeModelAdapter, opencode_orchestrator_config};
use crate::config::Config;
use crate::family;
use crate::routes::{self, OpenCodeRoute};

/// Capabilities declared for both root and critic routes: the codecs speak chat,
/// tools, and streaming for every dialect.
const ROUTE_CAPABILITIES: [ModelCapability; 3] =
    [ModelCapability::ChatCompletion, ModelCapability::Tools, ModelCapability::Streaming];

/// Resolves the configured critic route.
///
/// # Errors
///
/// Returns the bounded reason the critic is unavailable: no model configured,
/// an id that is not an exact catalog entry on the root surface, the root model
/// itself, or a model whose declared vendor family matches the root's.
pub fn resolve_critic(config: &Config) -> Result<OpenCodeRoute, String> {
    let Some(model_id) = config.critic_model.as_deref().map(str::trim).filter(|id| !id.is_empty())
    else {
        return Err(String::from("no critic model configured"));
    };
    let route = routes::resolve_route(config.route.surface, model_id).map_err(|error| {
        format!("critic model {model_id:?} is not usable on this surface: {error}")
    })?;
    if route.model_id == config.route.model_id {
        return Err(String::from("critic model id must differ from the root model id"));
    }
    let root_family = family::route_family(&config.route).ok_or_else(|| {
        format!("root model {} has no declared vendor family", config.route.model_id)
    })?;
    let critic_family = family::route_family(&route)
        .ok_or_else(|| format!("critic model {} has no declared vendor family", route.model_id))?;
    if critic_family == root_family {
        return Err(format!(
            "critic vendor family {} must differ from the root vendor family {}",
            family::family_label(&route),
            family::family_label(&config.route)
        ));
    }
    Ok(route)
}

/// Builds the production provider: root route plus the configured critic when it
/// is usable.
///
/// # Errors
///
/// Returns a diagnostic only when the root route itself cannot be built; an
/// unusable critic becomes the returned warning instead.
pub fn opencode_multi_model_provider(
    config: &Config,
    session_state_dir: PathBuf,
) -> Result<(OrchestratorProvider, Option<String>), String> {
    let root = OpenCodeModelAdapter::new(config)?;
    let (critic, warning) = match resolve_critic(config) {
        Err(reason) => (None, Some(format!("rubber duck unavailable: {reason}"))),
        Ok(route) => {
            let critic_config = Config { route, ..config.clone() };
            match OpenCodeModelAdapter::new(&critic_config) {
                Ok(adapter) => (Some((route, adapter)), None),
                Err(error) => (
                    None,
                    Some(format!("rubber duck unavailable: invalid critic configuration: {error}")),
                ),
            }
        }
    };
    let provider = provider_from_adapters(
        config,
        session_state_dir,
        Arc::new(root),
        critic.map(|(route, adapter)| (route, Arc::new(adapter) as Arc<dyn ModelAdapter>)),
    )?;
    Ok((provider, warning))
}

/// Builds the production provider over injected dialect codecs (tests).
///
/// The root and critic routes are resolved by the caller, so a scripted codec
/// cannot make the process claim a route the catalog does not serve.
///
/// # Errors
///
/// Returns a diagnostic when the root registry entry cannot be built; an
/// unusable critic becomes the returned warning instead.
#[cfg(any(test, feature = "test-utils"))]
pub fn opencode_multi_model_provider_with_codecs(
    config: &Config,
    session_state_dir: PathBuf,
    root: (OpenCodeRoute, Arc<dyn crate::adapter::DialectCodec>),
    critic: Option<(OpenCodeRoute, Arc<dyn crate::adapter::DialectCodec>)>,
) -> Result<(OrchestratorProvider, Option<String>), String> {
    let (root_route, root_codec) = root;
    let token = config.token_source();
    let root = OpenCodeModelAdapter::with_codec(root_route, token.clone(), root_codec);
    let (critic, warning) = match critic {
        None => (None, None),
        Some((route, codec)) => {
            let adapter = OpenCodeModelAdapter::with_codec(route, token, codec);
            (Some((route, Arc::new(adapter) as Arc<dyn ModelAdapter>)), None)
        }
    };
    let provider = provider_from_adapters(config, session_state_dir, Arc::new(root), critic)?;
    Ok((provider, warning))
}

/// Assembles the provider from resolved adapters, sharing one code path for the
/// production build and for tests.
fn provider_from_adapters(
    config: &Config,
    session_state_dir: PathBuf,
    root: Arc<dyn ModelAdapter>,
    critic: Option<(OpenCodeRoute, Arc<dyn ModelAdapter>)>,
) -> Result<OrchestratorProvider, String> {
    let registry = registry_with_root(root, config.route, critic)?;
    OrchestratorProvider::with_model_registry(
        opencode_orchestrator_config(config, session_state_dir),
        registry,
        ee_agent_orchestrator::default_agent_policy(),
    )
    .map_err(|error| error.to_string())
}

/// Registers the root route and, when present, the critic route.
///
/// The critic is registered under [`RUBBER_DUCK_ROLE`], which is what the
/// orchestrator's contrast selection looks up before it falls back to any other
/// registered model.
fn registry_with_root(
    root: Arc<dyn ModelAdapter>,
    root_route: OpenCodeRoute,
    critic: Option<(OpenCodeRoute, Arc<dyn ModelAdapter>)>,
) -> Result<ModelRegistry, String> {
    let mut registry = ModelRegistry::new();
    registry
        .register_model(
            DEFAULT_MODEL_ID,
            root,
            ModelRegistration::new(route_identity(&root_route)?).tier(ModelTier::Strong),
        )
        .map_err(|error| format!("invalid root model registration: {error}"))?;
    if let Some((route, adapter)) = critic {
        registry
            .register_model(
                RUBBER_DUCK_ROLE,
                adapter,
                ModelRegistration::new(route_identity(&route)?)
                    .for_roles(&[RUBBER_DUCK_ROLE])
                    .tier(ModelTier::Strong),
            )
            .map_err(|error| format!("invalid critic model registration: {error}"))?;
    }
    Ok(registry)
}

/// Declared identity for one route: catalog id, provider, vendor family.
fn route_identity(route: &OpenCodeRoute) -> Result<ModelIdentity, String> {
    let family = declared_family(route)?;
    ModelIdentity::new(route.model_id, "opencode", family, route.model_id, ROUTE_CAPABILITIES)
        .map_err(|error| error.to_string())
}

/// Declared vendor family, with a bounded error for a table gap.
fn declared_family(route: &OpenCodeRoute) -> Result<ModelFamily, String> {
    family::route_family(route)
        .ok_or_else(|| format!("model {} has no declared vendor family", route.model_id))
}

#[cfg(test)]
mod tests;
