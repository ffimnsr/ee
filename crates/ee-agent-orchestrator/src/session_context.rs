//! Session affinity carried on every model request.
//!
//! Providers key prompt caches and per-session rate limits on an affinity id,
//! not on the turn's own session id. The reference agent groups a whole agent
//! tree under one affinity: a child uses its parent's id and a fork uses the
//! fork source's id, so nested agents share one provider cache lineage.
//!
//! [`SessionContext`] is the normalized carrier. Provider crates lower it to
//! their own header set; the orchestrator never sees a provider name.

/// Provider-neutral session affinity for one model request.
///
/// All fields are opaque identifiers. Nothing here is a filesystem path, a
/// username, or credential material: providers validate the values before they
/// become HTTP headers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct SessionContext {
    /// The session issuing this request.
    pub session_id: String,
    /// Parent session id, when this session is a child agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Project/workspace grouping id, when the host exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Client name reported to the provider (for example `ee-cli`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl SessionContext {
    /// Builds a context for a root session.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), ..Self::default() }
    }

    /// Marks this session as a child of `parent_session_id`.
    #[must_use]
    pub fn with_parent(mut self, parent_session_id: impl Into<String>) -> Self {
        self.parent_session_id = Some(parent_session_id.into());
        self
    }

    /// Sets the project/workspace grouping id.
    #[must_use]
    pub fn with_project(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    /// Sets the client name sent to the provider.
    #[must_use]
    pub fn with_client(mut self, client_name: impl Into<String>) -> Self {
        self.client_name = Some(client_name.into());
        self
    }

    /// Affinity id grouping provider caches and rate limits.
    ///
    /// A child session inherits its parent's affinity so the whole agent tree
    /// shares one cache lineage. Fork sources are not modeled separately: a
    /// forked session is created with the source as its parent.
    #[must_use]
    pub fn affinity(&self) -> &str {
        self.parent_session_id.as_deref().unwrap_or(&self.session_id)
    }

    /// Prompt-cache key derived from the affinity id.
    ///
    /// The reference strips a `ses_` prefix from a 64-hex session id and sends
    /// the remaining digest. Every other shape is sent verbatim.
    #[must_use]
    pub fn cache_key(&self) -> String {
        let affinity = self.affinity();
        match affinity.strip_prefix("ses_") {
            Some(digest)
                if digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
            {
                digest.to_string()
            }
            _ => affinity.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_inherits_parent_affinity_but_keeps_its_own_id() {
        let context = SessionContext::new("ses_child").with_parent("ses_parent");
        assert_eq!(context.session_id, "ses_child");
        assert_eq!(context.affinity(), "ses_parent");
    }

    #[test]
    fn root_session_is_its_own_affinity() {
        let context = SessionContext::new("ses_root");
        assert_eq!(context.affinity(), "ses_root");
    }

    #[test]
    fn ses_prefixed_digest_is_stripped_for_the_cache_key() {
        let digest = "a".repeat(64);
        let context = SessionContext::new(format!("ses_{digest}"));
        assert_eq!(context.cache_key(), digest);
    }

    #[test]
    fn non_digest_ids_are_sent_verbatim() {
        let context = SessionContext::new("s-1");
        assert_eq!(context.cache_key(), "s-1");
        let short = SessionContext::new("ses_short");
        assert_eq!(short.cache_key(), "ses_short");
    }
}
