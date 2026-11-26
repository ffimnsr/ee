//! `ee do agent sessions` command implementations.

use std::collections::BTreeSet;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent_session_store::{WorkspaceSessionArchive, prune_sessions};
use crate::args::AgentSessionsPruneArgs;

/// Prunes archived transcripts and provider session logs under the user data
/// directory. Live session ids from the client registry are always protected.
pub(crate) fn prune(args: &AgentSessionsPruneArgs) -> io::Result<()> {
    let root = WorkspaceSessionArchive::default_root().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot resolve the user data directory")
    })?;
    let state_dir = crate::logs::state_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot resolve the user state directory")
    })?;
    prune_at(&root, &state_dir, args)
}

/// Testable prune entry point with explicit data and state roots.
pub(crate) fn prune_at(
    root: &std::path::Path,
    state_dir: &std::path::Path,
    args: &AgentSessionsPruneArgs,
) -> io::Result<()> {
    let live = live_session_ids_at(state_dir)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs());
    let cutoff = now.saturating_sub(args.older_than_days.saturating_mul(24 * 60 * 60));
    let report = prune_sessions(root, cutoff, args.all, &live, args.dry_run)
        .map_err(|error| io::Error::other(format!("agent session prune failed: {error}")))?;
    let prefix = if args.dry_run { "would prune" } else { "pruned" };
    println!(
        "{prefix} {} transcript session(s) and {} provider session log(s), {} byte(s); {} live session(s) protected",
        report.transcripts, report.session_logs, report.bytes, report.skipped_live
    );
    if report.transcripts == 0 && report.session_logs == 0 {
        println!("agent session prune: nothing to remove");
    }
    Ok(())
}

/// Reads the client registry and returns every live session id across all
/// workspaces. A malformed registry fails closed instead of risking deletion.
fn live_session_ids_at(state_dir: &std::path::Path) -> io::Result<BTreeSet<String>> {
    let path = state_dir.join("agent-sessions.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => {
            return Err(io::Error::other(format!(
                "failed to read the agent session registry {}: {error}",
                path.display()
            )));
        }
    };
    let documents: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        io::Error::other(format!("agent session registry {} is malformed: {error}", path.display()))
    })?;
    let mut live = BTreeSet::new();
    let Some(documents) = documents.as_object() else {
        return Err(io::Error::other("agent session registry is not an object"));
    };
    for document in documents.values() {
        let Some(sessions) = document.get("sessions").and_then(|value| value.as_array()) else {
            continue;
        };
        for session in sessions {
            if let Some(session_id) = session.get("session_id").and_then(|value| value.as_str()) {
                live.insert(session_id.to_string());
            }
        }
    }
    Ok(live)
}
