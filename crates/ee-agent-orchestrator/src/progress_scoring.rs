//! Task readiness scoring over the task graph.
//!
//! Readiness helpers turn graph state into execution decisions: a task is
//! ready only when every dependency completed, a task is blocked when any
//! dependency failed or was cancelled, and [`mark_blocked_by_failed_dependencies`]
//! persists that state.  [`TaskProgress`] computes a deterministic progress
//! percentage and status counts for milestone summaries and UI projections.

use serde::{Deserialize, Serialize};

use crate::tasks::{TaskGraph, TaskNode, TaskStatus};

/// Whether `task` may run: it is pending and every dependency completed.
#[must_use]
pub fn is_ready(task: &TaskNode, tasks: &TaskGraph) -> bool {
    task.status == TaskStatus::Pending
        && task.dependencies.iter().all(|dependency| {
            tasks.get(dependency).is_some_and(|node| node.status == TaskStatus::Completed)
        })
}

/// Whether `task` is blocked: a dependency failed or was cancelled.
#[must_use]
pub fn is_blocked(task: &TaskNode, tasks: &TaskGraph) -> bool {
    task.dependencies.iter().any(|dependency| {
        tasks
            .get(dependency)
            .is_some_and(|node| matches!(node.status, TaskStatus::Failed | TaskStatus::Cancelled))
    })
}

/// Ready (pending, all dependencies completed) tasks in stable id order.
#[must_use]
pub fn ready_tasks(tasks: &TaskGraph) -> Vec<TaskNode> {
    tasks.list().into_iter().filter(|task| is_ready(task, tasks)).collect()
}

/// Pending or blocked tasks whose dependencies failed or were cancelled, in
/// stable id order.
#[must_use]
pub fn blocked_tasks(tasks: &TaskGraph) -> Vec<TaskNode> {
    tasks
        .list()
        .into_iter()
        .filter(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked))
        .filter(|task| is_blocked(task, tasks))
        .collect()
}

/// Marks every pending task whose dependency failed or was cancelled as
/// blocked, returning the number of tasks transitioned.  Tasks already
/// blocked are left untouched.
pub fn mark_blocked_by_failed_dependencies(tasks: &mut TaskGraph) -> usize {
    let targets: Vec<_> = tasks
        .list()
        .into_iter()
        .filter(|task| task.status == TaskStatus::Pending && is_blocked(task, tasks))
        .map(|task| task.id)
        .collect();
    let mut marked = 0usize;
    for id in targets {
        if tasks.transition(&id, TaskStatus::Blocked).is_ok() {
            marked += 1;
        }
    }
    marked
}

/// Deterministic status counts and progress percentage of a task graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskProgress {
    /// Total tracked tasks (including the root).
    pub total: usize,
    /// Completed tasks.
    pub completed: usize,
    /// Failed tasks.
    pub failed: usize,
    /// Blocked tasks.
    pub blocked: usize,
    /// Pending tasks.
    pub pending: usize,
    /// Running tasks.
    pub running: usize,
    /// Cancelled tasks.
    pub cancelled: usize,
}

impl TaskProgress {
    /// Counts graph statuses in one pass.
    #[must_use]
    pub fn from_graph(tasks: &TaskGraph) -> Self {
        let mut progress = Self::default();
        for task in tasks.list() {
            progress.total += 1;
            match task.status {
                TaskStatus::Pending => progress.pending += 1,
                TaskStatus::Running => progress.running += 1,
                TaskStatus::Blocked => progress.blocked += 1,
                TaskStatus::Completed => progress.completed += 1,
                TaskStatus::Failed => progress.failed += 1,
                TaskStatus::Cancelled => progress.cancelled += 1,
            }
        }
        progress
    }

    /// Completion percentage (0–100); zero when nothing is tracked.
    #[must_use]
    pub fn percentage(&self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        ((self.completed * 100) / self.total) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with(children: usize) -> (TaskGraph, Vec<TaskNode>) {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("parent", "parent task");
        let mut tasks = Vec::new();
        for index in 0..children {
            tasks.push(
                graph
                    .create_child(&root.id, &format!("child {index}"), "child task")
                    .expect("child"),
            );
        }
        (graph, tasks)
    }

    #[test]
    fn task_is_ready_only_when_dependencies_complete() {
        let (mut graph, tasks) = graph_with(2);
        graph.add_dependency(&tasks[1].id, &tasks[0].id).expect("dependency");
        // Dependency pending: only the dependency-free task is ready.
        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, tasks[0].id);
        assert!(!is_ready(graph.get(&tasks[1].id).expect("task"), &graph));
        assert!(is_ready(graph.get(&tasks[0].id).expect("task"), &graph));
        // Dependency completed: the dependent becomes ready.
        graph.transition(&tasks[0].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[0].id, TaskStatus::Completed).expect("completed");
        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, tasks[1].id);
        assert!(is_ready(graph.get(&tasks[1].id).expect("task"), &graph));
    }

    #[test]
    fn failed_dependency_blocks_the_dependent() {
        let (mut graph, tasks) = graph_with(2);
        graph.add_dependency(&tasks[1].id, &tasks[0].id).expect("dependency");
        graph.transition(&tasks[0].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[0].id, TaskStatus::Failed).expect("failed");
        assert!(is_blocked(graph.get(&tasks[1].id).expect("task"), &graph));
        let blocked = blocked_tasks(&graph);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].id, tasks[1].id);
        assert!(ready_tasks(&graph).is_empty(), "blocked tasks are never ready");
    }

    #[test]
    fn cancelled_dependency_blocks_the_dependent() {
        let (mut graph, tasks) = graph_with(2);
        graph.add_dependency(&tasks[1].id, &tasks[0].id).expect("dependency");
        graph.transition(&tasks[0].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[0].id, TaskStatus::Cancelled).expect("cancelled");
        assert!(is_blocked(graph.get(&tasks[1].id).expect("task"), &graph));
    }

    #[test]
    fn mark_blocked_persists_blocked_state_and_counts() {
        let (mut graph, tasks) = graph_with(3);
        graph.add_dependency(&tasks[1].id, &tasks[0].id).expect("dependency");
        graph.add_dependency(&tasks[2].id, &tasks[0].id).expect("dependency");
        graph.transition(&tasks[0].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[0].id, TaskStatus::Failed).expect("failed");
        let marked = mark_blocked_by_failed_dependencies(&mut graph);
        assert_eq!(marked, 2);
        let graph = &graph;
        assert_eq!(graph.get(&tasks[1].id).expect("task").status, TaskStatus::Blocked);
        assert_eq!(graph.get(&tasks[2].id).expect("task").status, TaskStatus::Blocked);
        // Idempotent: already blocked tasks are not re-marked.
        let mut graph = graph.clone();
        assert_eq!(mark_blocked_by_failed_dependencies(&mut graph), 0);
    }

    #[test]
    fn progress_percentage_tracks_completion() {
        let (mut graph, tasks) = graph_with(4);
        let progress = TaskProgress::from_graph(&graph);
        assert_eq!(progress.total, 5, "root plus four children");
        assert_eq!(progress.percentage(), 0);

        for task in tasks.iter().take(2) {
            graph.transition(&task.id, TaskStatus::Running).expect("running");
            graph.transition(&task.id, TaskStatus::Completed).expect("completed");
        }
        let progress = TaskProgress::from_graph(&graph);
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.percentage(), 40, "2 of 5 tasks complete");

        for task in tasks.iter().skip(2) {
            graph.transition(&task.id, TaskStatus::Running).expect("running");
            graph.transition(&task.id, TaskStatus::Completed).expect("completed");
        }
        let root = graph.list().into_iter().find(|task| task.parent.is_none()).expect("root");
        graph.transition(&root.id, TaskStatus::Completed).expect("root completes");
        let progress = TaskProgress::from_graph(&graph);
        assert_eq!(progress.percentage(), 100);
    }

    #[test]
    fn progress_counts_failed_blocked_and_cancelled() {
        let (mut graph, tasks) = graph_with(3);
        graph.transition(&tasks[0].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[0].id, TaskStatus::Failed).expect("failed");
        graph.transition(&tasks[1].id, TaskStatus::Blocked).expect("blocked");
        graph.transition(&tasks[2].id, TaskStatus::Running).expect("running");
        graph.transition(&tasks[2].id, TaskStatus::Cancelled).expect("cancelled");
        let progress = TaskProgress::from_graph(&graph);
        assert_eq!(progress.failed, 1);
        assert_eq!(progress.blocked, 1);
        assert_eq!(progress.cancelled, 1);
        assert_eq!(progress.running, 1, "the root stays running");
        assert_eq!(progress.completed, 0);
    }

    #[test]
    fn empty_graph_scores_zero() {
        let graph = TaskGraph::new();
        let progress = TaskProgress::from_graph(&graph);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.percentage(), 0);
    }

    #[test]
    fn task_progress_roundtrips_through_json() {
        let (graph, _) = graph_with(2);
        let progress = TaskProgress::from_graph(&graph);
        let json = serde_json::to_string(&progress).expect("serializes");
        let restored: TaskProgress = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, progress);
    }
}
