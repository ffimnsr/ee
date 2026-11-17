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

/// Maximum accepted client-name length, in bytes.
const MAX_CLIENT_NAME_BYTES: usize = 64;

/// Deterministic, non-identifying project id for one workspace root.
///
/// Providers only need a stable grouping key for prompt caches and per-session
/// rate limits, so the path is hashed instead of sent: a cache or rate-limit
/// header must never carry a filesystem path.
#[must_use]
pub(crate) fn project_id_for_path(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut id = String::with_capacity(21);
    id.push_str("proj_");
    for byte in &digest[..8] {
        // Writing to a String cannot fail.
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Reduces an ACP client name to a header-safe token.
///
/// The header layer rejects values containing control characters, whitespace,
/// or separators, so an unusual client name must not be able to fail a turn.
/// Non-token characters are dropped; `None` means the caller should omit the
/// field rather than send an empty or unsafe value.
#[must_use]
pub(crate) fn sanitize_client_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-'))
        .take(MAX_CLIENT_NAME_BYTES)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
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

    #[test]
    fn project_ids_are_stable_per_path_and_never_carry_the_path() {
        let root = std::path::Path::new("/home/someone/work/project");
        let other = std::path::Path::new("/home/someone/work/other");

        let first = project_id_for_path(root);
        assert_eq!(first, project_id_for_path(root));
        assert_ne!(first, project_id_for_path(other));
        assert!(first.starts_with("proj_"), "{first}");
        assert_eq!(first.len(), 21, "proj_ plus eight bytes of hex");
        assert!(!first.contains("someone"), "a path must never reach a header");
        assert!(!first.contains("project"), "a path must never reach a header");
    }

    #[test]
    fn client_names_are_reduced_to_header_safe_tokens() {
        assert_eq!(sanitize_client_name("Zed").as_deref(), Some("Zed"));
        assert_eq!(sanitize_client_name("ee_cli-1.2").as_deref(), Some("ee_cli-1.2"));
        assert_eq!(sanitize_client_name("  ").as_deref(), None);
        assert_eq!(sanitize_client_name("/home/someone").as_deref(), Some("homesomeone"));
        let long = sanitize_client_name(&"a".repeat(200)).expect("token");
        assert_eq!(long.len(), MAX_CLIENT_NAME_BYTES);
    }
}
