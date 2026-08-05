//! Dependency-aware parallel read-only tool execution.
//!
//! [`ParallelToolRunner`] executes a batch of tool intents through the shared
//! [`ToolExecutor`] while serializing writes: the batch is laid into
//! dependency waves ([`ToolDependencyGraph`]) and read-only tools inside one
//! wave run concurrently under a configured parallelism limit, while any
//! wave containing a write/execute tool runs serially.  Results are always
//! collected in the original intent order and every tool start/finish is
//! recorded as an [`OrchestratorEvent`], so parallel execution stays
//! deterministic for tests and tracing.

use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{ClientBridge, UpdateSink};
use tokio::sync::watch;

use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::model::ModelMessage;
use crate::tasks::TaskNode;
use crate::tool_dependencies::{PlannedTool, ToolDependencyGraph};
use crate::tools::{
    SideEffectClass, ToolDefinition, ToolExecutor, ToolIntent, ToolRegistry, ToolResult,
};

/// Runs tool batches with deterministic parallel read-only execution.
#[derive(Clone)]
pub struct ParallelToolRunner {
    executor: ToolExecutor,
    tools: Arc<Mutex<ToolRegistry>>,
    max_parallel: usize,
    events: EventRecorder,
}

impl ParallelToolRunner {
    /// Creates a runner sharing the executor, registry, and event recorder.
    ///
    /// `max_parallel` bounds how many independent read-only tools run at
    /// once; write/execute tools always run serially.
    #[must_use]
    pub fn new(
        executor: ToolExecutor,
        tools: Arc<Mutex<ToolRegistry>>,
        max_parallel: usize,
        events: EventRecorder,
    ) -> Self {
        Self { executor, tools, max_parallel: max_parallel.max(1), events }
    }

    /// Executes every intent in the batch, returning one result per intent
    /// in the original order.  Cyclic dependencies fail closed: every slot
    /// carries the same graph error and no tool runs.
    pub async fn run_batch(
        &self,
        intents: &[ToolIntent],
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &TaskNode,
        transcript: &[ModelMessage],
    ) -> Vec<Result<ToolResult, OrchestratorError>> {
        if intents.is_empty() {
            return Vec::new();
        }
        let planned = {
            let registry = self.tools.lock().expect("tool registry poisoned");
            intents
                .iter()
                .map(|intent| {
                    let definition = registry
                        .get(&intent.name)
                        .map(|tool| tool.definition())
                        .unwrap_or_else(|| {
                            ToolDefinition::new(&intent.name, "unknown tool")
                                .side_effect_class(SideEffectClass::Execute)
                        });
                    PlannedTool::new(intent.clone(), definition)
                })
                .collect::<Vec<_>>()
        };
        let graph = match ToolDependencyGraph::build(&planned) {
            Ok(graph) => graph,
            Err(error) => return vec![Err(error); intents.len()],
        };

        let mut results: Vec<Option<Result<ToolResult, OrchestratorError>>> =
            vec![None; intents.len()];
        for wave in graph.waves() {
            for &index in wave {
                self.events.record(OrchestratorEvent::ToolStarted {
                    tool_call_id: planned[index].intent.tool_call_id.clone(),
                    tool_name: planned[index].intent.name.clone(),
                });
            }
            let read_only_wave = wave
                .iter()
                .all(|&index| planned[index].definition.side_effect_class == SideEffectClass::Read);
            if read_only_wave && wave.len() > 1 {
                for chunk in wave.chunks(self.max_parallel) {
                    let outcomes = futures::future::join_all(chunk.iter().map(|&index| {
                        self.executor.execute(
                            &planned[index].intent,
                            sink,
                            client,
                            cancel.clone(),
                            task,
                            transcript,
                        )
                    }))
                    .await;
                    for (&index, outcome) in chunk.iter().zip(outcomes) {
                        results[index] = Some(outcome);
                    }
                }
            } else {
                for &index in wave {
                    let outcome = self
                        .executor
                        .execute(
                            &planned[index].intent,
                            sink,
                            client,
                            cancel.clone(),
                            task,
                            transcript,
                        )
                        .await;
                    results[index] = Some(outcome);
                }
            }
            for &index in wave {
                let success = results[index]
                    .as_ref()
                    .is_some_and(|outcome| outcome.as_ref().is_ok_and(|result| result.success));
                self.events.record(OrchestratorEvent::ToolFinished {
                    tool_call_id: planned[index].intent.tool_call_id.clone(),
                    tool_name: planned[index].intent.name.clone(),
                    success,
                });
            }
        }
        results
            .into_iter()
            .map(|slot| slot.expect("every planned intent gets a result slot"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::SessionId;
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::budget::BudgetTracker;
    use crate::config::OrchestratorConfig;
    use crate::events::EventRecorder;
    use crate::policy::{PolicyEngine, ToolPolicy};
    use crate::tasks::TaskId;
    use crate::tool_dependencies::{ToolDataClass, ToolDependency};
    use crate::tools::{ServerTool, ToolCallContext, ToolErrorKind, ToolFuture, ToolRegistry};

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn task_fixture() -> TaskNode {
        TaskNode::new(TaskId::new("task-1"), "t", "d")
    }

    /// Shared concurrency probe: tracks peak overlap and execution order.
    #[derive(Default)]
    struct ProbeState {
        active: usize,
        max_active: usize,
        order: Vec<String>,
    }

    /// A tool that records overlap and order, sleeping briefly to widen the
    /// concurrency window.
    struct ProbeTool {
        definition: ToolDefinition,
        label: String,
        state: Arc<Mutex<ProbeState>>,
    }

    impl ProbeTool {
        fn new(
            name: &str,
            class: SideEffectClass,
            label: &str,
            state: Arc<Mutex<ProbeState>>,
        ) -> Arc<Self> {
            Self::with_dependency(name, class, label, ToolDependency::new(), state)
        }

        fn with_dependency(
            name: &str,
            class: SideEffectClass,
            label: &str,
            dependency: ToolDependency,
            state: Arc<Mutex<ProbeState>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                definition: ToolDefinition::new(name, "probe tool")
                    .side_effect_class(class)
                    .dependency(dependency),
                label: label.into(),
                state,
            })
        }
    }

    impl ServerTool for ProbeTool {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn execute(
            &self,
            _arguments: Value,
            _client: ClientBridge,
            _cancel: watch::Receiver<bool>,
            _context: ToolCallContext,
        ) -> ToolFuture<ToolResult> {
            let state = self.state.clone();
            let label = self.label.clone();
            Box::pin(async move {
                {
                    let mut state = state.lock().expect("probe poisoned");
                    state.active += 1;
                    state.max_active = state.max_active.max(state.active);
                    state.order.push(label);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                {
                    let mut state = state.lock().expect("probe poisoned");
                    state.active -= 1;
                }
                ToolResult::success("done")
            })
        }
    }

    fn harness(
        max_parallel: usize,
        policy: PolicyEngine,
    ) -> (ParallelToolRunner, Arc<Mutex<ProbeState>>, Arc<Mutex<ToolRegistry>>) {
        let config = OrchestratorConfig::default();
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let executor = ToolExecutor::new(
            config,
            tools.clone(),
            Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default()))),
            policy,
            0,
            EventRecorder::new(),
        );
        let events = EventRecorder::new();
        let runner = ParallelToolRunner::new(executor, tools.clone(), max_parallel, events.clone());
        (runner, Arc::new(Mutex::new(ProbeState::default())), tools)
    }

    fn intents(pairs: &[(&str, &str)]) -> Vec<ToolIntent> {
        pairs.iter().map(|(id, name)| ToolIntent::new(*id, *name, json!({}))).collect()
    }

    async fn run_batch(
        runner: &ParallelToolRunner,
        sink: &UpdateSink,
        client: &ClientBridge,
        intents: &[ToolIntent],
    ) -> Vec<Result<ToolResult, OrchestratorError>> {
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        runner.run_batch(intents, sink, client, cancel_rx, &task_fixture(), &[]).await
    }

    fn register(tools: &Arc<Mutex<ToolRegistry>>, tool: Arc<dyn ServerTool>) {
        tools.lock().expect("registry").register(tool).expect("registers tool");
    }

    fn assert_all_success(results: &[Result<ToolResult, OrchestratorError>]) {
        for (index, result) in results.iter().enumerate() {
            assert!(
                result.as_ref().is_ok_and(|result| result.success),
                "slot {index} succeeded: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn independent_reads_run_concurrently_with_deterministic_results() {
        let (runner, state, tools) = harness(2, PolicyEngine::default());
        let probe = state.clone();
        register(&tools, ProbeTool::new("read_a", SideEffectClass::Read, "a", probe.clone()));
        register(&tools, ProbeTool::new("read_b", SideEffectClass::Read, "b", probe));
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-a", "read_a"), ("tc-b", "read_b")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        assert_all_success(&results);
        assert_eq!(state.lock().expect("probe").max_active, 2, "reads overlap");
        assert_eq!(
            results.iter().map(|r| r.as_ref().expect("ok").text_output.clone()).collect::<Vec<_>>(),
            vec!["done", "done"],
            "results stay in original intent order"
        );
    }

    #[tokio::test]
    async fn write_tools_are_serialized() {
        let policy = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
        let (runner, state, tools) = harness(2, policy);
        let probe = state.clone();
        register(&tools, ProbeTool::new("write_a", SideEffectClass::Write, "a", probe.clone()));
        register(&tools, ProbeTool::new("write_b", SideEffectClass::Write, "b", probe));
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-a", "write_a"), ("tc-b", "write_b")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        assert_all_success(&results);
        let state = state.lock().expect("probe");
        assert_eq!(state.max_active, 1, "writes never overlap");
        assert_eq!(state.order, vec!["a", "b"], "writes run in original order");
    }

    #[tokio::test]
    async fn data_class_dependencies_force_ordering() {
        let (runner, state, tools) = harness(2, PolicyEngine::default());
        let probe = state.clone();
        let consumer = ProbeTool::with_dependency(
            "consumer",
            SideEffectClass::Read,
            "consumer",
            ToolDependency::new().requires(vec![ToolDataClass::FileText]),
            probe.clone(),
        );
        let producer = ProbeTool::with_dependency(
            "producer",
            SideEffectClass::Read,
            "producer",
            ToolDependency::new().produces(vec![ToolDataClass::FileText]),
            probe.clone(),
        );
        register(&tools, consumer as Arc<dyn ServerTool>);
        register(&tools, producer as Arc<dyn ServerTool>);
        let (sink, client, _rx) = plumbing();

        // The consumer is listed first but depends on the producer's output.
        let batch = intents(&[("tc-c", "consumer"), ("tc-p", "producer")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        assert_all_success(&results);
        let state = state.lock().expect("probe");
        assert_eq!(state.order, vec!["producer", "consumer"], "dependency order wins");
        assert_eq!(state.max_active, 1, "dependent reads never overlap");
    }

    #[tokio::test]
    async fn cyclic_dependencies_fail_closed_without_running() {
        let (runner, state, tools) = harness(2, PolicyEngine::default());
        let probe = state.clone();
        let a = ProbeTool::with_dependency(
            "a",
            SideEffectClass::Read,
            "a",
            ToolDependency::new()
                .requires(vec![ToolDataClass::TerminalOutput])
                .produces(vec![ToolDataClass::FileText]),
            probe.clone(),
        );
        let b = ProbeTool::with_dependency(
            "b",
            SideEffectClass::Read,
            "b",
            ToolDependency::new()
                .requires(vec![ToolDataClass::FileText])
                .produces(vec![ToolDataClass::TerminalOutput]),
            probe.clone(),
        );
        register(&tools, a as Arc<dyn ServerTool>);
        register(&tools, b as Arc<dyn ServerTool>);
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-a", "a"), ("tc-b", "b")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        for result in &results {
            assert!(
                matches!(result, Err(OrchestratorError::InvalidState(r)) if r.contains("cyclic")),
                "cycle fails closed: {result:?}"
            );
        }
        assert!(
            state.lock().expect("probe").order.is_empty(),
            "no tool body runs on a cyclic batch"
        );
    }

    #[tokio::test]
    async fn parallelism_limit_bounds_concurrency() {
        let (runner, state, tools) = harness(2, PolicyEngine::default());
        let probe = state.clone();
        for name in ["read_1", "read_2", "read_3", "read_4"] {
            register(&tools, ProbeTool::new(name, SideEffectClass::Read, name, probe.clone()));
        }
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[
            ("tc-1", "read_1"),
            ("tc-2", "read_2"),
            ("tc-3", "read_3"),
            ("tc-4", "read_4"),
        ]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        assert_all_success(&results);
        assert!(
            state.lock().expect("probe").max_active <= 2,
            "parallelism limit of 2 is respected"
        );
    }

    #[tokio::test]
    async fn mixed_batches_preserve_original_result_order() {
        let policy = PolicyEngine::new(ToolPolicy { allow_write: true, ..ToolPolicy::default() });
        let (runner, state, tools) = harness(2, policy);
        let probe = state.clone();
        register(&tools, ProbeTool::new("read_a", SideEffectClass::Read, "a", probe.clone()));
        register(&tools, ProbeTool::new("write_b", SideEffectClass::Write, "b", probe.clone()));
        register(&tools, ProbeTool::new("read_c", SideEffectClass::Read, "c", probe));
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-a", "read_a"), ("tc-b", "write_b"), ("tc-c", "read_c")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        assert_all_success(&results);
        let state = state.lock().expect("probe");
        assert_eq!(state.order, vec!["a", "b", "c"], "waves containing writes run serially");
        assert_eq!(state.max_active, 1, "reads around a write never overlap it");
    }

    #[tokio::test]
    async fn events_record_every_start_and_finish_in_batch_order() {
        let (runner, state, tools) = harness(2, PolicyEngine::default());
        let probe = state.clone();
        register(&tools, ProbeTool::new("read_a", SideEffectClass::Read, "a", probe.clone()));
        register(&tools, ProbeTool::new("read_b", SideEffectClass::Read, "b", probe));
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-a", "read_a"), ("tc-b", "read_b")]);
        run_batch(&runner, &sink, &client, &batch).await;

        let events = runner.events.events();
        assert_eq!(
            events,
            vec![
                OrchestratorEvent::ToolStarted {
                    tool_call_id: "tc-a".into(),
                    tool_name: "read_a".into(),
                },
                OrchestratorEvent::ToolStarted {
                    tool_call_id: "tc-b".into(),
                    tool_name: "read_b".into(),
                },
                OrchestratorEvent::ToolFinished {
                    tool_call_id: "tc-a".into(),
                    tool_name: "read_a".into(),
                    success: true,
                },
                OrchestratorEvent::ToolFinished {
                    tool_call_id: "tc-b".into(),
                    tool_name: "read_b".into(),
                    success: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn unknown_tools_fail_in_their_slot_without_parallelism() {
        let (runner, _state, _tools) = harness(2, PolicyEngine::default());
        let (sink, client, _rx) = plumbing();

        let batch = intents(&[("tc-1", "ghost"), ("tc-2", "ghost2")]);
        let results = run_batch(&runner, &sink, &client, &batch).await;
        for result in &results {
            assert!(
                result
                    .as_ref()
                    .is_ok_and(|result| result.error_kind == Some(ToolErrorKind::Backend)),
                "unknown tool is a backend failure: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn empty_batch_returns_no_results() {
        let (runner, _state, _tools) = harness(2, PolicyEngine::default());
        let (sink, client, _rx) = plumbing();
        assert!(run_batch(&runner, &sink, &client, &[]).await.is_empty());
    }
}
