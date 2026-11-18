//! Session todo plan backing the model-facing `todowrite` tool.
//!
//! The model sends the full ordered list on every call and the list replaces
//! the previous one, matching the reference `todowrite` contract. Items are
//! stored in a [`TaskGraph`] so the plan reuses the existing task status
//! transitions, id allocation, and serialization, while an explicit `order`
//! vector preserves the model-supplied ordering (task ids sort lexically and
//! therefore cannot express order past nine items).
//!
//! The plan is intentionally distinct from the execution task graph: replacing
//! it never removes the root task the loop is running on. The runtime keeps one
//! plan per session, so it survives across turns, and the ACP plan update is
//! emitted from it whenever the model mutates the list.

use ee_agent_protocol::{PlanEntry, PlanEntryPriority, PlanEntryStatus};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::tasks::{TaskGraph, TaskId, TaskStatus};

/// Maximum number of todo items accepted in one update.
pub const MAX_TODO_ITEMS: usize = 100;
/// Maximum characters accepted for one todo item before truncation.
pub const MAX_TODO_CONTENT_CHARS: usize = 500;
/// Maximum todo items rendered into the prompt.
pub const MAX_PROMPT_TODO_ITEMS: usize = 20;
/// Maximum characters of one todo item rendered into the prompt.
pub const MAX_PROMPT_TODO_CONTENT_CHARS: usize = 200;

const TODO_ROOT_TITLE: &str = "todos";
const TODO_ROOT_DESCRIPTION: &str = "session todo plan";

/// Lifecycle status of one todo item, matching the reference schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Currently being worked.
    InProgress,
    /// Finished successfully.
    Completed,
    /// No longer needed.
    Cancelled,
}

impl TodoStatus {
    /// Stable schema spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_task_status(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Pending | TaskStatus::Blocked => Self::Pending,
            TaskStatus::Running => Self::InProgress,
            TaskStatus::Completed => Self::Completed,
            TaskStatus::Failed | TaskStatus::Cancelled => Self::Cancelled,
        }
    }

    fn plan_status(self) -> PlanEntryStatus {
        match self {
            Self::Pending | Self::Cancelled => PlanEntryStatus::Pending,
            Self::InProgress => PlanEntryStatus::InProgress,
            Self::Completed => PlanEntryStatus::Completed,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Completed => "[x]",
            Self::Cancelled => "[-]",
        }
    }
}

/// One todo item exactly as the model supplies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TodoItem {
    /// Brief description of the task.
    pub content: String,
    /// Current status of the task.
    pub status: TodoStatus,
}

impl TodoItem {
    /// Creates a todo item.
    #[must_use]
    pub fn new(content: impl Into<String>, status: TodoStatus) -> Self {
        Self { content: content.into(), status }
    }
}

/// Ordered session todo plan.
///
/// `Default` and `new` build an empty plan with a running root task so a plan
/// is always self-consistent; `replace` swaps in a fresh graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoPlan {
    graph: TaskGraph,
    order: Vec<TaskId>,
}

impl Default for TodoPlan {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoPlan {
    /// Creates an empty plan.
    #[must_use]
    pub fn new() -> Self {
        let mut graph = TaskGraph::new();
        graph.create_root(TODO_ROOT_TITLE, TODO_ROOT_DESCRIPTION);
        Self { graph, order: Vec::new() }
    }

    /// Replaces the plan with `items`, preserving their order.
    ///
    /// Fails closed on an empty item, too many items, or an invalid status
    /// transition; on failure the previous plan is left untouched.
    pub fn replace(&mut self, items: &[TodoItem]) -> Result<(), OrchestratorError> {
        if items.len() > MAX_TODO_ITEMS {
            return Err(OrchestratorError::InvalidState(format!(
                "todowrite accepts at most {MAX_TODO_ITEMS} items, got {}",
                items.len()
            )));
        }
        let mut graph = TaskGraph::new();
        let root = graph.create_root(TODO_ROOT_TITLE, TODO_ROOT_DESCRIPTION).id;
        let mut order = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            if item.content.trim().is_empty() {
                return Err(OrchestratorError::InvalidState(format!(
                    "todowrite item at index {index} has empty content"
                )));
            }
            let content = truncate(&item.content, MAX_TODO_CONTENT_CHARS);
            let child = graph.create_child(&root, &content, "")?;
            transition_to(&mut graph, &child.id, item.status)?;
            order.push(child.id);
        }
        graph.validate_references()?;
        self.graph = graph;
        self.order = order;
        Ok(())
    }

    /// Whether the plan holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Items in model-supplied order.
    #[must_use]
    pub fn items(&self) -> Vec<TodoItem> {
        self.order
            .iter()
            .filter_map(|id| self.graph.get(id))
            .map(|task| {
                TodoItem::new(task.title.clone(), TodoStatus::from_task_status(task.status))
            })
            .collect()
    }

    /// ACP plan entries in model-supplied order.
    #[must_use]
    pub fn plan_entries(&self) -> Vec<PlanEntry> {
        self.order
            .iter()
            .filter_map(|id| self.graph.get(id))
            .map(|task| {
                PlanEntry::new(
                    task.title.clone(),
                    PlanEntryPriority::Medium,
                    TodoStatus::from_task_status(task.status).plan_status(),
                )
            })
            .collect()
    }

    /// Bounded prompt section describing the current plan.
    ///
    /// Returns `None` when the plan is empty so the prompt stays unchanged.
    #[must_use]
    pub fn render_prompt(&self) -> Option<String> {
        if self.order.is_empty() {
            return None;
        }
        let mut lines = vec!["Current session plan (`todowrite` replaces this list):".to_string()];
        for item in self.items().into_iter().take(MAX_PROMPT_TODO_ITEMS) {
            let content = truncate(&item.content, MAX_PROMPT_TODO_CONTENT_CHARS);
            lines.push(format!("- {} {}", item.status.marker(), content));
        }
        let hidden = self.order.len().saturating_sub(MAX_PROMPT_TODO_ITEMS);
        if hidden > 0 {
            lines.push(format!("- … and {hidden} more item(s)"));
        }
        Some(lines.join("\n"))
    }

    /// One-line summary used as the tool observation.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.order.is_empty() {
            return "Todos cleared".to_string();
        }
        let items = self.items();
        let total = items.len();
        let count = |status: TodoStatus| items.iter().filter(|item| item.status == status).count();
        let mut parts = vec![format!("{}/{total} completed", count(TodoStatus::Completed))];
        for status in [TodoStatus::InProgress, TodoStatus::Pending, TodoStatus::Cancelled] {
            let value = count(status);
            if value > 0 {
                parts.push(format!("{value} {}", status.as_str().replace('_', " ")));
            }
        }
        let active: Vec<&str> = items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .map(|item| item.content.as_str())
            .collect();
        let mut summary = format!("Todos updated: {}", parts.join(", "));
        if !active.is_empty() {
            summary.push_str(". In progress: ");
            summary.push_str(&active.join(", "));
        }
        summary
    }
}

fn transition_to(
    graph: &mut TaskGraph,
    id: &TaskId,
    status: TodoStatus,
) -> Result<(), OrchestratorError> {
    match status {
        TodoStatus::Pending => Ok(()),
        TodoStatus::InProgress => graph.transition(id, TaskStatus::Running),
        TodoStatus::Completed => {
            graph.transition(id, TaskStatus::Running)?;
            graph.transition(id, TaskStatus::Completed)
        }
        TodoStatus::Cancelled => graph.transition(id, TaskStatus::Cancelled),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(content, status)
    }

    #[test]
    fn replace_preserves_order_and_statuses() {
        let mut plan = TodoPlan::new();
        plan.replace(&[
            item("first", TodoStatus::Completed),
            item("second", TodoStatus::InProgress),
            item("third", TodoStatus::Pending),
            item("fourth", TodoStatus::Cancelled),
        ])
        .expect("replaces");

        let items = plan.items();
        assert_eq!(
            items.iter().map(|item| item.content.as_str()).collect::<Vec<_>>(),
            vec!["first", "second", "third", "fourth"]
        );
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[3].status, TodoStatus::Cancelled);
    }

    #[test]
    fn replace_adds_and_removes_items() {
        let mut plan = TodoPlan::new();
        plan.replace(&[item("one", TodoStatus::Pending), item("two", TodoStatus::Pending)])
            .expect("replaces");
        plan.replace(&[item("two", TodoStatus::Pending)]).expect("replaces");
        assert_eq!(plan.items().len(), 1);
        assert_eq!(plan.items()[0].content, "two");
    }

    #[test]
    fn empty_replace_clears_plan() {
        let mut plan = TodoPlan::new();
        plan.replace(&[item("one", TodoStatus::Pending)]).expect("replaces");
        plan.replace(&[]).expect("clears");
        assert!(plan.is_empty());
        assert!(plan.plan_entries().is_empty());
        assert!(plan.render_prompt().is_none());
        assert_eq!(plan.summary(), "Todos cleared");
    }

    #[test]
    fn invalid_update_leaves_previous_plan_intact() {
        let mut plan = TodoPlan::new();
        plan.replace(&[item("kept", TodoStatus::Pending)]).expect("replaces");
        plan.replace(&[item("  ", TodoStatus::Pending)]).expect_err("empty content rejected");
        assert_eq!(plan.items().len(), 1);
        assert_eq!(plan.items()[0].content, "kept");
    }

    #[test]
    fn too_many_items_are_rejected() {
        let mut plan = TodoPlan::new();
        let items = vec![item("x", TodoStatus::Pending); MAX_TODO_ITEMS + 1];
        plan.replace(&items).expect_err("bound enforced");
    }

    #[test]
    fn plan_entries_map_statuses() {
        let mut plan = TodoPlan::new();
        plan.replace(&[
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
            item("c", TodoStatus::Completed),
            item("d", TodoStatus::Cancelled),
        ])
        .expect("replaces");
        let entries = plan.plan_entries();
        assert_eq!(entries[0].status, PlanEntryStatus::Pending);
        assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
        assert_eq!(entries[2].status, PlanEntryStatus::Completed);
        assert_eq!(entries[3].status, PlanEntryStatus::Pending);
    }

    #[test]
    fn prompt_rendering_is_bounded() {
        let mut plan = TodoPlan::new();
        let items: Vec<TodoItem> = (0..MAX_PROMPT_TODO_ITEMS + 5)
            .map(|index| item(&format!("item {index}"), TodoStatus::Pending))
            .collect();
        plan.replace(&items).expect("replaces");
        let prompt = plan.render_prompt().expect("renders");
        assert!(prompt.contains("and 5 more item(s)"));
        assert!(!prompt.contains("item 24"));
    }

    #[test]
    fn summary_counts_statuses() {
        let mut plan = TodoPlan::new();
        plan.replace(&[
            item("done", TodoStatus::Completed),
            item("active", TodoStatus::InProgress),
            item("later", TodoStatus::Pending),
        ])
        .expect("replaces");
        let summary = plan.summary();
        assert!(summary.contains("1/3 completed"), "{summary}");
        assert!(summary.contains("1 in progress"), "{summary}");
        assert!(summary.contains("In progress: active"), "{summary}");
    }

    #[test]
    fn plan_roundtrips_through_json() {
        let mut plan = TodoPlan::new();
        plan.replace(&[item("one", TodoStatus::InProgress)]).expect("replaces");
        let json = serde_json::to_string(&plan).expect("serializes");
        let restored: TodoPlan = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(restored.items(), plan.items());
    }
}
