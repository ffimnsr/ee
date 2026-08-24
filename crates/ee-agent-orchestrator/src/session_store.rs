//! Durable normal-session state storage.
//!
//! Recovery checkpoints represent interrupted turns. This store separately
//! preserves completed ACP sessions so `session/load` still works after the
//! provider process exits. Entries are scoped by provider identity, canonical
//! workspace path, and session id; files use hashed names to avoid exposing
//! workspace paths in directory entries.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;

/// Current on-disk session-state format.
const SESSION_STATE_SCHEMA_VERSION: u32 = 1;

/// Atomic, bounded storage for durable ACP session snapshots.
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
        let payload = session_payload(provider, &workspace, session_id, state)?;
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
        let directory = root.join("sessions");
        std::fs::create_dir_all(&directory).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to create session state directory {}: {error}",
                directory.display()
            ))
        })?;
        let final_path = directory.join(state_file_name(provider, &stored.workspace, session_id));
        let tmp_path = directory.join(format!(".tmp-{}-{}.json", std::process::id(), nonce()));
        std::fs::write(&tmp_path, bytes).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to write session state {}: {error}",
                tmp_path.display()
            ))
        })?;
        std::fs::rename(&tmp_path, &final_path).map_err(|error| {
            let _ = std::fs::remove_file(&tmp_path);
            OrchestratorError::Serialization(format!(
                "failed to finalize session state {}: {error}",
                final_path.display()
            ))
        })
    }

    /// Loads a snapshot only when its provider, canonical workspace, session
    /// id, schema, and checksum match exactly.
    pub fn load<T: DeserializeOwned + Serialize>(
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
        let stored: StoredSession<T> = serde_json::from_slice(&bytes).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "session state {} failed to deserialize: {error}",
                path.display()
            ))
        })?;
        validate_stored(&stored, provider, &workspace, session_id)?;
        Ok(Some(stored.state))
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
            let Ok(stored) = serde_json::from_slice::<StoredSession<serde_json::Value>>(&bytes)
            else {
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
struct StoredSession<T> {
    schema_version: u32,
    provider: String,
    workspace: String,
    session_id: String,
    checksum: String,
    state: T,
}

fn validate_stored<T: Serialize>(
    stored: &StoredSession<T>,
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
