//! Durable normal-session state storage.
//!
//! Recovery checkpoints represent interrupted turns. This store separately
//! preserves completed ACP sessions so `session/load` still works after the
//! provider process exits. Entries are scoped by provider identity, canonical
//! workspace path, and session id; files use hashed names to avoid exposing
//! workspace paths in directory entries.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;

/// Current on-disk session-state format.
const SESSION_STATE_SCHEMA_VERSION: u32 = 2;

/// Bounded, best-effort crash-consistent storage for durable ACP session snapshots.
#[derive(Debug, Clone)]
pub struct SessionStateStore {
    root: Option<PathBuf>,
    max_bytes: usize,
}

impl SessionStateStore {
    /// Creates a store. `None` preserves in-process-only provider behavior.
    #[must_use]
    pub fn new(root: Option<PathBuf>, max_bytes: usize) -> Self {
        Self { root, max_bytes }
    }

    /// Saves `state` atomically when durable storage is configured.
    pub fn save<T: Serialize>(
        &self,
        provider: &str,
        cwd: &Path,
        session_id: &str,
        state: &T,
    ) -> Result<(), OrchestratorError> {
        let Some(root) = &self.root else { return Ok(()) };
        let workspace = canonical_workspace(cwd)?;
        let state = redact_state(state)?;
        let payload = session_payload(provider, &workspace, session_id, &state)?;
        if payload.len() > self.max_bytes {
            return Err(OrchestratorError::Serialization(format!(
                "session state payload {} bytes exceeds the {} byte cap",
                payload.len(),
                self.max_bytes
            )));
        }
        let stored = StoredSession {
            schema_version: SESSION_STATE_SCHEMA_VERSION,
            provider: provider.to_string(),
            workspace,
            session_id: session_id.to_string(),
            checksum: hex(&Sha256::digest(&payload)),
            state,
        };
        let bytes = serde_json::to_vec(&stored).map_err(|error| {
            OrchestratorError::Serialization(format!("session state serialization failed: {error}"))
        })?;
        create_private_dir(root)?;
        let directory = root.join("sessions");
        create_private_dir(&directory)?;
        let final_path = directory.join(state_file_name(provider, &stored.workspace, session_id));
        let tmp_path = directory.join(format!(".tmp-{}-{}.json", std::process::id(), nonce()));
        write_private_file(&tmp_path, &bytes)?;
        std::fs::rename(&tmp_path, &final_path).map_err(|error| {
            let _ = std::fs::remove_file(&tmp_path);
            OrchestratorError::Serialization(format!(
                "failed to finalize session state {}: {error}",
                final_path.display()
            ))
        })?;
        sync_directory(&directory)
    }

    /// Loads a snapshot only when its provider, canonical workspace, session
    /// id, schema, and checksum match exactly.
    pub fn load<T: DeserializeOwned>(
        &self,
        provider: &str,
        cwd: &Path,
        session_id: &str,
    ) -> Result<Option<T>, OrchestratorError> {
        let Some(root) = &self.root else { return Ok(None) };
        let workspace = canonical_workspace(cwd)?;
        let path = root.join("sessions").join(state_file_name(provider, &workspace, session_id));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(OrchestratorError::Serialization(format!(
                    "failed to read session state {}: {error}",
                    path.display()
                )));
            }
        };
        let stored: StoredSession = serde_json::from_slice(&bytes).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "session state {} failed to deserialize: {error}",
                path.display()
            ))
        })?;
        validate_stored(&stored, provider, &workspace, session_id)?;
        serde_json::from_value(stored.state).map(Some).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "session state payload failed to deserialize: {error}"
            ))
        })
    }

    /// Lists valid durable ids in one provider/workspace scope. Used to avoid
    /// reusing `session-N` after a provider restart.
    pub fn session_ids(
        &self,
        provider: &str,
        cwd: &Path,
    ) -> Result<Vec<String>, OrchestratorError> {
        let Some(root) = &self.root else { return Ok(Vec::new()) };
        let workspace = canonical_workspace(cwd)?;
        let directory = root.join("sessions");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(OrchestratorError::Serialization(format!(
                    "failed to read session state directory {}: {error}",
                    directory.display()
                )));
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "failed to read session state entry: {error}"
                ))
            })?;
            if entry.path().extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let bytes = std::fs::read(entry.path()).map_err(|error| {
                OrchestratorError::Serialization(format!(
                    "failed to read session state entry: {error}"
                ))
            })?;
            let Ok(stored) = serde_json::from_slice::<StoredSession>(&bytes) else {
                continue;
            };
            // Identity comes from typed envelope fields. Avoid reserializing
            // `Value` here: object-key ordering can differ from the original
            // typed snapshot and would make a valid checksum look corrupt.
            // `load` validates the checksum before trusting any state.
            if stored.schema_version == SESSION_STATE_SCHEMA_VERSION
                && stored.provider == provider
                && stored.workspace == workspace
            {
                ids.push(stored.session_id);
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSession {
    schema_version: u32,
    provider: String,
    workspace: String,
    session_id: String,
    checksum: String,
    state: serde_json::Value,
}

fn validate_stored(
    stored: &StoredSession,
    provider: &str,
    workspace: &str,
    session_id: &str,
) -> Result<(), OrchestratorError> {
    if stored.schema_version != SESSION_STATE_SCHEMA_VERSION {
        return Err(OrchestratorError::Serialization(format!(
            "unsupported session state schema version {}",
            stored.schema_version
        )));
    }
    if stored.provider != provider
        || stored.workspace != workspace
        || stored.session_id != session_id
    {
        return Err(OrchestratorError::Serialization(
            "session state identity does not match requested provider, workspace, and session"
                .to_string(),
        ));
    }
    let payload = session_payload(provider, workspace, session_id, &stored.state)?;
    if hex(&Sha256::digest(&payload)) != stored.checksum {
        return Err(OrchestratorError::Serialization(
            "session state checksum mismatch (corrupt or tampered)".to_string(),
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct SessionPayload<'a, T> {
    schema_version: u32,
    provider: &'a str,
    workspace: &'a str,
    session_id: &'a str,
    state: &'a T,
}

fn session_payload<T: Serialize>(
    provider: &str,
    workspace: &str,
    session_id: &str,
    state: &T,
) -> Result<Vec<u8>, OrchestratorError> {
    serde_json::to_vec(&SessionPayload {
        schema_version: SESSION_STATE_SCHEMA_VERSION,
        provider,
        workspace,
        session_id,
        state,
    })
    .map_err(|error| {
        OrchestratorError::Serialization(format!(
            "session state payload failed to serialize: {error}"
        ))
    })
}

fn redact_state<T: Serialize>(state: &T) -> Result<serde_json::Value, OrchestratorError> {
    let mut value = serde_json::to_value(state).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "session state payload failed to serialize: {error}"
        ))
    })?;
    redact_value_strings(&mut value, None);
    Ok(value)
}

fn redact_value_strings(value: &mut serde_json::Value, field: Option<&str>) {
    const OPAQUE_FIELDS: &[&str] =
        &["arguments", "conversation", "model", "prompt", "summary", "tool_summary", "transcript"];
    match value {
        serde_json::Value::String(text)
            if field.is_some_and(|name| OPAQUE_FIELDS.contains(&name)) =>
        {
            *text = crate::sensitive_data::REDACTED.to_string();
        }
        serde_json::Value::String(text) => *text = crate::sensitive_data::redact(text),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_value_strings(item, field);
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, item) in fields {
                redact_value_strings(item, Some(name));
            }
        }
        _ => {}
    }
}

fn canonical_workspace(cwd: &Path) -> Result<String, OrchestratorError> {
    std::fs::canonicalize(cwd).map(|path| path.to_string_lossy().into_owned()).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to canonicalize session workspace {}: {error}",
            cwd.display()
        ))
    })
}

fn state_file_name(provider: &str, workspace: &str, session_id: &str) -> String {
    let mut identity = Vec::new();
    identity.extend_from_slice(provider.as_bytes());
    identity.push(0);
    identity.extend_from_slice(workspace.as_bytes());
    identity.push(0);
    identity.extend_from_slice(session_id.as_bytes());
    format!("{}.json", hex(&Sha256::digest(identity)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn create_private_dir(path: &Path) -> Result<(), OrchestratorError> {
    std::fs::create_dir_all(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create session state directory {}: {error}",
            path.display()
        ))
    })?;
    set_private_directory_permissions(path)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), OrchestratorError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create session state {}: {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to durably write session state {}: {error}",
            path.display()
        ))
    })?;
    set_private_file_permissions(path)
}

fn sync_directory(path: &Path) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        std::fs::File::open(path).and_then(|directory| directory.sync_all()).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to sync session state directory {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict session state directory {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict session state {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_state_roundtrips_in_its_workspace_scope() {
        let root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = SessionStateStore::new(Some(root.path().to_path_buf()), 1024);
        store.save("provider", workspace.path(), "session-1", &"state").expect("save");

        assert_eq!(
            store.load::<String>("provider", workspace.path(), "session-1").expect("load"),
            Some("state".to_string())
        );
        assert_eq!(
            store.session_ids("provider", workspace.path()).expect("ids"),
            vec!["session-1"]
        );
    }

    #[test]
    fn durable_state_redacts_prompt_jwt_and_api_key() {
        let root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = SessionStateStore::new(Some(root.path().to_path_buf()), 8 * 1024);
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature";
        let state = serde_json::json!({
            "prompt": "PROMPT_DO_NOT_PERSIST",
            "model": "MODEL_DO_NOT_PERSIST",
            "jwt": jwt,
            "api_key": "sk-live-1234567890",
        });
        store.save("provider", workspace.path(), "session-1", &state).expect("save");

        let workspace = canonical_workspace(workspace.path()).expect("canonical workspace");
        let path =
            root.path().join("sessions").join(state_file_name("provider", &workspace, "session-1"));
        let persisted = std::fs::read_to_string(path).expect("reads state");
        for forbidden in
            ["PROMPT_DO_NOT_PERSIST", "MODEL_DO_NOT_PERSIST", jwt, "sk-live-1234567890"]
        {
            assert!(!persisted.contains(forbidden), "persisted state leaked {forbidden}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn durable_state_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = SessionStateStore::new(Some(root.path().to_path_buf()), 1024);
        store.save("provider", workspace.path(), "session-1", &"state").expect("save");
        let workspace = canonical_workspace(workspace.path()).expect("canonical workspace");
        let directory = root.path().join("sessions");
        let path = directory.join(state_file_name("provider", &workspace, "session-1"));
        assert_eq!(
            std::fs::metadata(directory).expect("directory metadata").permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path).expect("file metadata").permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupted_state_fails_closed() {
        let root = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = SessionStateStore::new(Some(root.path().to_path_buf()), 1024);
        store.save("provider", workspace.path(), "session-1", &"state").expect("save");
        let workspace_name = canonical_workspace(workspace.path()).expect("canonical workspace");
        let path = root.path().join("sessions").join(state_file_name(
            "provider",
            &workspace_name,
            "session-1",
        ));
        std::fs::write(path, b"{}").expect("corrupt");

        let error = store
            .load::<String>("provider", workspace.path(), "session-1")
            .expect_err("corrupt state rejected");
        assert!(error.to_string().contains("failed to deserialize"));
    }
}
