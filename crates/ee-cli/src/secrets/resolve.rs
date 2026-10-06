//! Secret-reference resolution for agent environments and MCP servers.
//!
//! References are resolved only at use time — agent launch, MCP session
//! forward, or MCP host start — after the final config merge, and only from
//! the typed values that carry their source layer. Literal values pass
//! through unchanged; any reference failure (missing, unreadable,
//! host-mismatched, corrupt) fails that consumer closed before a child
//! process or outbound request exists. Workspace-layer (`Ancestor`)
//! references additionally require the host-local workspace trust decision;
//! unknown or untrusted workspaces fail closed with
//! [`SecretStoreError::WorkspaceUntrusted`] before any secret value is read.
//!
//! Two reference shapes are supported:
//! - Exact `secret://<name>` values (agent env, MCP stdio env).
//! - Templates embedding exactly one `secret://<name>` token (MCP HTTP
//!   headers such as `Authorization = "Bearer secret://token"`).

use std::collections::BTreeMap;

use crate::config::{AgentServerSettings, ConfigLayerKind, ConfigSecretValue, McpServerSettings};

use super::{SecretName, SecretReference, SecretReferenceError, SecretStore, SecretStoreError};

/// Whether workspace-layer `secret://` references may resolve for one use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRefPolicy {
    /// The workspace is trusted: workspace-layer references resolve normally.
    Resolve,
    /// The workspace is untrusted or undecided: any workspace-layer reference
    /// aborts the consumer before a secret value is read.
    Deny,
}

/// One workspace-layer secret reference discovered in merged config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSecretReference {
    /// Dotted config location, e.g. `agents.servers.x.env.KEY` or
    /// `mcp.servers.y.headers.Authorization`.
    pub(crate) location: String,
    pub(crate) reference: String,
}

/// Workspace-layer references present in merged config (agent env, MCP
/// env/headers), ordered by location. Used by the startup trust prompt and
/// the workspace-trust CLI status; references are opaque names, never values.
pub(crate) fn workspace_secret_references(
    config: &crate::config::EditorSettings,
) -> Vec<WorkspaceSecretReference> {
    let mut references = Vec::new();
    for (server, settings) in &config.agents.servers {
        for (env_key, value) in &settings.env {
            if value.layer == ConfigLayerKind::Ancestor && value_has_reference(value) {
                references.push(WorkspaceSecretReference {
                    location: format!("agents.servers.{server}.env.{env_key}"),
                    reference: value.raw.clone(),
                });
            }
        }
    }
    for (server, settings) in &config.mcp.servers {
        match settings {
            McpServerSettings::Stdio { env, .. } => {
                for (key, value) in env {
                    if value.layer == ConfigLayerKind::Ancestor && value_has_reference(value) {
                        references.push(WorkspaceSecretReference {
                            location: format!("mcp.servers.{server}.env.{key}"),
                            reference: value.raw.clone(),
                        });
                    }
                }
            }
            McpServerSettings::StreamableHttp { headers, .. } => {
                for (key, value) in headers {
                    if value.layer == ConfigLayerKind::Ancestor && header_value_has_reference(value)
                    {
                        references.push(WorkspaceSecretReference {
                            location: format!("mcp.servers.{server}.headers.{key}"),
                            reference: value.raw.clone(),
                        });
                    }
                }
            }
        }
    }
    references.sort_by(|left, right| left.location.cmp(&right.location));
    references
}

/// Whether one value is an exact `secret://` reference.
pub(crate) fn value_has_reference(value: &ConfigSecretValue) -> bool {
    super::is_secret_reference_text(&value.raw)
}

/// Whether one header value needs store access: an exact reference, an
/// embedded token, or a malformed template that must fail closed.
pub(crate) fn header_value_has_reference(value: &ConfigSecretValue) -> bool {
    if super::is_secret_reference_text(&value.raw) {
        return true;
    }
    !matches!(embedded_reference(&value.raw), Ok(None))
}

/// Resolved reference value plus the bare secret (when the store was read)
/// so callers can feed existing redaction collection without re-reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedReference {
    pub(crate) resolved: String,
    pub(crate) secret: Option<String>,
}

/// Whether any value is an exact `secret://` reference that needs store
/// access to resolve.
pub(crate) fn values_have_references(values: &BTreeMap<String, ConfigSecretValue>) -> bool {
    values.values().any(|value| super::is_secret_reference_text(&value.raw))
}

/// Whether any header value needs store access: an exact reference, an
/// embedded reference token, or a malformed template that must fail closed
/// rather than be sent as a literal.
pub(crate) fn headers_have_references(values: &BTreeMap<String, ConfigSecretValue>) -> bool {
    values.values().any(|value| {
        if super::is_secret_reference_text(&value.raw) {
            return true;
        }
        !matches!(embedded_reference(&value.raw), Ok(None))
    })
}

/// Whether any MCP env/header value needs the secrets pipeline: an exact
/// reference to resolve, a header template to expand, or a value embedding
/// the prefix that must fail closed rather than pass through literally.
pub(crate) fn mcp_server_has_references(server: &McpServerSettings) -> bool {
    match server {
        McpServerSettings::Stdio { env, .. } => {
            values_have_references(env) || values_embed_reference(env)
        }
        McpServerSettings::StreamableHttp { headers, .. } => headers_have_references(headers),
    }
}

/// Resolves every exact reference in `values` against `store`.
///
/// Returns the final map only after ALL references resolve; any failure
/// propagates and the caller must fail closed. Secret values live in
/// zeroizing buffers inside the store and are cloned into the final map only
/// as the plain values the consumer API requires.
pub(crate) fn resolve_secret_values(
    store: &SecretStore,
    values: &BTreeMap<String, ConfigSecretValue>,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<BTreeMap<String, String>, SecretStoreError> {
    Ok(resolve_secret_values_collect(store, values, workspace_refs)?.0)
}

/// [`resolve_secret_values`] that also returns the bare secret of every
/// reference it read, so callers can feed redaction without re-reading.
///
/// Env values are exact-reference-only: a literal that embeds the prefix
/// fails closed with [`SecretReferenceError::EmbeddedNotAllowed`] instead of
/// reaching a child process as an unresolved template.
pub(crate) fn resolve_secret_values_collect(
    store: &SecretStore,
    values: &BTreeMap<String, ConfigSecretValue>,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<(BTreeMap<String, String>, Vec<String>), SecretStoreError> {
    let mut resolved = BTreeMap::new();
    let mut secrets = Vec::new();
    for (key, value) in values {
        if super::is_secret_reference_text(&value.raw) {
            let reference = resolve_exact_value(store, value, workspace_refs)?;
            secrets.extend(reference.secret);
            resolved.insert(key.clone(), reference.resolved);
        } else {
            reject_embedded_token(&value.raw).map_err(SecretStoreError::InvalidReference)?;
            resolved.insert(key.clone(), value.raw.clone());
        }
    }
    Ok((resolved, secrets))
}

/// Resolves one MCP HTTP header value: an exact `secret://<name>` value, a
/// template embedding exactly one reference token, or a literal.
pub(crate) fn resolve_embedded_reference(
    store: &SecretStore,
    value: &ConfigSecretValue,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<ResolvedReference, SecretStoreError> {
    if super::is_secret_reference_text(&value.raw) {
        return resolve_exact_value(store, value, workspace_refs);
    }
    match embedded_reference(&value.raw).map_err(SecretStoreError::InvalidReference)? {
        None => Ok(ResolvedReference { resolved: value.raw.clone(), secret: None }),
        Some(name) => {
            deny_workspace_reference(value.layer, workspace_refs)?;
            let secret = store.get(&name)?;
            let token = format!("{}{name}", super::SECRET_REFERENCE_PREFIX);
            Ok(ResolvedReference {
                resolved: value.raw.replacen(&token, secret.as_str(), 1),
                secret: Some(secret.to_string()),
            })
        }
    }
}

/// Resolves one exact `secret://<name>` value.
pub(crate) fn resolve_exact_value(
    store: &SecretStore,
    value: &ConfigSecretValue,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<ResolvedReference, SecretStoreError> {
    deny_workspace_reference(value.layer, workspace_refs)?;
    let reference =
        SecretReference::parse(&value.raw).map_err(SecretStoreError::InvalidReference)?;
    let secret = store.get(reference.name())?;
    Ok(ResolvedReference { resolved: secret.to_string(), secret: Some(secret.to_string()) })
}

fn deny_workspace_reference(
    layer: ConfigLayerKind,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<(), SecretStoreError> {
    if layer == ConfigLayerKind::Ancestor && workspace_refs == WorkspaceRefPolicy::Deny {
        return Err(SecretStoreError::WorkspaceUntrusted);
    }
    Ok(())
}

/// Finds the single `secret://<name>` token in a template value, if any.
/// More than one token, or an invalid name, is an error so the value fails
/// closed instead of reaching the server as a literal.
fn embedded_reference(raw: &str) -> Result<Option<SecretName>, SecretReferenceError> {
    let prefix = super::SECRET_REFERENCE_PREFIX;
    let Some(index) = raw.find(prefix) else {
        return Ok(None);
    };
    let after = &raw[index + prefix.len()..];
    if after.contains(prefix) {
        return Err(SecretReferenceError::MultipleReferences);
    }
    let name_len = after.find(|character| !is_secret_name_char(character)).unwrap_or(after.len());
    let name = SecretName::new(&after[..name_len]).map_err(SecretReferenceError::InvalidName)?;
    Ok(Some(name))
}

/// Grammar validation used by config-file validation and merge:
/// - exact `secret://<name>` values must parse fully;
/// - when `allow_embedded` is set (MCP headers), a value containing the
///   prefix must embed exactly one valid reference token;
/// - otherwise (agent env, MCP stdio env) a value containing the prefix is
///   rejected so a mistaken template never reaches a child process literal.
///
/// Values without the prefix are literals and always pass.
pub(crate) fn validate_secret_value(
    value: &str,
    allow_embedded: bool,
) -> Result<(), SecretReferenceError> {
    if super::is_secret_reference_text(value) {
        return SecretReference::parse(value).map(|_| ());
    }
    if !value.contains(super::SECRET_REFERENCE_PREFIX) {
        return Ok(());
    }
    if allow_embedded {
        embedded_reference(value).map(|_| ())
    } else {
        reject_embedded_token(value)
    }
}

fn is_secret_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

/// Fails closed when a value in an exact-reference context embeds the prefix.
fn reject_embedded_token(raw: &str) -> Result<(), SecretReferenceError> {
    if raw.contains(super::SECRET_REFERENCE_PREFIX) {
        return Err(SecretReferenceError::EmbeddedNotAllowed);
    }
    Ok(())
}

/// Whether any env value needs the secrets pipeline: an exact reference to
/// resolve, or a value embedding the prefix that the resolver must reject.
pub(crate) fn agent_env_has_references(env: &BTreeMap<String, ConfigSecretValue>) -> bool {
    values_have_references(env) || values_embed_reference(env)
}

/// Whether any non-exact value embeds the reference prefix.
fn values_embed_reference(values: &BTreeMap<String, ConfigSecretValue>) -> bool {
    values.values().any(|value| {
        !super::is_secret_reference_text(&value.raw)
            && value.raw.contains(super::SECRET_REFERENCE_PREFIX)
    })
}

/// Resolves every reference in `server.env` against `store`.
pub(crate) fn resolve_agent_env(
    store: &SecretStore,
    server: &AgentServerSettings,
    workspace_refs: WorkspaceRefPolicy,
) -> Result<BTreeMap<String, String>, SecretStoreError> {
    resolve_secret_values(store, &server.env, workspace_refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::test_support::StoredKeychain;
    use crate::secrets::{HostBinding, SecretName, SecretStore};
    use zeroize::Zeroizing;

    fn name(s: &str) -> SecretName {
        SecretName::new(s).expect("valid test name")
    }

    fn value(layer: ConfigLayerKind, raw: &str) -> ConfigSecretValue {
        ConfigSecretValue { layer, raw: raw.to_owned() }
    }

    fn server_with(env: BTreeMap<String, ConfigSecretValue>) -> AgentServerSettings {
        AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: Vec::new(),
            env,
            cwd: None,
        }
    }

    fn http_server_with(headers: BTreeMap<String, ConfigSecretValue>) -> McpServerSettings {
        McpServerSettings::StreamableHttp {
            url: String::from("https://mcp.example.com/mcp"),
            headers,
            timeout_ms: 30_000,
        }
    }

    fn test_binding() -> HostBinding {
        HostBinding::from_identifier_bytes(b"test-machine-id\n").expect("valid identifier")
    }

    fn store_with_secret(dir: &tempfile::TempDir) -> (SecretStore, StoredKeychain) {
        let keychain = StoredKeychain::new();
        let store = SecretStore::new(
            Box::new(keychain.clone()),
            test_binding(),
            dir.path().join("ee").join("secrets").join("v1.json"),
        );
        store
            .set(&name("openrouter-api-key"), &Zeroizing::new(String::from("sk-live-123")))
            .expect("seed secret");
        (store, keychain)
    }

    #[test]
    fn env_contexts_reject_values_that_only_embed_a_reference() {
        // Exact-reference contexts (agent env, MCP stdio env) must not let a
        // mistaken template reach a child process as an unresolved literal.
        assert_eq!(
            validate_secret_value("Bearer secret://openrouter-api-key", false),
            Err(SecretReferenceError::EmbeddedNotAllowed)
        );
        assert!(validate_secret_value("plain-literal", false).is_ok());
        assert_eq!(
            validate_secret_value("secret://", false),
            Err(SecretReferenceError::InvalidName(crate::secrets::SecretNameError::Empty))
        );
        // MCP header templates keep the embedded form. A prefix-first double
        // token is an exact-reference parse failure, not a template error.
        assert!(validate_secret_value("Bearer secret://openrouter-api-key", true).is_ok());
        assert_eq!(
            validate_secret_value("secret://a secret://b", true),
            Err(SecretReferenceError::MultiplePathSegments)
        );
        assert_eq!(
            validate_secret_value("Bearer secret://a secret://b", true),
            Err(SecretReferenceError::MultipleReferences)
        );
    }

    #[test]
    fn embedded_env_literals_fail_closed_at_resolution_too() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _keychain) = store_with_secret(&dir);
        let mut env = BTreeMap::new();
        env.insert(String::from("TOKEN"), value(ConfigLayerKind::UserXdg, "Bearer secret://x"));

        let error = resolve_secret_values(&store, &env, WorkspaceRefPolicy::Resolve)
            .expect_err("embedded template must not resolve as a literal");
        assert_eq!(
            error,
            SecretStoreError::InvalidReference(SecretReferenceError::EmbeddedNotAllowed)
        );
    }

    #[test]
    fn references_are_detected_only_for_exact_values() {
        let mut env = BTreeMap::new();
        assert!(!values_have_references(&env));
        env.insert(String::from("LANG"), value(ConfigLayerKind::UserXdg, "en_US.UTF-8"));
        assert!(!values_have_references(&env));
        env.insert(String::from("URL"), value(ConfigLayerKind::Ancestor, "https://x/secret://y"));
        assert!(
            !values_have_references(&env),
            "detection is exact-prefix-only; the resolver rejects embedded values"
        );
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::Ancestor, "secret://openrouter-api-key"),
        );
        assert!(values_have_references(&env));
        assert!(agent_env_has_references(&env));

        // Detection stays exact-only; a non-exact value embedding the prefix
        // still needs the pipeline so the resolver can reject it.
        let embedded = BTreeMap::from([(
            String::from("TOKEN"),
            value(ConfigLayerKind::UserXdg, "Bearer secret://openrouter-api-key"),
        )]);
        assert!(!values_have_references(&embedded));
        assert!(agent_env_has_references(&embedded));
    }

    #[test]
    fn resolved_openrouter_api_key_reaches_agent_process_env() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);

        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::UserXdg, "secret://openrouter-api-key"),
        );
        env.insert(String::from("LANG"), value(ConfigLayerKind::UserXdg, "en_US.UTF-8"));
        let server = AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: vec![String::from("--serve")],
            env,
            cwd: None,
        };

        let resolved = resolve_agent_env(&store, &server, WorkspaceRefPolicy::Resolve)
            .expect("all references resolve");
        assert_eq!(resolved.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-live-123"));
        assert_eq!(resolved.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
    }

    #[test]
    fn resolve_agent_env_aborts_on_missing_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::UserXdg, "secret://missing-key"),
        );
        let server = server_with(env);
        assert!(matches!(
            resolve_agent_env(&store, &server, WorkspaceRefPolicy::Resolve),
            Err(SecretStoreError::NotFound)
        ));
    }

    #[test]
    fn resolve_agent_env_aborts_on_host_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_with_secret(&dir);
        drop(store);
        // Same vault file and keychain content, different host binding: fail
        // closed with a mismatch before any value is returned.
        let other_host = HostBinding::from_identifier_bytes(b"other-machine-id").expect("valid");
        let foreign = SecretStore::new(
            Box::new(keychain),
            other_host,
            dir.path().join("ee").join("secrets").join("v1.json"),
        );
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::UserXdg, "secret://openrouter-api-key"),
        );
        let server = server_with(env);
        assert!(matches!(
            resolve_agent_env(&foreign, &server, WorkspaceRefPolicy::Resolve),
            Err(SecretStoreError::HostBindingMismatch { version: 1 })
        ));
    }

    #[test]
    fn resolve_agent_env_without_references_never_touches_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_with_secret(&dir);
        let loads_before = keychain.load_calls();

        let mut env = BTreeMap::new();
        env.insert(String::from("LANG"), value(ConfigLayerKind::Ancestor, "en_US.UTF-8"));
        let server = server_with(env);
        let resolved =
            resolve_agent_env(&store, &server, WorkspaceRefPolicy::Deny).expect("literals only");
        assert_eq!(resolved.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(keychain.load_calls(), loads_before, "no store interaction for literals");
    }

    #[test]
    fn workspace_reference_denied_before_any_secret_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_with_secret(&dir);
        let loads_before = keychain.load_calls();
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::Ancestor, "secret://openrouter-api-key"),
        );
        let server = server_with(env);

        let error = resolve_agent_env(&store, &server, WorkspaceRefPolicy::Deny)
            .expect_err("untrusted workspace denies references");
        assert_eq!(error, SecretStoreError::WorkspaceUntrusted);
        assert_eq!(keychain.load_calls(), loads_before, "deny fails before keychain access");
        assert!(error.to_string().contains("workspace is not trusted"));
        assert!(!error.to_string().contains("openrouter-api-key"));
    }

    #[test]
    fn trusted_workspace_reference_resolves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::Ancestor, "secret://openrouter-api-key"),
        );
        let resolved = resolve_agent_env(&store, &server_with(env), WorkspaceRefPolicy::Resolve)
            .expect("trusted workspace resolves");
        assert_eq!(resolved.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-live-123"));
    }

    #[test]
    fn user_layer_reference_ignores_workspace_trust() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);
        let mut env = BTreeMap::new();
        env.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::UserXdg, "secret://openrouter-api-key"),
        );
        let resolved = resolve_agent_env(&store, &server_with(env), WorkspaceRefPolicy::Deny)
            .expect("user-layer references stay user-owned");
        assert_eq!(resolved.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-live-123"));
    }

    #[test]
    fn workspace_secret_references_list_only_ancestor_references() {
        let mut config = crate::config::EditorSettings::default();
        let mut workspace_ref = BTreeMap::new();
        workspace_ref.insert(
            String::from("OPENROUTER_API_KEY"),
            value(ConfigLayerKind::Ancestor, "secret://agent.alpha.key"),
        );
        workspace_ref
            .insert(String::from("MODEL"), value(ConfigLayerKind::Ancestor, "alpha-model"));
        let mut user_ref = BTreeMap::new();
        user_ref.insert(
            String::from("OTHER_KEY"),
            value(ConfigLayerKind::UserXdg, "secret://user-owned"),
        );
        config.agents.servers.insert(String::from("zeta"), server_with(workspace_ref));
        config.agents.servers.insert(String::from("alpha"), server_with(user_ref));
        // MCP refs count too: exact stdio env and header templates.
        config.mcp.servers.insert(
            String::from("tools"),
            McpServerSettings::Stdio {
                command: String::from("mcp-tools"),
                args: Vec::new(),
                env: BTreeMap::from([(
                    String::from("GITHUB_TOKEN"),
                    value(ConfigLayerKind::Ancestor, "secret://mcp.github-token"),
                )]),
                cwd: None,
            },
        );
        config.mcp.servers.insert(
            String::from("remote"),
            McpServerSettings::StreamableHttp {
                url: String::from("https://example.com/mcp"),
                headers: BTreeMap::from([(
                    String::from("Authorization"),
                    value(ConfigLayerKind::Ancestor, "Bearer secret://mcp.remote-token"),
                )]),
                timeout_ms: 30_000,
            },
        );

        let references = workspace_secret_references(&config);
        assert_eq!(
            references,
            vec![
                WorkspaceSecretReference {
                    location: String::from("agents.servers.zeta.env.OPENROUTER_API_KEY"),
                    reference: String::from("secret://agent.alpha.key"),
                },
                WorkspaceSecretReference {
                    location: String::from("mcp.servers.remote.headers.Authorization"),
                    reference: String::from("Bearer secret://mcp.remote-token"),
                },
                WorkspaceSecretReference {
                    location: String::from("mcp.servers.tools.env.GITHUB_TOKEN"),
                    reference: String::from("secret://mcp.github-token"),
                },
            ]
        );
    }

    #[test]
    fn mcp_env_resolves_exact_references_under_trust_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);
        let env = BTreeMap::from([
            (
                String::from("API_KEY"),
                value(ConfigLayerKind::Ancestor, "secret://openrouter-api-key"),
            ),
            (String::from("LOG_LEVEL"), value(ConfigLayerKind::Ancestor, "info")),
        ]);
        let server = McpServerSettings::Stdio {
            command: String::from("mcp-server"),
            args: Vec::new(),
            env,
            cwd: None,
        };
        assert!(mcp_server_has_references(&server));
        let McpServerSettings::Stdio { env, .. } = &server else { unreachable!() };

        let denied = resolve_secret_values(&store, env, WorkspaceRefPolicy::Deny)
            .expect_err("untrusted workspace denies MCP env reference");
        assert_eq!(denied, SecretStoreError::WorkspaceUntrusted);

        let resolved = resolve_secret_values(&store, env, WorkspaceRefPolicy::Resolve)
            .expect("trusted workspace resolves MCP env");
        assert_eq!(resolved.get("API_KEY").map(String::as_str), Some("sk-live-123"));
        assert_eq!(resolved.get("LOG_LEVEL").map(String::as_str), Some("info"));
    }

    #[test]
    fn header_templates_embed_one_reference_or_fail_closed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);

        // Bearer-style template.
        let bearer = value(ConfigLayerKind::UserXdg, "Bearer secret://openrouter-api-key");
        let resolved = resolve_embedded_reference(&store, &bearer, WorkspaceRefPolicy::Deny)
            .expect("template resolves");
        assert_eq!(resolved.resolved, "Bearer sk-live-123");
        assert_eq!(resolved.secret.as_deref(), Some("sk-live-123"));
        // Exact reference.
        let exact = value(ConfigLayerKind::UserXdg, "secret://openrouter-api-key");
        let resolved = resolve_embedded_reference(&store, &exact, WorkspaceRefPolicy::Deny)
            .expect("exact resolves");
        assert_eq!(resolved.resolved, "sk-live-123");
        assert_eq!(resolved.secret.as_deref(), Some("sk-live-123"));
        // Plain literal passes through untouched with no secret to redact.
        let literal = value(ConfigLayerKind::UserXdg, "application/json");
        let resolved = resolve_embedded_reference(&store, &literal, WorkspaceRefPolicy::Deny)
            .expect("literal passes");
        assert_eq!(resolved.resolved, "application/json");
        assert_eq!(resolved.secret, None);
        // Prefix-first values are exact references: a second token is a name
        // violation, not an embedded template.
        let doubled = value(
            ConfigLayerKind::UserXdg,
            "secret://openrouter-api-key secret://openrouter-api-key",
        );
        assert!(matches!(
            resolve_embedded_reference(&store, &doubled, WorkspaceRefPolicy::Deny),
            Err(SecretStoreError::InvalidReference(_))
        ));
        // Non-prefix templates may embed only one token.
        let two_tokens = value(ConfigLayerKind::UserXdg, "Bearer secret://a and secret://b");
        assert!(matches!(
            resolve_embedded_reference(&store, &two_tokens, WorkspaceRefPolicy::Deny),
            Err(SecretStoreError::InvalidReference(SecretReferenceError::MultipleReferences))
        ));
        // Empty name fails closed.
        let empty = value(ConfigLayerKind::UserXdg, "Bearer secret://");
        assert!(matches!(
            resolve_embedded_reference(&store, &empty, WorkspaceRefPolicy::Deny),
            Err(SecretStoreError::InvalidReference(SecretReferenceError::InvalidName(_)))
        ));
        // Workspace template is gated before any read.
        let workspace = value(ConfigLayerKind::Ancestor, "Bearer secret://openrouter-api-key");
        assert!(matches!(
            resolve_embedded_reference(&store, &workspace, WorkspaceRefPolicy::Deny),
            Err(SecretStoreError::WorkspaceUntrusted)
        ));
    }

    #[test]
    fn header_reference_detection_includes_malformed_templates() {
        let headers = BTreeMap::from([(
            String::from("Authorization"),
            value(ConfigLayerKind::Ancestor, "Bearer secret://token"),
        )]);
        assert!(headers_have_references(&headers));
        assert!(mcp_server_has_references(&http_server_with(headers)));

        let plain = BTreeMap::from([(
            String::from("Content-Type"),
            value(ConfigLayerKind::Ancestor, "application/json"),
        )]);
        assert!(!headers_have_references(&plain));

        let malformed = BTreeMap::from([(
            String::from("Authorization"),
            value(ConfigLayerKind::Ancestor, "secret://a secret://b"),
        )]);
        assert!(headers_have_references(&malformed), "malformed templates still need the store");
    }
}
