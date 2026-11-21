//! Git-worktree isolation tests for write subagents.

use std::path::Path;

use super::super::*;
use super::manager_harness;
use crate::test_support::FakeModel;
use crate::workspace_isolation::{WorkspaceIsolationConfig, git_command};

fn run_git(root: &Path, args: &[&str]) {
    let status = git_command(root, args).status().expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

fn seed_repo(root: &Path) {
    std::fs::create_dir_all(root).expect("repo dir");
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "test@example.invalid"]);
    run_git(root, &["config", "user.name", "ee test"]);
    std::fs::write(root.join("file.txt"), "base\n").expect("seed file");
    run_git(root, &["add", "file.txt"]);
    run_git(root, &["commit", "-q", "-m", "seed"]);
}

fn delegation(root: &std::path::Path, isolation_role: bool) -> DelegationRequest {
    let role = if isolation_role {
        BuiltinSubagentRole::Implementer.role()
    } else {
        BuiltinSubagentRole::Researcher.role()
    };
    DelegationRequest {
        parent_task_id: TaskId::new("task-1"),
        role,
        scoped_prompt: String::from("edit the file"),
        context_snapshot: Vec::new(),
        scope: Some(WorkspaceScope::new(vec![root.to_path_buf()], Vec::new())),
        model_id: None,
        resume_child_id: None,
        fork_context: false,
    }
}

fn completed_result(subagent_id: SubagentId) -> SubagentResult {
    SubagentResult {
        subagent_id: subagent_id.clone(),
        handoff: SubagentHandoff::terminal(
            "implementer",
            subagent_id.as_str(),
            SubagentStatus::Completed,
        ),
        produced_memory_items: Vec::new(),
        tool_call_count: 0,
        error_summary: None,
    }
}

#[test]
fn isolated_implementer_edits_worktree_then_prunes_after_capture() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let worktree_root = temp.path().join("worktrees");
    let config = OrchestratorConfig {
        workspace_isolation: WorkspaceIsolationConfig {
            enabled: true,
            worktree_root: Some(worktree_root.clone()),
            keep_worktrees: false,
        },
        ..OrchestratorConfig::default()
    };
    let harness = manager_harness(config, Arc::new(FakeModel::new(Vec::new())));
    let request = delegation(&repo, true);
    let subagent_id = SubagentId::new("child-1");

    let worktree = harness
        .manager
        .prepare_isolation(&request, &subagent_id)
        .expect("prepare")
        .expect("implementer is isolated");
    std::fs::write(worktree.path().join("file.txt"), "isolated\n").expect("write worktree");

    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).expect("primary file"),
        "base\n",
        "isolated child cannot mutate the primary checkout"
    );

    let result =
        harness.manager.finalize_isolation(worktree.clone(), completed_result(subagent_id));
    assert!(
        result.handoff.recommended_actions.iter().any(|action| action.contains("changed file(s)")),
        "worktree changes become handoff evidence"
    );
    assert!(!worktree.path().exists(), "worktree prunes when not retained");
    let patch = worktree_root.join("child-1.patch");
    assert!(patch.exists(), "bounded patch is written for host apply");
}

#[test]
fn keep_worktrees_retains_completed_child_for_apply() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let worktree_root = temp.path().join("worktrees");
    let config = OrchestratorConfig {
        workspace_isolation: WorkspaceIsolationConfig {
            enabled: true,
            worktree_root: Some(worktree_root),
            keep_worktrees: true,
        },
        ..OrchestratorConfig::default()
    };
    let harness = manager_harness(config, Arc::new(FakeModel::new(Vec::new())));
    let request = delegation(&repo, true);
    let subagent_id = SubagentId::new("child-2");

    let worktree = harness
        .manager
        .prepare_isolation(&request, &subagent_id)
        .expect("prepare")
        .expect("isolated");
    std::fs::write(worktree.path().join("file.txt"), "retained\n").expect("write");
    let result =
        harness.manager.finalize_isolation(worktree.clone(), completed_result(subagent_id));

    assert_eq!(result.handoff.status, SubagentStatus::Completed);
    assert!(worktree.path().exists(), "keep_worktrees retains completed worktrees");
    worktree.prune().expect("cleanup");
}

#[test]
fn read_only_roles_and_disabled_sessions_stay_shared() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);

    let disabled = manager_harness(
        OrchestratorConfig {
            workspace_isolation: WorkspaceIsolationConfig {
                enabled: false,
                worktree_root: Some(temp.path().join("worktrees")),
                keep_worktrees: false,
            },
            ..OrchestratorConfig::default()
        },
        Arc::new(FakeModel::new(Vec::new())),
    );
    assert!(
        disabled
            .manager
            .prepare_isolation(&delegation(&repo, true), &SubagentId::new("child-3"))
            .expect("prepare")
            .is_none(),
        "disabled sessions keep the shared checkout"
    );

    let enabled = manager_harness(
        OrchestratorConfig {
            workspace_isolation: WorkspaceIsolationConfig {
                enabled: true,
                worktree_root: Some(temp.path().join("worktrees")),
                keep_worktrees: false,
            },
            ..OrchestratorConfig::default()
        },
        Arc::new(FakeModel::new(Vec::new())),
    );
    assert!(
        enabled
            .manager
            .prepare_isolation(&delegation(&repo, false), &SubagentId::new("child-4"))
            .expect("prepare")
            .is_none(),
        "read-only roles never take a worktree"
    );
}
