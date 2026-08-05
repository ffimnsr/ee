//! Tool dependency metadata and the planned-tool dependency graph.
//!
//! Each [`ToolDefinition`] carries a [`ToolDependency`] describing the data
//! classes it consumes and produces and the path scope it mutates.  When a
//! batch of tool intents is planned, [`ToolDependencyGraph::build`] lays the
//! batch into deterministic execution waves: indices inside one wave are
//! mutually independent (no data-class path between them), every earlier wave
//! is dependency-complete, and cyclic dependencies are rejected before any
//! tool runs.  The parallel tool runner uses the waves to run independent
//! read-only tools concurrently while writes stay serialized.

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::tools::{ToolDefinition, ToolIntent};

/// Data classes one tool can consume or produce for another.
///
/// A planned tool may run only after every tool whose `produces` list covers
/// one of its `requires` entries has completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ToolDataClass {
    /// File text contents (read/write file tools).
    FileText,
    /// A terminal handle created by `create_terminal`.
    TerminalHandle,
    /// Terminal output snapshots.
    TerminalOutput,
    /// Terminal exit status.
    TerminalExit,
    /// User answers to elicitation.
    UserInput,
    /// Bounded subagent summaries from `delegate_task`.
    SubagentSummary,
}

impl ToolDataClass {
    /// Stable lowercase name for schemas and diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileText => "file_text",
            Self::TerminalHandle => "terminal_handle",
            Self::TerminalOutput => "terminal_output",
            Self::TerminalExit => "terminal_exit",
            Self::UserInput => "user_input",
            Self::SubagentSummary => "subagent_summary",
        }
    }
}

/// Dependency metadata attached to a [`ToolDefinition`].
///
/// `requires` lists data classes a prior tool must have produced before this
/// tool runs; `produces` lists the classes this tool makes available to later
/// tools.  `affected_path` is the static path scope this tool mutates; cache
/// invalidation also derives affected paths from tool arguments (write
/// tools), because paths are usually dynamic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDependency {
    /// Data classes a prior tool must have produced before this tool runs.
    #[serde(default)]
    pub requires: Vec<ToolDataClass>,
    /// Data classes this tool makes available to later tools.
    #[serde(default)]
    pub produces: Vec<ToolDataClass>,
    /// Static path scope this tool mutates; cache entries whose scope
    /// overlaps are invalidated on success.
    #[serde(default)]
    pub affected_path: Option<String>,
}

impl ToolDependency {
    /// Creates an empty dependency declaration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the required prior data classes.
    #[must_use]
    pub fn requires(mut self, classes: Vec<ToolDataClass>) -> Self {
        self.requires = classes;
        self
    }

    /// Sets the produced data classes.
    #[must_use]
    pub fn produces(mut self, classes: Vec<ToolDataClass>) -> Self {
        self.produces = classes;
        self
    }

    /// Sets the static affected path scope.
    #[must_use]
    pub fn affected_path(mut self, path: impl Into<String>) -> Self {
        self.affected_path = Some(path.into());
        self
    }
}

/// One planned tool execution: the model intent plus its resolved definition.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTool {
    /// The model's tool intent.
    pub intent: ToolIntent,
    /// The resolved tool definition, used for class and dependency lookups.
    pub definition: ToolDefinition,
}

impl PlannedTool {
    /// Creates a planned tool.
    #[must_use]
    pub fn new(intent: ToolIntent, definition: ToolDefinition) -> Self {
        Self { intent, definition }
    }
}

/// Deterministic wave layering of a planned tool batch.
///
/// `waves()` returns execution waves over input indices.  Every index in a
/// wave is independent of every other index in the same wave, all waves
/// before it are dependency-complete, and indices within a wave are sorted
/// ascending so execution order is stable.
#[derive(Debug)]
pub struct ToolDependencyGraph {
    waves: Vec<Vec<usize>>,
}

impl ToolDependencyGraph {
    /// Builds the dependency graph for a planned batch, rejecting duplicate
    /// tool-call ids and cyclic data-class dependencies.
    pub fn build(planned: &[PlannedTool]) -> Result<Self, OrchestratorError> {
        let mut seen = HashSet::new();
        for tool in planned {
            if !seen.insert(tool.intent.tool_call_id.clone()) {
                return Err(OrchestratorError::InvalidState(format!(
                    "duplicate tool call id in planned batch: {}",
                    tool.intent.tool_call_id
                )));
            }
        }

        let count = planned.len();
        let mut successors: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut indegree = vec![0usize; count];
        for (index, tool) in planned.iter().enumerate() {
            for (other_index, other) in planned.iter().enumerate() {
                // A tool requiring a class it produces is a self-cycle: it
                // would wait for its own output before running.
                let depends = tool
                    .definition
                    .dependency
                    .produces
                    .iter()
                    .any(|class| other.definition.dependency.requires.contains(class));
                if depends && index != other_index {
                    successors[index].push(other_index);
                    indegree[other_index] += 1;
                } else if depends {
                    indegree[index] += 1;
                }
            }
        }

        // Kahn's algorithm with a smallest-index-first ready queue keeps the
        // layering deterministic regardless of map iteration order.
        let mut ready: BTreeSet<usize> = (0..count).filter(|&index| indegree[index] == 0).collect();
        let mut level = vec![0usize; count];
        let mut processed = 0usize;
        while let Some(&node) = ready.iter().next() {
            ready.remove(&node);
            processed += 1;
            for &next in &successors[node] {
                indegree[next] -= 1;
                level[next] = level[next].max(level[node] + 1);
                if indegree[next] == 0 {
                    ready.insert(next);
                }
            }
        }
        if processed != count {
            return Err(OrchestratorError::InvalidState(
                "cyclic tool dependency in planned batch".into(),
            ));
        }

        let mut waves: Vec<Vec<usize>> = Vec::new();
        for (index, &depth) in level.iter().enumerate() {
            while waves.len() <= depth {
                waves.push(Vec::new());
            }
            waves[depth].push(index);
        }
        Ok(Self { waves })
    }

    /// Execution waves over input indices; indices within a wave are sorted
    /// ascending and mutually independent.
    #[must_use]
    pub fn waves(&self) -> &[Vec<usize>] {
        &self.waves
    }

    /// Flattened topological execution order over input indices.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        self.waves.iter().flatten().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str, requires: &[ToolDataClass], produces: &[ToolDataClass]) -> PlannedTool {
        PlannedTool::new(
            ToolIntent::new(name, name, json!({})),
            ToolDefinition::new(name, "test tool").dependency(
                ToolDependency::new().requires(requires.to_vec()).produces(produces.to_vec()),
            ),
        )
    }

    fn plan(tools: Vec<PlannedTool>) -> ToolDependencyGraph {
        ToolDependencyGraph::build(&tools).expect("planned batch is acyclic")
    }

    #[test]
    fn no_dependencies_form_a_single_wave_in_input_order() {
        let batch = vec![
            tool("read_a", &[], &[ToolDataClass::FileText]),
            tool("read_b", &[], &[]),
            tool("read_c", &[], &[]),
        ];
        let graph = plan(batch);
        assert_eq!(graph.waves(), &[vec![0, 1, 2]]);
        assert_eq!(graph.order(), vec![0, 1, 2]);
    }

    #[test]
    fn data_class_dependency_produces_waves() {
        let batch = vec![
            tool("terminal_output", &[ToolDataClass::TerminalHandle], &[]),
            tool("create_terminal", &[], &[ToolDataClass::TerminalHandle]),
        ];
        let graph = plan(batch);
        assert_eq!(graph.waves(), &[vec![1], vec![0]]);
        assert_eq!(graph.order(), vec![1, 0]);
    }

    #[test]
    fn chains_lay_into_increasing_waves() {
        let batch = vec![
            tool("consumer", &[ToolDataClass::TerminalOutput], &[]),
            tool("producer", &[ToolDataClass::TerminalHandle], &[ToolDataClass::TerminalOutput]),
            tool("root", &[], &[ToolDataClass::TerminalHandle]),
        ];
        let graph = plan(batch);
        assert_eq!(graph.waves(), &[vec![2], vec![1], vec![0]]);
        assert_eq!(graph.order(), vec![2, 1, 0]);
    }

    #[test]
    fn independent_tools_share_a_wave_regardless_of_other_chains() {
        let batch = vec![
            tool("a", &[], &[ToolDataClass::FileText]),
            tool("b", &[ToolDataClass::FileText], &[]),
            tool("c", &[], &[]),
        ];
        let graph = plan(batch);
        assert_eq!(graph.waves(), &[vec![0, 2], vec![1]]);
    }

    #[test]
    fn cycles_are_rejected_before_execution() {
        let batch = vec![
            tool("a", &[ToolDataClass::TerminalOutput], &[ToolDataClass::FileText]),
            tool("b", &[ToolDataClass::FileText], &[ToolDataClass::TerminalOutput]),
        ];
        let error = ToolDependencyGraph::build(&batch).expect_err("cycle is rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(ref r) if r.contains("cyclic")));
    }

    #[test]
    fn self_cycles_are_rejected() {
        let batch = vec![tool("loop", &[ToolDataClass::FileText], &[ToolDataClass::FileText])];
        let error = ToolDependencyGraph::build(&batch).expect_err("self-cycle is rejected");
        assert!(matches!(error, OrchestratorError::InvalidState(ref r) if r.contains("cyclic")));
    }

    #[test]
    fn duplicate_tool_call_ids_are_rejected() {
        let batch = vec![tool("same", &[], &[]), tool("same", &[], &[])];
        let error = ToolDependencyGraph::build(&batch).expect_err("duplicate ids are rejected");
        assert!(
            matches!(error, OrchestratorError::InvalidState(ref r) if r.contains("duplicate tool call id"))
        );
    }

    #[test]
    fn waves_cover_every_index_exactly_once() {
        let batch = vec![
            tool("a", &[], &[ToolDataClass::FileText]),
            tool("b", &[ToolDataClass::FileText], &[ToolDataClass::TerminalHandle]),
            tool("c", &[ToolDataClass::TerminalHandle], &[]),
            tool("d", &[], &[]),
        ];
        let graph = plan(batch);
        let mut covered: Vec<usize> = graph.order();
        covered.sort_unstable();
        assert_eq!(covered, vec![0, 1, 2, 3]);
    }

    #[test]
    fn data_class_names_are_stable() {
        assert_eq!(ToolDataClass::FileText.as_str(), "file_text");
        assert_eq!(ToolDataClass::TerminalHandle.as_str(), "terminal_handle");
        assert_eq!(ToolDataClass::TerminalOutput.as_str(), "terminal_output");
        assert_eq!(ToolDataClass::TerminalExit.as_str(), "terminal_exit");
        assert_eq!(ToolDataClass::UserInput.as_str(), "user_input");
        assert_eq!(ToolDataClass::SubagentSummary.as_str(), "subagent_summary");
    }
}
