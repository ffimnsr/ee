//! Plan compiler: vague model plans become executable task graphs.
//!
//! The model emits [`PlanInput`] items (title, action, scope, expected
//! result, optional verification criteria, explicit dependencies).  The
//! [`PlanCompiler`] rejects vague or unexecutable tasks — a task must carry
//! an action, a scope, an expected result, and either a verification
//! criterion or an executable `tool:` reference — resolves dependencies by
//! stable title or `#index`, and rejects unknown references and dependency
//! cycles before anything executes.  The result is a [`TaskGraph`] with a
//! root task and one pending child per plan item, plus per-task criteria for
//! issue-checklist integration.

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus, truncate};

/// Cap on task descriptions copied into the graph.
const MAX_TASK_DESCRIPTION_CHARS: usize = 4_000;
/// Title of the root task owning the compiled plan.
const PLAN_ROOT_TITLE: &str = "plan";

/// One model-emitted plan item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlanInput {
    /// Short task title (also the checklist criteria subject).
    #[serde(default)]
    pub title: String,
    /// What the task does; an executable `tool:<name>` reference marks the
    /// action as executable without verification criteria.
    #[serde(default)]
    pub action: String,
    /// The file/area the task operates on.
    #[serde(default)]
    pub scope: String,
    /// The observable expected result.
    #[serde(default)]
    pub expected_result: String,
    /// Verification criterion (command/check) that must pass for the task to
    /// count as complete; empty requires the action to carry a `tool:` ref.
    #[serde(default)]
    pub verification: String,
    /// Explicit dependencies: stable titles or `#<1-based index>` references.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl PlanInput {
    /// Creates a plan item from its required parts.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        action: impl Into<String>,
        scope: impl Into<String>,
        expected_result: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            action: action.into(),
            scope: scope.into(),
            expected_result: expected_result.into(),
            verification: String::new(),
            depends_on: Vec::new(),
        }
    }

    /// Sets the verification criterion.
    #[must_use]
    pub fn verification(mut self, verification: impl Into<String>) -> Self {
        self.verification = verification.into();
        self
    }

    /// Sets the explicit dependencies.
    #[must_use]
    pub fn depends_on(mut self, dependencies: Vec<String>) -> Self {
        self.depends_on = dependencies;
        self
    }
}

/// Verification and scope criteria for one compiled task, used by
/// issue-checklist integration and progress scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskCriteria {
    /// The compiled task node id.
    pub task_id: TaskId,
    /// The task title (stable text for checklist matching).
    pub title: String,
    /// The scope the task operates on.
    pub scope: String,
    /// The executable action text.
    pub action: String,
    /// The verification criterion, when the plan item declared one.
    pub verification: Option<String>,
}

/// A compiled plan: a task graph plus per-task criteria.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlanCompilation {
    /// Graph with one root task and one pending child per plan item; child
    /// ids are assigned in item order.
    pub graph: TaskGraph,
    /// Criteria for every child task, in item order.
    pub criteria: Vec<TaskCriteria>,
}

impl PlanCompilation {
    /// The compiled child tasks in stable id order.
    #[must_use]
    pub fn tasks(&self) -> Vec<TaskNode> {
        self.graph.list().into_iter().filter(|task| task.status == TaskStatus::Pending).collect()
    }
}

/// Compiles validated model plans into executable task graphs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanCompiler;

impl PlanCompiler {
    /// Creates a compiler.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Compiles `items` into a task graph.
    ///
    /// Every item must provide a title, an action, a scope, and an expected
    /// result, and must be executable: an action carrying a `tool:<name>`
    /// reference (validated against `known_tools` when non-empty) or a
    /// verification criterion.  Dependencies resolve by stable title or
    /// `#index`; unknown references, self-dependencies, and cycles fail
    /// closed before any task can run.
    pub fn compile(
        &self,
        items: &[PlanInput],
        known_tools: &[String],
    ) -> Result<PlanCompilation, OrchestratorError> {
        // Structural validation first: every task must be actionable.
        for (index, item) in items.iter().enumerate() {
            if item.title.trim().is_empty() {
                return Err(vague(index, "missing title"));
            }
            if item.action.trim().is_empty() {
                return Err(vague(index, "missing executable action"));
            }
            if item.scope.trim().is_empty() {
                return Err(vague(index, "missing scope"));
            }
            if item.expected_result.trim().is_empty() {
                return Err(vague(index, "missing expected result"));
            }
            let tool_ref = tool_reference(&item.action);
            if tool_ref.is_none() && item.verification.trim().is_empty() {
                return Err(vague(
                    index,
                    "no executable action (tool:<name> reference) or verification criteria",
                ));
            }
            if let Some(name) = tool_ref
                && !known_tools.is_empty()
                && !known_tools.iter().any(|tool| tool == &name)
            {
                return Err(OrchestratorError::InvalidState(format!(
                    "plan task at index {index} references unknown tool {name}"
                )));
            }
        }

        // Resolve dependencies before building the graph.
        let mut resolved: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
        for (index, item) in items.iter().enumerate() {
            for reference in &item.depends_on {
                let dependency = resolve_dependency(reference, items).ok_or_else(|| {
                    OrchestratorError::InvalidState(format!(
                        "plan task at index {index} references unknown dependency {reference}"
                    ))
                })?;
                if dependency == index {
                    return Err(OrchestratorError::InvalidState(format!(
                        "plan task at index {index} depends on itself"
                    )));
                }
                if !resolved[index].contains(&dependency) {
                    resolved[index].push(dependency);
                }
            }
        }

        let mut graph = TaskGraph::new();
        let root = graph.create_root(PLAN_ROOT_TITLE, "compiled plan");
        let mut criteria = Vec::with_capacity(items.len());
        let mut child_ids = Vec::with_capacity(items.len());
        for item in items {
            let description = truncate(&item.action, MAX_TASK_DESCRIPTION_CHARS);
            let child = graph.create_child(&root.id, &item.title, &description)?;
            child_ids.push(child.id.clone());
            criteria.push(TaskCriteria {
                task_id: child.id,
                title: item.title.clone(),
                scope: item.scope.clone(),
                action: item.action.clone(),
                verification: (!item.verification.trim().is_empty())
                    .then(|| item.verification.trim().to_string()),
            });
        }
        // Dependency edges after every child exists; cycles and unknown
        // references fail closed inside `add_dependency`.
        for (index, dependencies) in resolved.iter().enumerate() {
            for &dependency in dependencies {
                graph.add_dependency(&child_ids[index], &child_ids[dependency])?;
            }
        }
        graph.validate_references()?;
        Ok(PlanCompilation { graph, criteria })
    }
}

fn vague(index: usize, reason: &str) -> OrchestratorError {
    OrchestratorError::InvalidState(format!("vague plan task at index {index}: {reason}"))
}

/// First `tool:<name>` reference in an action, if any.
fn tool_reference(action: &str) -> Option<String> {
    let bytes = action.as_bytes();
    let mut index = 0usize;
    while index + b"tool:".len() <= bytes.len() {
        if bytes[index..].starts_with(b"tool:") {
            let start = index + b"tool:".len();
            let mut end = start;
            while end < bytes.len() && is_tool_name_byte(bytes[end]) {
                end += 1;
            }
            let name = action[start..end].to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
        index += 1;
    }
    None
}

fn is_tool_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// Resolves `#<1-based index>` or a stable title to an item index.
fn resolve_dependency(reference: &str, items: &[PlanInput]) -> Option<usize> {
    let reference = reference.trim();
    if let Some(index) = reference.strip_prefix('#') {
        let position: usize = index.trim().parse().ok()?;
        if position == 0 || position > items.len() {
            return None;
        }
        return Some(position - 1);
    }
    items.iter().position(|item| item.title.trim() == reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskStatus;

    fn plan_item(index: usize) -> PlanInput {
        PlanInput::new(
            format!("task {index}"),
            format!("tool:read_file path=/work/f{index}.rs"),
            format!("/work/f{index}.rs"),
            format!("f{index}.rs read"),
        )
        .verification(format!("cargo test --quiet f{index}"))
    }

    #[test]
    fn valid_plan_compiles_into_ordered_tasks() {
        let items = vec![plan_item(1), plan_item(2)];
        let compilation =
            PlanCompiler::new().compile(&items, &["read_file".to_string()]).expect("compiles");
        let tasks = compilation.tasks();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "task 1");
        assert_eq!(tasks[1].title, "task 2");
        assert!(tasks[0].parent.is_some());
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert_eq!(compilation.criteria.len(), 2);
        assert_eq!(compilation.criteria[0].task_id, tasks[0].id);
        assert_eq!(compilation.criteria[0].verification.as_deref(), Some("cargo test --quiet f1"));
        assert_eq!(compilation.criteria[0].scope, "/work/f1.rs");
        assert!(compilation.graph.validate_references().is_ok());
    }

    #[test]
    fn verification_makes_action_executable_without_tool_reference() {
        let item = PlanInput::new("research", "read the docs", "docs/", "notes")
            .verification("cargo test --quiet");
        let compilation =
            PlanCompiler::new().compile(&[item], &[]).expect("verification satisfies the rule");
        assert_eq!(compilation.tasks().len(), 1);
    }

    #[test]
    fn tool_reference_makes_action_executable_without_verification() {
        let item =
            PlanInput::new("read", "tool:read_file path=/work/a.rs", "/work/a.rs", "content");
        let compilation =
            PlanCompiler::new().compile(&[item], &["read_file".to_string()]).expect("compiles");
        assert_eq!(compilation.criteria[0].verification, None);
    }

    #[test]
    fn vague_tasks_are_rejected_before_compilation() {
        let cases: Vec<(&str, PlanInput)> = vec![
            ("missing title", PlanInput::new("", "tool:read_file", "/work", "out")),
            ("missing action", PlanInput::new("t", "", "/work", "out")),
            ("missing scope", PlanInput::new("t", "tool:read_file", "", "out")),
            ("missing expected result", PlanInput::new("t", "tool:read_file", "/work", "")),
            ("neither tool nor verification", PlanInput::new("t", "read the file", "/work", "out")),
        ];
        for (reason, item) in cases {
            let error =
                PlanCompiler::new().compile(&[item], &[]).expect_err("vague task must be rejected");
            assert!(
                matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("vague")),
                "{reason}: {error}"
            );
        }
    }

    #[test]
    fn unknown_tool_reference_is_rejected() {
        let item = PlanInput::new(
            "read",
            "tool:definitely_missing path=/work/a.rs",
            "/work/a.rs",
            "content",
        );
        let error = PlanCompiler::new()
            .compile(&[item], &["read_file".to_string()])
            .expect_err("unknown tool rejected");
        assert!(error.to_string().contains("unknown tool"));
    }

    #[test]
    fn explicit_dependencies_resolve_by_title_and_index() {
        let mut first = plan_item(1);
        first.verification = String::new(); // executable via tool ref alone
        let mut second = plan_item(2);
        second.depends_on = vec!["task 1".to_string()];
        let mut third = plan_item(3);
        third.depends_on = vec!["#2".to_string()];
        let compilation = PlanCompiler::new()
            .compile(&[first, second, third], &["read_file".to_string()])
            .expect("compiles");
        let tasks = compilation.tasks();
        let by_title =
            |title: &str| tasks.iter().find(|task| task.title == title).expect("task").clone();
        let second = by_title("task 2");
        let third = by_title("task 3");
        assert_eq!(second.dependencies, vec![by_title("task 1").id]);
        assert_eq!(third.dependencies, vec![second.id]);
        // Only the dependency-free first task is ready.
        let ready = compilation.graph.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].title, "task 1");
    }

    #[test]
    fn unknown_dependency_reference_is_rejected() {
        let mut item = plan_item(1);
        item.depends_on = vec!["#9".to_string()];
        let error = PlanCompiler::new().compile(&[item], &[]).expect_err("unknown dependency");
        assert!(error.to_string().contains("unknown dependency"));
        let mut item = plan_item(1);
        item.depends_on = vec!["missing task".to_string()];
        let error = PlanCompiler::new().compile(&[item], &[]).expect_err("unknown dependency");
        assert!(error.to_string().contains("unknown dependency"));
    }

    #[test]
    fn dependency_cycles_are_rejected() {
        let mut first = plan_item(1);
        first.depends_on = vec!["task 2".to_string()];
        let mut second = plan_item(2);
        second.depends_on = vec!["task 1".to_string()];
        let error = PlanCompiler::new().compile(&[first, second], &[]).expect_err("cycle rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref message) if message.contains("cycle")),
            "{error}"
        );
    }

    #[test]
    fn self_dependency_is_rejected() {
        let mut item = plan_item(1);
        item.depends_on = vec!["task 1".to_string()];
        let error = PlanCompiler::new().compile(&[item], &[]).expect_err("self-dependency");
        assert!(error.to_string().contains("depends on itself"));
    }

    #[test]
    fn plan_input_and_criteria_roundtrip_through_json() {
        let item = plan_item(1);
        let json = serde_json::to_string(&item).expect("serializes");
        let restored: PlanInput = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, item);

        let compilation = PlanCompiler::new().compile(&[plan_item(1)], &[]).expect("compiles");
        let criteria = &compilation.criteria[0];
        let json = serde_json::to_string(criteria).expect("serializes");
        let restored: TaskCriteria = serde_json::from_str(&json).expect("parses");
        assert_eq!(&restored, criteria);
    }
}
