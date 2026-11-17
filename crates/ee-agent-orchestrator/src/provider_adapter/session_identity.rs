//! Provider-facing session affinity for one ACP session.
//!
//! A provider keys prompt caches and per-session rate limits on an affinity id,
//! not on the host's internal identity, so every session records one context
//! here before its first turn. The context carries the session id, an optional
//! parent (a branch or forked child), a hashed project id (never the path), and
//! the ACP client name when it is header-safe.

use std::path::Path;

use ee_agent_protocol::SessionId;

use super::ProviderError;
use crate::runtime::OrchestratorRuntime;
use crate::session_context::{SessionContext, project_id_for_path, sanitize_client_name};

/// Builds the affinity context for one session.
///
/// `parent` is set when the session was forked or branched from another one:
/// the child keeps its own id but inherits the parent's affinity, so a whole
/// agent tree shares one provider cache lineage.
#[must_use]
pub(super) fn for_session(
    session_id: &SessionId,
    workspace: &Path,
    implementation_name: &str,
    parent: Option<&str>,
) -> SessionContext {
    let mut context =
        SessionContext::new(session_id.to_string()).with_project(project_id_for_path(workspace));
    if let Some(parent) = parent {
        context = context.with_parent(parent);
    }
    if let Some(client) = sanitize_client_name(implementation_name) {
        context = context.with_client(client);
    }
    context
}

/// Records session affinity and registers the built-in tools.
///
/// Session construction always needs both, and the affinity must exist before
/// the first turn so the provider can key caches on it.
///
/// # Errors
///
/// Returns a provider error when a built-in tool name is already registered.
pub(super) fn register(
    runtime: &OrchestratorRuntime,
    session_id: &SessionId,
    workspace: &Path,
    implementation_name: &str,
    parent: Option<&str>,
) -> Result<(), ProviderError> {
    runtime.set_session_context(for_session(session_id, workspace, implementation_name, parent));
    runtime.register_builtins(session_id).map_err(|error| {
        ProviderError::BackendFailure(format!("failed to register built-in tools: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_session_records_project_and_client() {
        let context =
            for_session(&SessionId::new("s-1"), Path::new("/home/someone/work"), "Zed", None);

        assert_eq!(context.session_id, "s-1");
        assert!(context.parent_session_id.is_none());
        assert!(context.project_id.as_deref().is_some_and(|id| id.starts_with("proj_")));
        assert_eq!(context.client_name.as_deref(), Some("Zed"));
        assert_eq!(context.affinity(), "s-1");
    }

    #[test]
    fn child_session_inherits_parent_affinity() {
        let context = for_session(
            &SessionId::new("s-child"),
            Path::new("/home/someone/work"),
            "Zed",
            Some("s-parent"),
        );

        assert_eq!(context.session_id, "s-child");
        assert_eq!(context.parent_session_id.as_deref(), Some("s-parent"));
        assert_eq!(context.affinity(), "s-parent");
    }

    #[test]
    fn unsafe_client_names_are_omitted() {
        let context = for_session(&SessionId::new("s-1"), Path::new("/w"), "  ", None);
        assert!(context.client_name.is_none());
    }
}
