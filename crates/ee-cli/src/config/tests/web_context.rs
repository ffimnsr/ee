use super::super::*;
use super::common::*;
use std::collections::BTreeSet;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{
    AgentWebContextConfig,
    web_context::{
        BraveLlmContextOptions, ExaSearchMode, TavilySearchOptions, WebSearchProvider,
        WebSearchProviderOptions,
    },
};
// The process cwd is process-global; lock it while mutating.
#[test]
fn agent_web_context_is_enabled_by_default() {
    assert!(EditorSettings::default().agents.web_context.enabled);

    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    let settings = load_config_with_env(None, &env);
    assert!(settings.agents.web_context.enabled);
}
#[test]
fn agent_web_context_exa_uses_defaults_and_user_global_options() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.web_context]
enabled = true
backend = "exa"
provider_secret_reference = "secret://exa-api-key"

[agents.web_context.exa]
max_results = 7
search_mode = "neural"
"#,
    );

    let settings = load_for(&env);
    let web = &settings.agents.web_context;
    assert!(web.enabled);
    assert_eq!(web.provider, WebSearchProvider::Exa);
    let WebSearchProviderOptions::Exa(options) = &web.provider_options else {
        panic!("expected Exa provider options");
    };
    assert_eq!(options.max_results, 7);
    assert_eq!(options.search_mode, ExaSearchMode::Neural);
    assert!(web.search_endpoint.is_none());
    assert_eq!(web.provider_secret_reference.as_deref(), Some("secret://exa-api-key"));

    let defaults = AgentWebContextConfig::default();
    assert_eq!(defaults.provider, WebSearchProvider::Searxng);
    assert_eq!(defaults.provider_options, WebSearchProviderOptions::Searxng);
}
#[test]
fn agent_web_context_provider_defaults_and_excluded_options_are_stable() {
    assert_eq!(web_search_provider(WebContextBackendToml::Searxng), WebSearchProvider::Searxng);
    assert_eq!(web_search_provider(WebContextBackendToml::Exa), WebSearchProvider::Exa);
    assert_eq!(
        web_search_provider(WebContextBackendToml::BraveLlmContext),
        WebSearchProvider::BraveLlmContext
    );
    assert_eq!(web_search_provider(WebContextBackendToml::Tavily), WebSearchProvider::Tavily);

    let tavily: EeToml = toml::from_str(
        "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\n",
    )
    .unwrap();
    let tavily = tavily.agents.unwrap().web_context.unwrap();
    assert_eq!(
        web_search_provider_options(WebContextBackendToml::Tavily, &tavily),
        WebSearchProviderOptions::Tavily(TavilySearchOptions::default())
    );

    let brave: EeToml = toml::from_str(
            "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\n",
        )
        .unwrap();
    let brave = brave.agents.unwrap().web_context.unwrap();
    assert_eq!(
        web_search_provider_options(WebContextBackendToml::BraveLlmContext, &brave),
        WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default())
    );

    for excluded in [
        "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\nenable_local = true\n",
        "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\nresearch = true\n",
        "[agents.web_context]\nbackend = \"exa\"\n\n[agents.web_context.exa]\ndomains = [\"example.com\"]\n",
    ] {
        assert!(toml::from_str::<EeToml>(excluded).is_err(), "excluded option parsed: {excluded}");
    }
}
#[test]
fn agent_web_context_provider_limits_fail_config_validation() {
    assert_eq!(MAX_EXA_RESULTS, ee_agent_host::web_context::MAX_EXA_RESULTS);
    assert_eq!(MAX_TAVILY_RESULTS, ee_agent_host::web_context::MAX_TAVILY_RESULTS);
    assert_eq!(
        MAX_TAVILY_CHUNKS_PER_SOURCE,
        ee_agent_host::web_context::MAX_TAVILY_CHUNKS_PER_SOURCE
    );
    assert_eq!(MAX_BRAVE_RESULTS, ee_agent_host::web_context::MAX_BRAVE_RESULTS);
    assert_eq!(MAX_BRAVE_TOKENS, ee_agent_host::web_context::MAX_BRAVE_TOKENS);
    assert_eq!(MAX_BRAVE_URLS, ee_agent_host::web_context::MAX_BRAVE_URLS);
    assert_eq!(MAX_BRAVE_SNIPPETS, ee_agent_host::web_context::MAX_BRAVE_SNIPPETS);

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    for (field, config) in [
        (
            "exa.max_results",
            "[agents.web_context]\nbackend = \"exa\"\n\n[agents.web_context.exa]\nmax_results = 0\n",
        ),
        (
            "tavily.chunks_per_source",
            "[agents.web_context]\nbackend = \"tavily\"\n\n[agents.web_context.tavily]\nchunks_per_source = 4\n",
        ),
        (
            "brave_llm_context.max_tokens",
            "[agents.web_context]\nbackend = \"brave_llm_context\"\n\n[agents.web_context.brave_llm_context]\nmax_tokens = 10001\n",
        ),
    ] {
        std::fs::write(&path, config).unwrap();
        let error = validate_config_file(&path).unwrap_err();
        assert!(error.contains(field), "{error}");
    }
}
#[test]
fn agent_web_context_rejects_vendor_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(
        &path,
        "[agents.web_context]\nbackend = \"tavily\"\nendpoint = \"https://search.example\"\n",
    )
    .unwrap();

    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("endpoint is only permitted when backend is searxng"));

    std::fs::write(&path, "[agents.web_context]\nbackend = \"searxng\"\n").unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("endpoint is required when backend is searxng"));
}
#[test]
fn agent_web_context_uses_user_global_config_across_workspace_root_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let file = env.cwd.join("project").join("main.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://search.example/search"
hosts = ["search.example", "docs.example"]

[agents.web_context.limits]
max_response_bytes = 8192
max_text_bytes = 4096
max_search_results = 12
max_redirects = 2
request_timeout_ms = 30000
max_concurrent_requests = 4
"#,
    );
    std::fs::write(
        env.cwd.join(".ee.toml"),
        r#"
root = true

[agents.web_context]
hosts = ["docs.example"]

[agents.web_context.limits]
max_text_bytes = 2048
"#,
    )
    .unwrap();

    let settings = load_config_with_env(Some(&file), &env);
    let web = &settings.agents.web_context;
    assert!(web.enabled);
    assert_eq!(web.search_endpoint.as_deref(), Some("https://search.example/search"));
    assert_eq!(web.preapproved_hosts, BTreeSet::from([String::from("docs.example")]));
    assert_eq!(web.limits.max_response_bytes, 8192);
    assert_eq!(web.limits.max_text_bytes, 2048);
    assert_eq!(web.limits.max_search_results, 12);
    assert_eq!(web.limits.max_redirects, 2);
    assert_eq!(web.limits.request_timeout_ms, 30_000);
    assert_eq!(web.limits.max_concurrent_requests, 4);
    assert!(web.provider_secret_reference.is_none());

    let rendered = toml::to_string_pretty(&resolved_config_with_env(Some(&file), &env)).unwrap();
    assert!(!rendered.contains("provider_secret_reference"));
}
#[test]
fn agent_web_context_workspace_cannot_widen_or_enable_untrusted_config() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://search.example/search"
hosts = ["search.example", "docs.example"]

[agents.web_context.limits]
max_response_bytes = 8192
max_text_bytes = 4096
max_search_results = 12
max_redirects = 2
"#,
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        r#"
[agents.web_context]
enabled = true
backend = "searxng"
endpoint = "https://untrusted.example/search"
hosts = ["docs.example", "untrusted.example"]
provider_secret_reference = "secret://workspace-provider"

[agents.web_context.limits]
max_response_bytes = 16384
max_text_bytes = 2048
max_search_results = 20
max_redirects = 3
"#,
    );

    let settings = load_config_with_env(None, &env);
    let web = &settings.agents.web_context;
    assert!(web.enabled);
    assert_eq!(web.search_endpoint.as_deref(), Some("https://search.example/search"));
    assert_eq!(web.preapproved_hosts, BTreeSet::from([String::from("docs.example")]));
    assert_eq!(web.limits.max_response_bytes, 8192);
    assert_eq!(web.limits.max_text_bytes, 2048);
    assert_eq!(web.limits.max_search_results, 12);
    assert_eq!(web.limits.max_redirects, 2);
    assert!(web.provider_secret_reference.is_none());
    assert_eq!(web.provider, WebSearchProvider::Searxng);
    assert_eq!(web.provider_options, WebSearchProviderOptions::Searxng);
}
#[test]
fn agent_web_context_workspace_cannot_change_provider_or_semantic_options() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(&env.cwd).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        r#"
[agents.web_context]
enabled = true
backend = "exa"

[agents.web_context.exa]
max_results = 7
search_mode = "neural"
"#,
    );
    write_config_layer(
        &env,
        ConfigLayerKind::Ancestor,
        r#"
[agents.web_context]
backend = "tavily"

[agents.web_context.tavily]
max_results = 99
chunks_per_source = 9
search_depth = "advanced"
"#,
    );

    let web = &load_for(&env).agents.web_context;
    assert_eq!(web.provider, WebSearchProvider::Exa);
    let WebSearchProviderOptions::Exa(options) = &web.provider_options else {
        panic!("expected Exa provider options");
    };
    assert_eq!(options.max_results, 7);
    assert_eq!(options.search_mode, ExaSearchMode::Neural);
}
#[test]
fn agent_web_context_provider_reference_from_xdg_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    write_config_layer(
        &env,
        ConfigLayerKind::UserXdg,
        "[agents.web_context]\nprovider_secret_reference = \"secret://web-provider\"\n",
    );

    let settings = load_for(&env);
    assert_eq!(
        settings.agents.web_context.provider_secret_reference.as_deref(),
        Some("secret://web-provider")
    );
    let rendered = toml::to_string_pretty(&resolved_config_with_env(None, &env)).unwrap();
    assert!(rendered.contains("secret://web-provider"));
}
#[test]
fn agent_web_context_provider_reference_from_system_or_workspace_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    let reference = "[agents.web_context]\nprovider_secret_reference = \"secret://web-provider\"\n";
    write_config_layer(&env, ConfigLayerKind::System, reference);
    write_config_layer(&env, ConfigLayerKind::Ancestor, reference);

    let settings = load_for(&env);
    assert!(settings.agents.web_context.provider_secret_reference.is_none());
}
#[test]
fn agent_web_context_rejects_malformed_provider_reference_without_echoing_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(
        &path,
        "[agents.web_context]\nprovider_secret_reference = \"secret://bad name\"\n",
    )
    .unwrap();

    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("agents.web_context.provider_secret_reference"));
    assert!(!error.contains("bad name"));
}
#[test]
fn agent_web_context_raw_request_limits_require_nonzero_values() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(
        &path,
        "[agents.web_context.limits]\nrequest_timeout_ms = 30000\nmax_concurrent_requests = 4\n",
    )
    .unwrap();
    validate_config_file(&path).unwrap();

    std::fs::write(
        &path,
        "[agents.web_context.limits]\nrequest_timeout_ms = 0\nmax_concurrent_requests = 0\n",
    )
    .unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("agents.web_context.limits.request_timeout_ms"));

    std::fs::write(
        &path,
        "[agents.web_context.limits]\nrequest_timeout_ms = 1\nmax_concurrent_requests = 0\n",
    )
    .unwrap();
    let error = validate_config_file(&path).unwrap_err();
    assert!(error.contains("agents.web_context.limits.max_concurrent_requests"));
}
