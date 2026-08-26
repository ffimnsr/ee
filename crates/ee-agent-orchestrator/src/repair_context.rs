//! Fresh, bounded host-context snapshots for repair attempts.
//!
//! This module turns results from existing read-only editor MCP tools into a
//! [`ContextPlanningInput`]. It never discovers files, runs commands, or
//! invents revisions. A repair snapshot requires current untruncated buffer,
//! diagnostics, changed-file, diff, and review observations from the host.

use serde_json::Value;

use crate::context_planner::{
    ContextCandidate, ContextFreshness, ContextPlanIdentity, ContextPlanningInput, ContextSource,
    ContextTrustClass,
};
use crate::repair::RepairStopReason;
use crate::tools::{ToolErrorKind, ToolResult};

/// Read-only editor tools required for a repair snapshot, in stable order.
pub const REPAIR_CONTEXT_TOOLS: &[&str] = &[
    "ee_project_instructions",
    "ee_open_buffers",
    "ee_get_diagnostics",
    "ee_changed_files",
    "ee_git_diff",
    "ee_review_context",
];

/// One observed read-only host tool result.
#[derive(Debug, Clone)]
pub struct RepairContextObservation {
    /// Registered model-facing tool name.
    pub tool_name: String,
    /// Normalized tool result.
    pub result: ToolResult,
}

/// Current host facts safe to use for one bounded repair attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairContextSnapshot {
    /// Fresh planning input. All repository-derived candidates remain untrusted.
    pub input: ContextPlanningInput,
    /// Opaque host buffer revision identity for repair freshness checks.
    pub revision: String,
    /// Bounded non-content diff identity. It intentionally does not hash paths or diff text.
    pub diff_fingerprint: String,
    /// Number of current error diagnostics.
    pub error_diagnostic_count: usize,
    /// Number of changed files observed by the host.
    pub changed_file_count: usize,
    /// Whether SCM reports an unresolved conflict in the current changed-file inventory.
    pub has_conflicts: bool,
}

/// Builds one fresh bounded snapshot from required host read-tool results.
///
/// Results are required because a partial snapshot could combine stale editor
/// state with a current validation failure. Tool output stays repository
/// content; `ee_project_instructions` is deliberately not promoted to system
/// policy because repository instruction files are untrusted data.
pub fn build_repair_context(
    session_id: &str,
    observations: &[RepairContextObservation],
) -> Result<RepairContextSnapshot, RepairStopReason> {
    let project = required_value(observations, "ee_project_instructions")?;
    let buffers = required_value(observations, "ee_open_buffers")?;
    let diagnostics = required_value(observations, "ee_get_diagnostics")?;
    let changed_files = required_value(observations, "ee_changed_files")?;
    let diff = required_value(observations, "ee_git_diff")?;
    let review = required_value(observations, "ee_review_context")?;

    if is_truncated(&diagnostics)
        || is_truncated(&changed_files)
        || is_truncated(&diff)
        || nested_truncated(&review, "diagnostics")
        || nested_truncated(&review, "changedFiles")
    {
        return Err(RepairStopReason::UnavailableEnvironment);
    }

    let revision = buffer_revision(&buffers).ok_or(RepairStopReason::StaleState)?;
    let error_diagnostic_count = error_diagnostic_count(&diagnostics);
    let changed_file_count = changed_file_count(&changed_files);
    let has_conflicts = changed_files.get("files").and_then(Value::as_array).is_some_and(|files| {
        files.iter().any(|file| file.get("conflicted").and_then(Value::as_bool) == Some(true))
    });
    let diff_fingerprint = format!(
        "revision:{revision};bytes:{};truncated:{}",
        diff.get("bytesReturned").and_then(Value::as_u64).unwrap_or_default(),
        diff.get("truncated").and_then(Value::as_bool).unwrap_or(false),
    );

    let freshness = ContextFreshness::fresh(revision.clone());
    let candidates = vec![
        candidate("project_instructions", ContextSource::ProjectInstructions, &freshness, project),
        candidate("open_buffers", ContextSource::DirtyBuffer, &freshness, buffers),
        candidate("diagnostics", ContextSource::Diagnostics, &freshness, diagnostics),
        candidate("changed_files", ContextSource::GitDiff, &freshness, changed_files),
        candidate("git_diff", ContextSource::GitDiff, &freshness, diff),
        candidate("review_context", ContextSource::SymbolNeighborhood, &freshness, review),
    ];
    Ok(RepairContextSnapshot {
        input: ContextPlanningInput {
            identity: ContextPlanIdentity {
                session_id: session_id.to_string(),
                policy_revision: "host-policy-current".to_string(),
                workspace_revision: revision.clone(),
                buffer_revision: revision.clone(),
                diagnostics_revision: revision.clone(),
                graph_revision: "host-review-current".to_string(),
                checkout_revision: "host-worktree-current".to_string(),
            },
            candidates,
        },
        revision,
        diff_fingerprint,
        error_diagnostic_count,
        changed_file_count,
        has_conflicts,
    })
}

fn required_value(
    observations: &[RepairContextObservation],
    name: &str,
) -> Result<Value, RepairStopReason> {
    let observation = observations
        .iter()
        .find(|observation| observation.tool_name == name)
        .ok_or(RepairStopReason::UnavailableEnvironment)?;
    if !observation.result.success {
        return Err(match observation.result.error_kind {
            Some(ToolErrorKind::PermissionDenied) => RepairStopReason::PolicyDenial,
            Some(ToolErrorKind::Cancelled) => RepairStopReason::Cancellation,
            Some(ToolErrorKind::Timeout) => RepairStopReason::Timeout,
            Some(ToolErrorKind::InvalidArguments) | Some(ToolErrorKind::Backend) | None => {
                RepairStopReason::UnavailableEnvironment
            }
        });
    }
    observation
        .result
        .structured_output
        .clone()
        .or_else(|| serde_json::from_str(&observation.result.text_output).ok())
        .ok_or(RepairStopReason::UnavailableEnvironment)
}

fn candidate(
    id: &str,
    source: ContextSource,
    freshness: &ContextFreshness,
    value: Value,
) -> ContextCandidate {
    ContextCandidate::new(
        id,
        source,
        ContextTrustClass::RepositoryContent,
        freshness.clone(),
        value.to_string(),
    )
}

fn buffer_revision(buffers: &Value) -> Option<String> {
    let mut revisions = buffers
        .get("buffers")?
        .as_array()?
        .iter()
        .filter_map(|buffer| buffer.get("revisionId").and_then(Value::as_str))
        .filter(|revision| !revision.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    revisions.sort();
    revisions.dedup();
    (!revisions.is_empty()).then(|| format!("buffers:{}", revisions.join(",")))
}

fn error_diagnostic_count(diagnostics: &Value) -> usize {
    diagnostics
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| {
                    entry
                        .get("severity")
                        .and_then(Value::as_str)
                        .is_some_and(|severity| severity.eq_ignore_ascii_case("error"))
                })
                .count()
        })
        .unwrap_or_default()
}

fn changed_file_count(changed_files: &Value) -> usize {
    changed_files.get("files").and_then(Value::as_array).map_or(0, Vec::len)
}

fn is_truncated(value: &Value) -> bool {
    value.get("truncated").and_then(Value::as_bool).unwrap_or(false)
}

fn nested_truncated(value: &Value, field: &str) -> bool {
    value.get(field).is_some_and(is_truncated)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn observation(tool_name: &str, value: Value) -> RepairContextObservation {
        RepairContextObservation {
            tool_name: tool_name.to_string(),
            result: ToolResult::success_structured(value.to_string(), value),
        }
    }

    fn observations(revision: &str) -> Vec<RepairContextObservation> {
        vec![
            observation("ee_project_instructions", json!({"sources": []})),
            observation(
                "ee_open_buffers",
                json!({"buffers": [{"path": "/work/src/lib.rs", "revisionId": revision, "dirty": true}]}),
            ),
            observation(
                "ee_get_diagnostics",
                json!({"diagnostics": [{"severity": "error"}], "truncated": false}),
            ),
            observation(
                "ee_changed_files",
                json!({"files": [{"path": "/work/src/lib.rs"}], "truncated": false}),
            ),
            observation(
                "ee_git_diff",
                json!({"diff": "diff --git", "bytesReturned": 10, "truncated": false}),
            ),
            observation(
                "ee_review_context",
                json!({"diagnostics": {"truncated": false}, "changedFiles": {"truncated": false}}),
            ),
        ]
    }

    #[test]
    fn snapshot_uses_host_buffer_revision_and_keeps_repository_content_untrusted() {
        let snapshot =
            build_repair_context("session-1", &observations("buffer-42")).expect("fresh");
        assert_eq!(snapshot.revision, "buffers:buffer-42");
        assert_eq!(snapshot.error_diagnostic_count, 1);
        assert_eq!(snapshot.changed_file_count, 1);
        assert!(
            snapshot
                .input
                .candidates
                .iter()
                .all(|candidate| candidate.trust == ContextTrustClass::RepositoryContent)
        );
    }

    #[test]
    fn missing_buffer_revision_fails_closed_as_stale() {
        let mut values = observations("buffer-42");
        values[1] = observation("ee_open_buffers", json!({"buffers": []}));
        assert_eq!(build_repair_context("session-1", &values), Err(RepairStopReason::StaleState));
    }

    #[test]
    fn denied_and_truncated_snapshots_do_not_start_repair() {
        let mut denied = observations("buffer-42");
        denied[2].result = ToolResult::failure(ToolErrorKind::PermissionDenied, "denied");
        assert_eq!(build_repair_context("session-1", &denied), Err(RepairStopReason::PolicyDenial));

        let mut truncated = observations("buffer-42");
        truncated[3] = observation("ee_changed_files", json!({"files": [], "truncated": true}));
        assert_eq!(
            build_repair_context("session-1", &truncated),
            Err(RepairStopReason::UnavailableEnvironment)
        );
    }
}
