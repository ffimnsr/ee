//! In-memory session-scoped approval state (Phase 1 foundation).
//!
//! The shared precedence contract keeps session decisions here, in memory
//! only: session deny precedes every session allow and every persistent
//! rule.  `allow_once` / `deny_once` are resolved by the approval UI layer
//! and never reach this store; session rows are cleared on session teardown.

use std::collections::BTreeSet;

/// One session-scoped decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionChoice {
    Allow,
    Deny,
}

/// `(session_id, fingerprint)` entries auto-resolved for the rest of the
/// session.  Keys use the operation fingerprint computed by the session
/// layer (path for writes, command+args for terminals).
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionPolicy {
    allowed: BTreeSet<(String, String)>,
    denied: BTreeSet<(String, String)>,
}

impl SessionPolicy {
    /// Auto-decision for `(session_id, fingerprint)`, if recorded; a deny
    /// always wins over an allow for the same key.
    pub(crate) fn lookup(&self, session_id: &str, fingerprint: &str) -> Option<SessionChoice> {
        if self.is_denied(session_id, fingerprint) {
            return Some(SessionChoice::Deny);
        }
        if self.is_allowed(session_id, fingerprint) {
            return Some(SessionChoice::Allow);
        }
        None
    }

    /// Records a session-scoped decision.
    pub(crate) fn record(&mut self, session_id: &str, fingerprint: &str, choice: SessionChoice) {
        let key = (session_id.to_string(), fingerprint.to_string());
        match choice {
            SessionChoice::Allow => {
                self.allowed.insert(key);
            }
            SessionChoice::Deny => {
                self.denied.insert(key);
            }
        }
    }

    /// Drops every recorded decision for `session_id` (session close /
    /// connection loss).
    pub(crate) fn invalidate_session(&mut self, session_id: &str) {
        self.allowed.retain(|(session, _)| session != session_id);
        self.denied.retain(|(session, _)| session != session_id);
    }

    /// Injected evaluator input: whether a session deny covers the
    /// operation fingerprint.
    pub(crate) fn is_denied(&self, session_id: &str, fingerprint: &str) -> bool {
        self.denied.contains(&(session_id.to_string(), fingerprint.to_string()))
    }

    /// Injected evaluator input: whether a session allow covers the
    /// operation fingerprint.
    pub(crate) fn is_allowed(&self, session_id: &str, fingerprint: &str) -> bool {
        self.allowed.contains(&(session_id.to_string(), fingerprint.to_string()))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.denied.is_empty()
    }
}
