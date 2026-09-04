//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

#[cfg(any(feature = "agents", test))]
use super::discovery::{
    ConfigEnvironment, ConfigLayerKind, discover_config_layers_with_env, probe_config_file,
};
#[cfg(any(feature = "agents", test))]
use super::raw::{AgentWebContextToml, WebContextLimitsToml, parse_ee_toml};
#[cfg(any(feature = "agents", test))]
use super::web_context::{
    validate_agent_web_context_config, web_search_provider, web_search_provider_options,
};
#[cfg(any(feature = "agents", test))]
use std::path::Path;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{AgentWebContextConfig, WebContextLimits};

#[cfg(any(feature = "agents", test))]
pub(super) fn resolve_agent_web_context_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> AgentWebContextConfig {
    let mut config = AgentWebContextConfig::default();

    if let Some(web_context) = user_global_agent_web_context_toml(env)
        && let Err(err) = merge_user_global_web_context(&mut config, &web_context)
    {
        eprintln!("ee: warning: invalid user-global agents.web_context config: {err}");
    }

    for layer in discover_config_layers_with_env(env, file_path)
        .layers
        .into_iter()
        .filter(|layer| matches!(layer.kind, ConfigLayerKind::Ancestor))
    {
        let Some(patch) = parse_ee_toml(&layer.path) else {
            continue;
        };
        let Some(web_context) = patch.agents.and_then(|agents| agents.web_context) else {
            continue;
        };
        restrict_workspace_web_context(&mut config, &web_context, &layer.path);
    }

    config
}

#[cfg(any(feature = "agents", test))]
fn user_global_agent_web_context_toml(env: &ConfigEnvironment) -> Option<AgentWebContextToml> {
    let user_config = env
        .xdg_user_config_path()
        .filter(|path| probe_config_file(path).exists)
        .or_else(|| env.legacy_user_config_path().filter(|path| probe_config_file(path).exists))?;
    parse_ee_toml(&user_config)?.agents?.web_context
}

#[cfg(any(feature = "agents", test))]
fn merge_user_global_web_context(
    config: &mut AgentWebContextConfig,
    patch: &AgentWebContextToml,
) -> Result<(), String> {
    validate_agent_web_context_config(patch)?;

    let mut limits = config.limits.clone();
    apply_user_global_web_context_limits(&mut limits, patch.limits.as_ref())?;

    if let Some(enabled) = patch.enabled {
        config.enabled = enabled;
    }
    if let Some(backend) = patch.backend {
        config.provider = web_search_provider(backend);
        config.provider_options = web_search_provider_options(backend, patch);
        config.search_endpoint = patch.endpoint.clone();
    }
    if let Some(hosts) = &patch.hosts {
        config.preapproved_hosts = hosts.clone();
    }
    if let Some(reference) = &patch.provider_secret_reference {
        config.provider_secret_reference = Some(reference.clone());
    }
    if let Some(account_id) = &patch.browser_run_account_id {
        config.browser_run_account_id = Some(account_id.clone());
    }
    if let Some(reference) = &patch.browser_run_api_token_reference {
        config.browser_run_api_token_reference = Some(reference.clone());
    }
    if let Some(max_attempts) = patch.browser_run_max_attempts {
        config.browser_run_retry.max_attempts = max_attempts;
    }
    if let Some(base_delay_ms) = patch.browser_run_base_delay_ms {
        config.browser_run_retry.base_delay_ms = base_delay_ms;
    }
    if let Some(max_delay_ms) = patch.browser_run_max_delay_ms {
        config.browser_run_retry.max_delay_ms = max_delay_ms;
    }
    config.limits = limits;
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn apply_user_global_web_context_limits(
    limits: &mut WebContextLimits,
    patch: Option<&WebContextLimitsToml>,
) -> Result<(), String> {
    let Some(patch) = patch else {
        return Ok(());
    };

    if let Some(value) = patch.max_response_bytes {
        validate_web_context_limit(
            "max_response_bytes",
            value,
            ee_agent_host::web_context::MAX_RESPONSE_BYTES,
            false,
        )?;
        limits.max_response_bytes = value;
    }
    if let Some(value) = patch.max_text_bytes {
        validate_web_context_limit(
            "max_text_bytes",
            value,
            ee_agent_host::web_context::MAX_TEXT_BYTES,
            false,
        )?;
        limits.max_text_bytes = value;
    }
    if let Some(value) = patch.max_search_results {
        validate_web_context_limit(
            "max_search_results",
            value,
            ee_agent_host::web_context::MAX_SEARCH_RESULTS,
            false,
        )?;
        limits.max_search_results = value;
    }
    if let Some(value) = patch.max_redirects {
        validate_web_context_limit(
            "max_redirects",
            value,
            ee_agent_host::web_context::MAX_REDIRECTS,
            true,
        )?;
        limits.max_redirects = value;
    }
    if let Some(value) = patch.request_timeout_ms {
        if value == 0 || value > ee_agent_host::web_context::MAX_REQUEST_TIMEOUT_MS {
            return Err(String::from("request_timeout_ms must be within supported bounds"));
        }
        limits.request_timeout_ms = value;
    }
    if let Some(value) = patch.max_concurrent_requests {
        validate_web_context_limit(
            "max_concurrent_requests",
            value,
            ee_agent_host::web_context::MAX_CONCURRENT_REQUESTS,
            false,
        )?;
        limits.max_concurrent_requests = value;
    }

    if limits.max_text_bytes > limits.max_response_bytes {
        limits.max_text_bytes = limits.max_response_bytes;
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn validate_web_context_limit(
    name: &str,
    value: usize,
    maximum: usize,
    zero_allowed: bool,
) -> Result<(), String> {
    if (!zero_allowed && value == 0) || value > maximum {
        return Err(format!("{name} must be within supported bounds"));
    }
    Ok(())
}

#[cfg(any(feature = "agents", test))]
fn restrict_workspace_web_context(
    config: &mut AgentWebContextConfig,
    patch: &AgentWebContextToml,
    path: &Path,
) {
    if patch.enabled == Some(true) {
        eprintln!(
            "ee: warning: ignoring agents.web_context.enabled = true in workspace config {}",
            path.display()
        );
    }
    if patch.enabled == Some(false) {
        config.enabled = false;
    }
    if patch.backend.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.backend in workspace config {}",
            path.display()
        );
    }
    if patch.exa.is_some() || patch.brave_llm_context.is_some() || patch.tavily.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context provider options in workspace config {}",
            path.display()
        );
    }
    if patch.endpoint.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.endpoint in workspace config {}",
            path.display()
        );
    }
    if patch.provider_secret_reference.is_some() {
        eprintln!(
            "ee: warning: ignoring agents.web_context.provider_secret_reference outside user-global config in {}",
            path.display()
        );
    }
    if patch.browser_run_account_id.is_some()
        || patch.browser_run_api_token_reference.is_some()
        || patch.browser_run_max_attempts.is_some()
        || patch.browser_run_base_delay_ms.is_some()
        || patch.browser_run_max_delay_ms.is_some()
    {
        eprintln!(
            "ee: warning: ignoring agents.web_context Browser Run configuration outside user-global config in {}",
            path.display()
        );
    }
    if let Some(hosts) = &patch.hosts {
        if !hosts.is_subset(&config.preapproved_hosts) {
            eprintln!(
                "ee: warning: ignoring workspace agents.web_context hosts not approved by user-global config in {}",
                path.display()
            );
        }
        config.preapproved_hosts = config.preapproved_hosts.intersection(hosts).cloned().collect();
    }
    restrict_workspace_web_context_limits(&mut config.limits, patch.limits.as_ref(), path);
}

#[cfg(any(feature = "agents", test))]
fn restrict_workspace_web_context_limits(
    limits: &mut WebContextLimits,
    patch: Option<&WebContextLimitsToml>,
    path: &Path,
) {
    let Some(patch) = patch else {
        return;
    };

    restrict_web_context_limit(
        "max_response_bytes",
        &mut limits.max_response_bytes,
        patch.max_response_bytes,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_text_bytes",
        &mut limits.max_text_bytes,
        patch.max_text_bytes,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_search_results",
        &mut limits.max_search_results,
        patch.max_search_results,
        false,
        path,
    );
    restrict_web_context_limit(
        "max_redirects",
        &mut limits.max_redirects,
        patch.max_redirects,
        true,
        path,
    );
    restrict_web_context_u64_limit(
        "request_timeout_ms",
        &mut limits.request_timeout_ms,
        patch.request_timeout_ms,
        path,
    );
    restrict_web_context_limit(
        "max_concurrent_requests",
        &mut limits.max_concurrent_requests,
        patch.max_concurrent_requests,
        false,
        path,
    );
    limits.max_text_bytes = limits.max_text_bytes.min(limits.max_response_bytes);
}

#[cfg(any(feature = "agents", test))]
fn restrict_web_context_limit(
    name: &str,
    current: &mut usize,
    requested: Option<usize>,
    zero_allowed: bool,
    path: &Path,
) {
    let Some(requested) = requested else {
        return;
    };
    if !zero_allowed && requested == 0 {
        eprintln!(
            "ee: warning: ignoring invalid workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else if requested > *current {
        eprintln!(
            "ee: warning: ignoring widening workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else {
        *current = requested;
    }
}

#[cfg(any(feature = "agents", test))]
fn restrict_web_context_u64_limit(
    name: &str,
    current: &mut u64,
    requested: Option<u64>,
    path: &Path,
) {
    let Some(requested) = requested else {
        return;
    };
    if requested == 0 {
        eprintln!(
            "ee: warning: ignoring invalid workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else if requested > *current {
        eprintln!(
            "ee: warning: ignoring widening workspace agents.web_context.limits.{name} in {}",
            path.display()
        );
    } else {
        *current = requested;
    }
}
