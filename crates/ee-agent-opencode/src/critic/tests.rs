//! Critic tests: exact-route resolution, family contrast, registry roles, and
//! the bounded degrade-to-root-only path. No test performs a network call.

use std::collections::BTreeSet;

use ee_agent_orchestrator::{ContrastUnavailable, ModelInfo};

use super::*;
use crate::adapter::test_support::{ScriptedAnswer, ScriptedCodec, test_config};
use crate::routes::{OpenCodeDialect, OpenCodeSurface};

/// Route for one exact model id on a surface.
fn route_of(surface: OpenCodeSurface, model_id: &str) -> OpenCodeRoute {
    routes::resolve_route(surface, model_id).expect("documented route")
}

fn config_with_critic(root_model: &str, critic: Option<&str>) -> Config {
    let mut config = test_config(OpenCodeSurface::Zen, root_model);
    config.critic_model = critic.map(String::from);
    config
}

/// Scripted adapter for one route.
fn scripted(route: OpenCodeRoute) -> Arc<dyn ModelAdapter> {
    let codec = Arc::new(ScriptedCodec::new(route.dialect, vec![ScriptedAnswer::text("unused")]));
    Arc::new(OpenCodeModelAdapter::with_codec(route, config_token(), codec))
}

fn config_token() -> ee_chat_completions::TokenSource {
    crate::adapter::test_support::test_token_source()
}

fn model_ids(models: &[ModelInfo]) -> Vec<String> {
    models.iter().map(|model| model.id.clone()).collect()
}

fn required_capabilities() -> BTreeSet<ModelCapability> {
    BTreeSet::from([ModelCapability::ChatCompletion, ModelCapability::Tools])
}

#[test]
fn critic_requires_one_exact_route_on_the_root_surface() {
    assert_eq!(
        resolve_critic(&config_with_critic("gpt-5.5", None)).expect_err("no critic"),
        "no critic model configured"
    );

    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("not-a-model")))
        .expect_err("unknown id");
    assert!(error.contains("is not usable on this surface"), "{error}");
    assert!(error.contains("not a documented OpenCode zen model"), "{error}");

    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("opencode/grok-4.5")))
        .expect_err("TUI alias");
    assert!(error.contains("TUI alias"), "{error}");

    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("kimi-k3")))
        .expect("kimi-k3 is documented on zen chat completions");
    assert_eq!(error.model_id, "kimi-k3");
    assert_eq!(error.dialect, OpenCodeDialect::OpenAiChatCompletions);
}

#[test]
fn critic_must_differ_from_the_root_model_and_family() {
    let error =
        resolve_critic(&config_with_critic("gpt-5.5", Some("gpt-5.5"))).expect_err("same model");
    assert!(error.contains("must differ from the root model id"), "{error}");

    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("gpt-5.6-luna")))
        .expect_err("same vendor family");
    assert!(error.contains("critic vendor family openai"), "{error}");
    assert!(error.contains("must differ from the root vendor family"), "{error}");
}

#[test]
fn critic_accepts_a_different_vendor_in_the_same_dialect() {
    let route = resolve_critic(&config_with_critic("gpt-5.5", Some("grok-4.5")))
        .expect("grok is a different declared family");

    assert_eq!(route.model_id, "grok-4.5");
    assert_eq!(route.dialect, OpenCodeDialect::OpenAiResponses);
}

#[test]
fn unsupported_and_retired_critic_ids_keep_their_guidance() {
    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("gemini-3-flash")))
        .expect_err("google dialect is unsupported");
    assert!(error.contains("does not support yet"), "{error}");

    let error = resolve_critic(&config_with_critic("gpt-5.5", Some("gpt-5.1-codex")))
        .expect_err("retired id");
    assert!(error.contains("deprecated"), "{error}");
}

#[test]
fn registry_registers_the_critic_under_the_rubber_duck_role() {
    let root_route = route_of(OpenCodeSurface::Zen, "gpt-5.5");
    let critic_route = route_of(OpenCodeSurface::Zen, "kimi-k3");

    let registry = registry_with_root(
        scripted(root_route),
        root_route,
        Some((critic_route, scripted(critic_route))),
    )
    .expect("registry builds");

    let ids = model_ids(&registry.advertised());
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&String::from(DEFAULT_MODEL_ID)), "{ids:?}");
    assert!(ids.contains(&String::from(RUBBER_DUCK_ROLE)), "{ids:?}");

    let contrast = registry
        .select_contrasting(DEFAULT_MODEL_ID, &required_capabilities())
        .expect("a different-family critic is selectable");
    assert_eq!(contrast.selected.id, RUBBER_DUCK_ROLE);
    assert_eq!(contrast.selected.identity.model_id, "kimi-k3");
    assert_eq!(contrast.active.identity.model_id, "gpt-5.5");
}

#[test]
fn registry_without_a_critic_reports_no_contrast_instead_of_guessing() {
    let root_route = route_of(OpenCodeSurface::Zen, "gpt-5.5");
    let registry = registry_with_root(scripted(root_route), root_route, None).expect("registry");

    assert_eq!(model_ids(&registry.advertised()), vec![String::from(DEFAULT_MODEL_ID)]);
    assert!(
        matches!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &required_capabilities()),
            Err(ContrastUnavailable::NoAlternative)
        ),
        "a root-only registry cannot offer contrast"
    );
}

#[test]
fn registry_refuses_a_critic_from_the_root_vendor_family() {
    let root_route = route_of(OpenCodeSurface::Zen, "gpt-5.5");
    let critic_route = route_of(OpenCodeSurface::Zen, "gpt-5.6-luna");

    let registry = registry_with_root(
        scripted(root_route),
        root_route,
        Some((critic_route, scripted(critic_route))),
    )
    .expect("registry builds");

    assert!(
        matches!(
            registry.select_contrasting(DEFAULT_MODEL_ID, &required_capabilities()),
            Err(ContrastUnavailable::SameFamilyOnly { .. })
        ),
        "the framework rule blocks a same-family critic"
    );
}

#[test]
fn production_builder_degrades_to_root_only_with_one_bounded_warning() {
    for (critic, expected) in [
        (None, "no critic model configured"),
        (Some("not-a-model"), "is not usable on this surface"),
        (Some("gpt-5.5"), "must differ from the root model id"),
        (Some("gpt-5.6-luna"), "must differ from the root vendor family"),
    ] {
        let config = config_with_critic("gpt-5.5", critic);
        let (provider, warning) = opencode_multi_model_provider(
            &config,
            PathBuf::from("/tmp/ee-opencode-critic-degrade"),
        )
        .expect("root provider still builds");

        assert_eq!(provider.registered_models().len(), 1, "root-only for {critic:?}");
        let warning = warning.expect("unavailable critic explains itself");
        assert!(warning.starts_with("rubber duck unavailable: "), "{warning}");
        assert!(warning.contains(expected), "{warning}");
        assert!(!warning.contains("sk-"), "{warning}");
    }
}

#[test]
fn production_builder_registers_the_configured_critic_route() {
    let config = config_with_critic("gpt-5.5", Some("kimi-k3"));
    let (provider, warning) =
        opencode_multi_model_provider(&config, PathBuf::from("/tmp/ee-opencode-critic-valid"))
            .expect("provider builds");

    assert!(warning.is_none(), "{warning:?}");
    let ids = model_ids(&provider.registered_models());
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&String::from(RUBBER_DUCK_ROLE)), "{ids:?}");
}

#[test]
fn critic_routes_and_codecs_are_independent_of_the_root_dialect() {
    let root_route = route_of(OpenCodeSurface::Go, "minimax-m3");
    let critic_route = route_of(OpenCodeSurface::Go, "kimi-k3");
    assert_ne!(root_route.dialect, critic_route.dialect);

    let registry = registry_with_root(
        scripted(root_route),
        root_route,
        Some((critic_route, scripted(critic_route))),
    )
    .expect("registry builds");

    let contrast = registry
        .select_contrasting(DEFAULT_MODEL_ID, &required_capabilities())
        .expect("cross-dialect critic");
    assert_eq!(contrast.selected.identity.family, family::route_family(&critic_route).unwrap());
}
