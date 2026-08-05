//! Server-side session state.
//!
//! The framework owns session state — providers never touch transport or
//! session bookkeeping.  [`SessionStore`] keeps live sessions in a
//! `RwLock<BTreeMap>` so `session/list` returns stable, deterministic
//! order.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::RwLock;

use ee_agent_protocol::{McpServer, SessionId};

/// One live session, owned by the framework.
///
/// Non-exhaustive so future session fields (modes, config options) can be
/// added without breaking readers; constructed only by the framework.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerSession {
    /// Unique session id, as resolved by the provider.
    pub session_id: SessionId,
    /// Absolute working directory, when the session has one.
    pub cwd: Option<std::path::PathBuf>,
    /// Absolute additional workspace roots.
    pub additional_directories: Vec<std::path::PathBuf>,
    /// MCP servers advertised for this session.
    pub mcp_servers: Vec<McpServer>,
    /// Human-readable session title (surfaced in `session/list`).
    pub title: Option<String>,
    /// Provider-owned metadata; the framework treats it as opaque.
    pub metadata: serde_json::Value,
}

/// Error returned by [`SessionStore::insert_new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStoreError {
    /// A session with the same id is already registered.
    DuplicateSession(SessionId),
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSession(session_id) => {
                write!(f, "session already exists: {session_id}")
            }
        }
    }
}

impl std::error::Error for SessionStoreError {}

/// Thread-safe registry of live sessions.
///
/// Sessions are keyed by their id string and iterate in stable (sorted)
/// order.  `SessionId` itself does not implement `Ord` in the SDK, so the
/// store keys on its display form.
#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: RwLock<BTreeMap<String, ServerSession>>,
}

impl SessionStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new session; rejects duplicate ids.
    pub fn insert_new(&self, session: ServerSession) -> Result<(), SessionStoreError> {
        let key = session.session_id.to_string();
        let mut sessions = self.sessions.write().expect("session store poisoned");
        if sessions.contains_key(&key) {
            return Err(SessionStoreError::DuplicateSession(session.session_id));
        }
        sessions.insert(key, session);
        Ok(())
    }

    /// Returns a clone of the session with the given id, if present.
    #[must_use]
    pub fn get(&self, session_id: &SessionId) -> Option<ServerSession> {
        self.sessions.read().expect("session store poisoned").get(&session_id.to_string()).cloned()
    }

    /// Returns all live sessions in stable (sorted id) order.
    #[must_use]
    pub fn list(&self) -> Vec<ServerSession> {
        self.sessions.read().expect("session store poisoned").values().cloned().collect()
    }

    /// Removes and returns the session with the given id, if present.
    pub fn remove(&self, session_id: &SessionId) -> Option<ServerSession> {
        self.sessions.write().expect("session store poisoned").remove(&session_id.to_string())
    }

    /// Whether a session with the given id is registered.
    #[must_use]
    pub fn contains(&self, session_id: &SessionId) -> bool {
        self.sessions.read().expect("session store poisoned").contains_key(&session_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn session(id: &str) -> ServerSession {
        ServerSession {
            session_id: SessionId::new(id),
            cwd: Some(PathBuf::from("/work")),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            title: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn insert_get_contains_remove() {
        let store = SessionStore::new();
        assert!(!store.contains(&SessionId::new("s-1")));
        assert!(store.get(&SessionId::new("s-1")).is_none());
        assert!(store.remove(&SessionId::new("s-1")).is_none());

        store.insert_new(session("s-1")).expect("inserts");
        assert!(store.contains(&SessionId::new("s-1")));
        assert_eq!(
            store.get(&SessionId::new("s-1")).expect("gets").session_id,
            SessionId::new("s-1")
        );

        let removed = store.remove(&SessionId::new("s-1")).expect("removes");
        assert_eq!(removed.session_id, SessionId::new("s-1"));
        assert!(!store.contains(&SessionId::new("s-1")));
    }

    #[test]
    fn duplicate_session_id_is_rejected() {
        let store = SessionStore::new();
        store.insert_new(session("s-1")).expect("first insert");
        assert_eq!(
            store.insert_new(session("s-1")),
            Err(SessionStoreError::DuplicateSession(SessionId::new("s-1")))
        );
    }

    #[test]
    fn list_returns_stable_sorted_order() {
        let store = SessionStore::new();
        store.insert_new(session("zeta")).expect("inserts zeta");
        store.insert_new(session("alpha")).expect("inserts alpha");
        store.insert_new(session("mid")).expect("inserts mid");

        let ids: Vec<String> = store.list().into_iter().map(|s| s.session_id.to_string()).collect();
        assert_eq!(ids, vec!["alpha", "mid", "zeta"]);
    }
}
