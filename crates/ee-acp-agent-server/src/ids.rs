//! Monotonic ID generation for requests and sessions.
//!
//! Each generator owns its counter as plain instance state — no global
//! mutable state — so IDs are unique within one generator instance.  Keep
//! one generator per server instance and hand it to the dispatch path.

use ee_agent_protocol::{RequestId, SessionId};

/// Generates monotonically increasing JSON-RPC request ids.
///
/// Ids start at `1` (matching the SDK's own convention) and increase by one
/// per call.  The counter lives in the instance, so concurrent servers each
/// have their own id space and never need shared mutable state.
#[derive(Debug, Clone)]
pub struct RequestIdGenerator {
    next: i64,
}

impl RequestIdGenerator {
    /// Creates a generator whose first id is `1`.
    #[must_use]
    pub fn new() -> Self {
        Self { next: 1 }
    }

    /// Returns the next request id.
    #[must_use]
    pub fn next_id(&mut self) -> RequestId {
        let id = self.next;
        self.next += 1;
        RequestId::Number(id)
    }
}

impl Default for RequestIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Generates monotonically increasing session ids with a configured prefix.
///
/// Ids take the form `{prefix}-{counter}` (for example `session-1`,
/// `session-2`) and are unique within one generator instance, without any
/// global mutable state.
#[derive(Debug, Clone)]
pub struct SessionIdGenerator {
    prefix: String,
    next: u64,
}

impl SessionIdGenerator {
    /// Creates a generator for the given id prefix.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into(), next: 1 }
    }

    /// Returns the next session id.
    #[must_use]
    pub fn next_id(&mut self) -> SessionId {
        let id = self.next;
        self.next += 1;
        SessionId::new(format!("{}-{id}", self.prefix))
    }

    /// The configured prefix.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_monotonic_and_unique() {
        let mut generator = RequestIdGenerator::new();
        let mut previous: Option<i64> = None;
        for _ in 0..1_000 {
            let RequestId::Number(id) = generator.next_id() else {
                panic!("expected numeric request id");
            };
            if let Some(previous) = previous {
                assert!(id > previous, "ids must strictly increase");
            }
            previous = Some(id);
        }
    }

    #[test]
    fn request_ids_start_at_one() {
        let mut generator = RequestIdGenerator::new();
        assert_eq!(generator.next_id(), RequestId::Number(1));
        assert_eq!(generator.next_id(), RequestId::Number(2));
    }

    #[test]
    fn independent_generators_do_not_share_state() {
        let mut a = RequestIdGenerator::new();
        let mut b = RequestIdGenerator::new();
        assert_eq!(a.next_id(), RequestId::Number(1));
        assert_eq!(b.next_id(), RequestId::Number(1));
    }

    #[test]
    fn session_ids_use_configured_prefix() {
        let mut generator = SessionIdGenerator::new("session");
        assert_eq!(generator.prefix(), "session");
        assert_eq!(generator.next_id(), SessionId::new("session-1"));
        assert_eq!(generator.next_id(), SessionId::new("session-2"));
    }

    #[test]
    fn session_ids_use_custom_prefix() {
        let mut generator = SessionIdGenerator::new("provider-x");
        assert_eq!(generator.next_id(), SessionId::new("provider-x-1"));
    }
}
