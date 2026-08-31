//! Agent-environment secret-reference resolution (phase 5).
//!
//! References are resolved only at agent launch, after the final config
//! merge, and only from the typed [`AgentServerSettings`] values that carry
//! their source layer. Literal values pass through unchanged; any reference
//! failure (missing, unreadable, host-mismatched, corrupt) aborts that
//! agent's launch before a child process is spawned.

use std::collections::BTreeMap;

use crate::config::{AgentEnvValue, AgentServerSettings};

use super::{SecretName, SecretReference, SecretStore, SecretStoreError};

/// Whether any env value in the server is an exact `secret://` reference.
pub(crate) fn agent_env_has_references(env: &BTreeMap<String, AgentEnvValue>) -> bool {
    env.values().any(|value| super::is_secret_reference_text(&value.raw))
}

/// Resolves every reference in `server.env` against `store`.
///
/// Returns the final process environment only after ALL references resolve;
/// any failure propagates and the caller must abort the launch. Secret values
/// live in zeroizing buffers inside the store and are cloned into the final
/// map only as the plain process-env values the child-spawn API requires.
pub(crate) fn resolve_agent_env(
    store: &SecretStore,
    server: &AgentServerSettings,
) -> Result<BTreeMap<String, String>, SecretStoreError> {
    let mut env = BTreeMap::new();
    for (key, value) in &server.env {
        if super::is_secret_reference_text(&value.raw) {
            let reference =
                SecretReference::parse(&value.raw).map_err(SecretStoreError::InvalidReference)?;
            let name: &SecretName = reference.name();
            let secret = store.get(name)?;
            env.insert(key.clone(), secret.to_string());
        } else {
            env.insert(key.clone(), value.raw.clone());
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigLayerKind;
    use crate::secrets::test_support::StoredKeychain;
    use crate::secrets::{HostBinding, SecretName, SecretStore};
    use zeroize::Zeroizing;

    fn name(s: &str) -> SecretName {
        SecretName::new(s).expect("valid test name")
    }

    fn env_value(raw: &str) -> AgentEnvValue {
        AgentEnvValue { layer: ConfigLayerKind::UserXdg, raw: raw.to_owned() }
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
    fn agent_env_has_references_detects_only_exact_references() {
        let mut env = BTreeMap::new();
        assert!(!agent_env_has_references(&env));
        env.insert(String::from("LANG"), env_value("en_US.UTF-8"));
        assert!(!agent_env_has_references(&env));
        env.insert(String::from("URL"), env_value("https://x/secret://y"));
        assert!(!agent_env_has_references(&env), "substring stays literal");
        env.insert(String::from("OPENROUTER_API_KEY"), env_value("secret://openrouter-api-key"));
        assert!(agent_env_has_references(&env));
    }

    #[test]
    fn resolved_openrouter_api_key_reaches_agent_process_env() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);

        let mut env = BTreeMap::new();
        env.insert(String::from("OPENROUTER_API_KEY"), env_value("secret://openrouter-api-key"));
        env.insert(String::from("LANG"), env_value("en_US.UTF-8"));
        env.insert(String::from("URL"), env_value("https://x/secret://y"));
        let server = AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: vec![String::from("--serve")],
            env,
            cwd: None,
        };

        let resolved = resolve_agent_env(&store, &server).expect("all references resolve");
        assert_eq!(resolved.get("OPENROUTER_API_KEY").map(String::as_str), Some("sk-live-123"));
        assert_eq!(resolved.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(
            resolved.get("URL").map(String::as_str),
            Some("https://x/secret://y"),
            "literal preserved byte-exactly"
        );

        // The resolved map is exactly what the child-spawn config receives.
        let process_config = ee_agent_host::AgentProcessConfig {
            command: server.command.clone(),
            args: server.args.clone(),
            env: resolved,
            cwd: None,
        };
        assert_eq!(
            process_config.env.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-live-123")
        );
        assert_eq!(process_config.env.len(), 3);
    }

    #[test]
    fn resolve_agent_env_aborts_on_missing_secret() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_with_secret(&dir);
        let mut env = BTreeMap::new();
        env.insert(String::from("OPENROUTER_API_KEY"), env_value("secret://missing-key"));
        let server = AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: Vec::new(),
            env,
            cwd: None,
        };
        assert!(matches!(resolve_agent_env(&store, &server), Err(SecretStoreError::NotFound)));
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
        env.insert(String::from("OPENROUTER_API_KEY"), env_value("secret://openrouter-api-key"));
        let server = AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: Vec::new(),
            env,
            cwd: None,
        };
        assert!(matches!(
            resolve_agent_env(&foreign, &server),
            Err(SecretStoreError::HostBindingMismatch { version: 1 })
        ));
    }

    #[test]
    fn resolve_agent_env_without_references_never_touches_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_with_secret(&dir);
        let loads_before = keychain.load_calls();

        let mut env = BTreeMap::new();
        env.insert(String::from("LANG"), env_value("en_US.UTF-8"));
        let server = AgentServerSettings {
            label: None,
            command: String::from("agent-bin"),
            args: Vec::new(),
            env,
            cwd: None,
        };
        let resolved = resolve_agent_env(&store, &server).expect("literals only");
        assert_eq!(resolved.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(keychain.load_calls(), loads_before, "no store interaction for literals");
    }
}
