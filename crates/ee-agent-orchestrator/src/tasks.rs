//! Task graph: ids, nodes, status transitions, and plan projection.
//!
//! [`TaskGraph`] tracks plan items with parent/child links and dependency
//! edges, validates status transitions, answers ready-task queries in
//! deterministic (id) order, and projects its state onto ACP v1
//! [`PlanEntry`] values for `UpdateSink::plan_replace`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use ee_agent_protocol::{PlanEntry, PlanEntryPriority, PlanEntryStatus};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;

/// Prefix used by [`TaskGraph`] for generated task ids.
pub const DEFAULT_TASK_ID_PREFIX: &str = "task";

/// Cap on the stored per-task result summary.
pub const MAX_RESULT_SUMMARY_CHARS: usize = 2_000;

/// Stable identifier for one task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    /// Creates a task id from its string form.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The task id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle status of one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TaskStatus {
    /// Created but not started.
    Pending,
    /// Currently being worked.
    Running,
    /// Waiting on a dependency or external condition.
    Blocked,
    /// Finished successfully.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
}

/// Worker assigned to a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TaskWorker {
    /// Worked by the main agent loop.
    Root,
    /// Delegated to a subagent.
    Subagent(TaskId),
}

/// One node in the task graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskNode {
    /// Stable task id.
    pub id: TaskId,
    /// Short human-readable title.
    pub title: String,
    /// Longer description.
    pub description: String,
    /// Parent task, when this is a child (subtask).
    pub parent: Option<TaskId>,
    /// Prerequisites that must complete before this task is ready.
    pub dependencies: Vec<TaskId>,
    /// Current status.
    pub status: TaskStatus,
    /// Assigned worker, when known.
    pub assigned_worker: Option<TaskWorker>,
    /// Bounded result summary, when the task finished.
    pub result_summary: Option<String>,
    /// Registry model id the task runs on, when a subagent selection
    /// resolved one (explicit selection or parent fallback); root tasks leave
    /// this unset.
    #[serde(default)]
    pub model_id: Option<String>,
}

impl TaskNode {
    /// Creates an idle (pending) task with no links.
    #[must_use]
    pub fn new(id: TaskId, title: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            description: description.into(),
            parent: None,
            dependencies: Vec::new(),
            status: TaskStatus::Pending,
            assigned_worker: None,
            result_summary: None,
            model_id: None,
        }
    }

    /// Records the worker assigned to this task.
    pub fn set_worker(&mut self, worker: TaskWorker) {
        self.assigned_worker = Some(worker);
    }

    /// Records the registry model id this task runs on.
    pub fn set_model_id(&mut self, model_id: Option<String>) {
        self.model_id = model_id;
    }

    /// Records the result summary, truncated to
    /// [`MAX_RESULT_SUMMARY_CHARS`].
    pub fn set_result_summary(&mut self, summary: impl Into<String>) {
        let summary = summary.into();
        self.result_summary = Some(truncate(&summary, MAX_RESULT_SUMMARY_CHARS));
    }
}

/// Deterministic task graph.
///
/// Ids are generated from a per-graph monotonic counter (`task-1`, `task-2`,
/// ...), so behavior is reproducible for any given construction sequence.
/// Queries iterate the backing `BTreeMap`, so results are always in stable id
/// order.  Serializable so session state can be persisted and restored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraph {
    next: u64,
    tasks: BTreeMap<TaskId, TaskNode>,
}

impl TaskGraph {
    /// Creates an empty graph whose first id is `task-1`.
    #[must_use]
    pub fn new() -> Self {
        Self { next: 1, tasks: BTreeMap::new() }
    }

    /// Creates and stores a running root task, returning it.
    pub fn create_root(&mut self, title: &str, description: &str) -> TaskNode {
        let id = TaskId::new(format!("{DEFAULT_TASK_ID_PREFIX}-{}", self.next));
        self.next += 1;
        let mut task = TaskNode::new(id, title, description);
        task.status = TaskStatus::Running;
        self.tasks.insert(task.id.clone(), task.clone());
        task
    }

    /// Creates and stores a pending child task under `parent`.
    pub fn create_child(
        &mut self,
        parent: &TaskId,
        title: &str,
        description: &str,
    ) -> Result<TaskNode, OrchestratorError> {
        if !self.tasks.contains_key(parent) {
            return Err(OrchestratorError::InvalidState(format!("unknown parent task {parent}")));
        }
        let id = TaskId::new(format!("{DEFAULT_TASK_ID_PREFIX}-{}", self.next));
        self.next += 1;
        let mut task = TaskNode::new(id, title, description);
        task.parent = Some(parent.clone());
        self.tasks.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    /// Adds a dependency edge: `task` cannot start until `prerequisite`
    /// completes.  Rejects unknown tasks, self-dependencies, and cycles.
    pub fn add_dependency(
        &mut self,
        task: &TaskId,
        prerequisite: &TaskId,
    ) -> Result<(), OrchestratorError> {
        if task == prerequisite {
            return Err(OrchestratorError::InvalidState("a task cannot depend on itself".into()));
        }
        if !self.tasks.contains_key(task) {
            return Err(OrchestratorError::InvalidState(format!("unknown task {task}")));
        }
        if !self.tasks.contains_key(prerequisite) {
            return Err(OrchestratorError::InvalidState(format!(
                "unknown prerequisite {prerequisite}"
            )));
        }
        if self.creates_cycle(task, prerequisite) {
            return Err(OrchestratorError::InvalidState(format!(
                "dependency edge {task} -> {prerequisite} would create a cycle"
            )));
        }
        let node = self.tasks.get_mut(task).expect("task checked above");
        if !node.dependencies.contains(prerequisite) {
            node.dependencies.push(prerequisite.clone());
        }
        Ok(())
    }

    /// Whether `task` is reachable by following `prerequisite`'s dependency
    /// chain (which would make the edge a cycle).
    fn creates_cycle(&self, task: &TaskId, prerequisite: &TaskId) -> bool {
        let mut seen: BTreeSet<TaskId> = BTreeSet::new();
        let mut stack = vec![prerequisite.clone()];
        while let Some(current) = stack.pop() {
            if &current == task {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some(node) = self.tasks.get(&current) {
                stack.extend(node.dependencies.iter().cloned());
            }
        }
        false
    }

    /// Transitions a task to `to`, rejecting invalid transitions.
    pub fn transition(&mut self, id: &TaskId, to: TaskStatus) -> Result<(), OrchestratorError> {
        let Some(node) = self.tasks.get_mut(id) else {
            return Err(OrchestratorError::InvalidState(format!("unknown task {id}")));
        };
        if !can_transition(node.status, to) {
            return Err(OrchestratorError::InvalidState(format!(
                "invalid task transition {:?} -> {:?} for {id}",
                node.status, to
            )));
        }
        node.status = to;
        Ok(())
    }

    /// Tasks that are pending and whose dependencies are all completed, in
    /// stable id order.
    #[must_use]
    pub fn ready_tasks(&self) -> Vec<TaskNode> {
        self.tasks
            .values()
            .filter(|task| task.status == TaskStatus::Pending)
            .filter(|task| {
                task.dependencies.iter().all(|dependency| {
                    self.tasks
                        .get(dependency)
                        .is_some_and(|node| node.status == TaskStatus::Completed)
                })
            })
            .cloned()
            .collect()
    }

    /// Completed tasks in stable id order.
    #[must_use]
    pub fn completed_tasks(&self) -> Vec<TaskNode> {
        self.tasks.values().filter(|task| task.status == TaskStatus::Completed).cloned().collect()
    }

    /// Projects the graph onto ACP plan entries, in stable id order.
    ///
    /// The ACP v1 plan format cannot express failed or cancelled tasks, so
    /// terminal non-completed tasks are omitted; blocked tasks appear as
    /// pending (not started, waiting on dependencies).
    #[must_use]
    pub fn plan_entries(&self) -> Vec<PlanEntry> {
        self.tasks
            .values()
            .filter_map(|task| {
                let status = match task.status {
                    TaskStatus::Pending | TaskStatus::Blocked => PlanEntryStatus::Pending,
                    TaskStatus::Running => PlanEntryStatus::InProgress,
                    TaskStatus::Completed => PlanEntryStatus::Completed,
                    TaskStatus::Failed | TaskStatus::Cancelled => return None,
                };
                Some(PlanEntry::new(task.title.clone(), PlanEntryPriority::Medium, status))
            })
            .collect()
    }

    /// Looks up a task by id.
    #[must_use]
    pub fn get(&self, id: &TaskId) -> Option<&TaskNode> {
        self.tasks.get(id)
    }

    /// Records a bounded result summary on an existing task.
    pub fn set_result_summary(
        &mut self,
        id: &TaskId,
        summary: impl Into<String>,
    ) -> Result<(), OrchestratorError> {
        let Some(node) = self.tasks.get_mut(id) else {
            return Err(OrchestratorError::InvalidState(format!("unknown task {id}")));
        };
        node.set_result_summary(summary);
        Ok(())
    }

    /// Records the resolved registry model id on an existing task.
    pub fn set_model_id(
        &mut self,
        id: &TaskId,
        model_id: Option<String>,
    ) -> Result<(), OrchestratorError> {
        let Some(node) = self.tasks.get_mut(id) else {
            return Err(OrchestratorError::InvalidState(format!("unknown task {id}")));
        };
        node.set_model_id(model_id);
        Ok(())
    }

    /// All tasks in stable (id) order.
    #[must_use]
    pub fn list(&self) -> Vec<TaskNode> {
        self.tasks.values().cloned().collect()
    }

    /// Number of stored tasks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the graph is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Validates every stored reference: parents, dependencies, and subagent
    /// workers must point at tasks that exist in the graph, and the id
    /// counter must never be zero (which would regenerate ids).  Used by
    /// checkpoint restore to reject corrupted state.
    pub fn validate_references(&self) -> Result<(), OrchestratorError> {
        if self.next == 0 {
            return Err(OrchestratorError::InvalidState("task graph id counter is zero".into()));
        }
        for node in self.tasks.values() {
            if let Some(parent) = &node.parent
                && !self.tasks.contains_key(parent)
            {
                return Err(OrchestratorError::InvalidState(format!(
                    "task {} references unknown parent {parent}",
                    node.id
                )));
            }
            for dependency in &node.dependencies {
                if !self.tasks.contains_key(dependency) {
                    return Err(OrchestratorError::InvalidState(format!(
                        "task {} references unknown dependency {dependency}",
                        node.id
                    )));
                }
            }
            if let Some(TaskWorker::Subagent(worker)) = &node.assigned_worker
                && !self.tasks.contains_key(worker)
            {
                return Err(OrchestratorError::InvalidState(format!(
                    "task {} references unknown subagent worker {worker}",
                    node.id
                )));
            }
        }
        Ok(())
    }
}

/// Allowed status transitions; terminal states never leave.
fn can_transition(from: TaskStatus, to: TaskStatus) -> bool {
    matches!(
        (from, to),
        (TaskStatus::Pending, TaskStatus::Running)
            | (TaskStatus::Pending, TaskStatus::Blocked)
            | (TaskStatus::Pending, TaskStatus::Cancelled)
            | (TaskStatus::Running, TaskStatus::Blocked)
            | (TaskStatus::Running, TaskStatus::Completed)
            | (TaskStatus::Running, TaskStatus::Failed)
            | (TaskStatus::Running, TaskStatus::Cancelled)
            | (TaskStatus::Blocked, TaskStatus::Pending)
            | (TaskStatus::Blocked, TaskStatus::Running)
            | (TaskStatus::Blocked, TaskStatus::Cancelled)
    )
}

/// Truncates text to `max_chars` characters, appending an ellipsis when it
/// was cut.  Shared by the task graph, runtime, and subagent summaries.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max_chars).collect();
        truncated.push('…');
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_root_generates_deterministic_ids() {
        let mut graph = TaskGraph::new();
        let first = graph.create_root("one", "first");
        let second = graph.create_root("two", "second");
        assert_eq!(first.id, TaskId::new("task-1"));
        assert_eq!(second.id, TaskId::new("task-2"));
        assert_eq!(first.status, TaskStatus::Running);
        assert_eq!(first.parent, None);
        assert_eq!(first.title, "one");
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn create_child_links_to_parent_and_starts_pending() {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("root", "do work");
        let child = graph.create_child(&root.id, "sub", "sub work").expect("creates child");
        assert_eq!(child.parent, Some(root.id.clone()));
        assert_eq!(child.status, TaskStatus::Pending);
        assert_eq!(graph.get(&child.id).expect("stored").parent, Some(root.id));

        let error =
            graph.create_child(&TaskId::new("task-99"), "x", "y").expect_err("unknown parent");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn valid_status_transitions_are_accepted() {
        let mut graph = TaskGraph::new();
        let task = graph.create_root("t", "d");
        for (to, expected) in [
            (TaskStatus::Blocked, TaskStatus::Blocked),
            (TaskStatus::Running, TaskStatus::Running),
            (TaskStatus::Completed, TaskStatus::Completed),
        ] {
            graph.transition(&task.id, to).expect("transition is valid");
            assert_eq!(graph.get(&task.id).expect("exists").status, expected);
        }
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        let mut graph = TaskGraph::new();
        let task = graph.create_root("t", "d"); // Running

        // Same-status and backward transitions from Running are invalid.
        for to in [TaskStatus::Running, TaskStatus::Pending] {
            let error = graph.transition(&task.id, to).expect_err("rejected");
            assert!(
                matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("invalid task transition")),
                "unexpected error: {error}"
            );
            assert_eq!(graph.get(&task.id).expect("exists").status, TaskStatus::Running);
        }

        graph.transition(&task.id, TaskStatus::Blocked).expect("blocks");
        // From Blocked only Pending/Running/Cancelled are valid.
        for to in [TaskStatus::Blocked, TaskStatus::Completed, TaskStatus::Failed] {
            let error = graph.transition(&task.id, to).expect_err("rejected");
            assert!(
                matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("invalid task transition")),
                "unexpected error: {error}"
            );
            assert_eq!(graph.get(&task.id).expect("exists").status, TaskStatus::Blocked);
        }

        graph.transition(&task.id, TaskStatus::Running).expect("runs");
        graph.transition(&task.id, TaskStatus::Completed).expect("completes");
        // Terminal states are final.
        for to in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Blocked,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            graph.transition(&task.id, to).expect_err("terminal state is final");
        }
        assert_eq!(graph.get(&task.id).expect("exists").status, TaskStatus::Completed);

        let error =
            graph.transition(&TaskId::new("task-99"), TaskStatus::Running).expect_err("unknown");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
    }

    #[test]
    fn ready_tasks_follow_dependency_ordering() {
        let mut graph = TaskGraph::new();
        graph.create_root("root", "r");
        let first = graph.create_child(&TaskId::new("task-1"), "first", "f").expect("child");
        let second = graph.create_child(&TaskId::new("task-1"), "second", "s").expect("child");
        graph.add_dependency(&second.id, &first.id).expect("edge");

        // The first child has no dependencies and is ready immediately.
        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, first.id);

        graph.transition(&first.id, TaskStatus::Running).expect("runs");
        assert!(graph.ready_tasks().is_empty(), "second still waits");

        graph.transition(&first.id, TaskStatus::Completed).expect("completes");
        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, second.id, "only the second child is ready");
    }

    #[test]
    fn dependency_edges_reject_self_unknown_and_cycles() {
        let mut graph = TaskGraph::new();
        graph.create_root("root", "r");
        let a = graph.create_child(&TaskId::new("task-1"), "a", "a").expect("child");
        let b = graph.create_child(&TaskId::new("task-1"), "b", "b").expect("child");

        let error = graph.add_dependency(&a.id, &a.id).expect_err("self");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
        let error = graph.add_dependency(&TaskId::new("task-99"), &a.id).expect_err("unknown");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));
        let error =
            graph.add_dependency(&a.id, &TaskId::new("task-99")).expect_err("unknown prereq");
        assert!(matches!(error, OrchestratorError::InvalidState(_)));

        graph.add_dependency(&b.id, &a.id).expect("b waits on a");
        let error = graph.add_dependency(&a.id, &b.id).expect_err("cycle");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("cycle"))
        );
    }

    #[test]
    fn completed_summary_query_returns_only_completed() {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("root", "r");
        let child = graph.create_child(&root.id, "child", "c").expect("child");
        graph.transition(&root.id, TaskStatus::Blocked).expect("blocks");
        graph.transition(&root.id, TaskStatus::Running).expect("runs");
        graph.transition(&child.id, TaskStatus::Running).expect("runs");
        graph.transition(&child.id, TaskStatus::Completed).expect("completes");

        let completed = graph.completed_tasks();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, child.id);
    }

    #[test]
    fn result_summary_is_bounded() {
        let mut node = TaskNode::new(TaskId::new("task-1"), "t", "d");
        node.set_result_summary("ok");
        assert_eq!(node.result_summary.as_deref(), Some("ok"));

        let long = "x".repeat(MAX_RESULT_SUMMARY_CHARS + 100);
        node.set_result_summary(long.clone());
        let stored = node.result_summary.expect("stored");
        assert_eq!(stored.chars().count(), MAX_RESULT_SUMMARY_CHARS + 1);
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn plan_entries_project_task_state() {
        let mut graph = TaskGraph::new();
        let root = graph.create_root("root", "r");
        let done = graph.create_child(&root.id, "done", "d").expect("child");
        let blocked = graph.create_child(&root.id, "blocked", "b").expect("child");
        let waiting = graph.create_child(&root.id, "waiting", "w").expect("child");
        let failed = graph.create_child(&root.id, "failed", "f").expect("child");
        let cancelled = graph.create_child(&root.id, "cancelled", "c").expect("child");

        graph.transition(&done.id, TaskStatus::Running).expect("runs");
        graph.transition(&done.id, TaskStatus::Completed).expect("completes");
        graph.transition(&blocked.id, TaskStatus::Blocked).expect("blocks");
        graph.transition(&failed.id, TaskStatus::Running).expect("runs");
        graph.transition(&failed.id, TaskStatus::Failed).expect("fails");
        graph.transition(&cancelled.id, TaskStatus::Cancelled).expect("cancels");
        graph.transition(&waiting.id, TaskStatus::Pending).expect_err("no-op is invalid");

        let entries = graph.plan_entries();
        // Running root, completed child, blocked child, pending child; the
        // failed and cancelled children are not representable and omitted.
        let statuses: Vec<PlanEntryStatus> =
            entries.iter().map(|entry| entry.status.clone()).collect();
        assert_eq!(
            statuses,
            vec![
                PlanEntryStatus::InProgress,
                PlanEntryStatus::Completed,
                PlanEntryStatus::Pending,
                PlanEntryStatus::Pending,
            ]
        );
        let titles: Vec<&str> = entries.iter().map(|entry| entry.content.as_str()).collect();
        assert_eq!(titles, vec!["root", "done", "blocked", "waiting"]);
    }

    #[test]
    fn get_and_list_roundtrip_in_stable_order() {
        let mut graph = TaskGraph::new();
        let first = graph.create_root("one", "first");
        graph.create_root("two", "second");

        assert_eq!(graph.get(&first.id), Some(&first));
        assert_eq!(graph.get(&TaskId::new("task-99")), None);

        let listed = graph.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, TaskId::new("task-1"));
        assert_eq!(listed[1].id, TaskId::new("task-2"));
    }

    #[test]
    fn task_ids_serialize_deterministically() {
        let id = TaskId::new("task-7");
        let json = serde_json::to_string(&id).expect("serializes");
        assert_eq!(json, "\"task-7\"");
        let restored: TaskId = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, id);
    }

    #[test]
    fn task_node_roundtrips_through_json() {
        let mut node = TaskNode::new(TaskId::new("task-1"), "t", "d");
        node.set_worker(TaskWorker::Root);
        node.set_result_summary("done");
        let json = serde_json::to_string(&node).expect("serializes");
        let restored: TaskNode = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, node);
    }
}
