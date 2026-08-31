//! Fan-out/fan-in coordination for ready independent subagent tasks.
//!
//! The coordinator splits the ready, independent children of a parent task
//! into [`SubagentRequest`] values, runs them through an injected spawn
//! function bounded by the configured parallelism, collects the child
//! handoffs in deterministic task order, merges completed handoff JSON into the
//! parent transcript, and marks the parent task blocked when a required child
//! fails. A [`WriteScopeConflictDetector`] is active by default, so overlapping
//! intended write scopes of concurrent children are rejected before any spawn;
//! locks are released when the child finishes or is cancelled.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::error::OrchestratorError;
use crate::model::{ModelMessage, ModelRole};
use crate::subagent_handoff::{SubagentHandoff, SubagentStatus};

use crate::subagents::{SubagentId, SubagentRequest, SubagentResult, SubagentRole};
use crate::tasks::{TaskGraph, TaskId, TaskNode, TaskStatus};
use crate::workspace_scope::WorkspaceScope;
use crate::write_conflicts::WriteScopeConflictDetector;

/// Boxed spawn outcome future, unifying spawned and pre-failed children in one
/// batch.
type SpawnOutcomeFuture =
    Pin<Box<dyn Future<Output = Result<SubagentResult, OrchestratorError>> + Send>>;

/// Splits ready independent tasks into subagent requests and fans them out
/// with deterministic merge and parent blocking.
#[derive(Debug, Clone)]
pub struct FanOutFanInCoordinator {
    max_parallel: usize,
    tasks: Arc<Mutex<TaskGraph>>,
    write_conflicts: Option<Arc<Mutex<WriteScopeConflictDetector>>>,
}

impl FanOutFanInCoordinator {
    /// Creates a coordinator running at most `max_parallel` children at once
    /// over the shared task graph.
    #[must_use]
    pub fn new(max_parallel: usize, tasks: Arc<Mutex<TaskGraph>>) -> Self {
        Self {
            max_parallel,
            tasks,
            write_conflicts: Some(Arc::new(Mutex::new(WriteScopeConflictDetector::new()))),
        }
    }

    /// Replaces the default write-scope conflict detector. Children whose
    /// intended write scopes overlap are rejected before spawn.
    #[must_use]
    pub fn with_write_conflicts(
        mut self,
        detector: Arc<Mutex<WriteScopeConflictDetector>>,
    ) -> Self {
        self.write_conflicts = Some(detector);
        self
    }

    /// Pending children of `parent` whose dependencies are all completed, in
    /// stable task-id order.
    #[must_use]
    pub fn ready_children(&self, parent: &TaskId) -> Vec<TaskNode> {
        let tasks = self.tasks.lock().expect("task graph poisoned");
        tasks
            .ready_tasks()
            .into_iter()
            .filter(|task| task.parent.as_ref() == Some(parent))
            .collect()
    }

    /// Builds delegation requests for the ready children of `parent` in
    /// stable task-id order.  Each request's prompt is the child task
    /// description (falling back to its title); context snapshots and write
    /// scopes are left empty for the caller to fill.
    #[must_use]
    pub fn plan_requests(
        &self,
        parent: &TaskId,
        role: SubagentRole,
        scope: Option<WorkspaceScope>,
    ) -> Vec<SubagentRequest> {
        self.ready_children(parent)
            .into_iter()
            .map(|child| {
                let scoped_prompt = if child.description.is_empty() {
                    child.title.clone()
                } else {
                    child.description.clone()
                };
                SubagentRequest {
                    parent_task_id: parent.clone(),
                    child_task_id: child.id.clone(),
                    role: role.clone(),
                    scoped_prompt,
                    context_snapshot: Vec::new(),
                    scope: scope.clone(),
                    write_scope: Vec::new(),
                    model_id: None,
                }
            })
            .collect()
    }

    /// Runs the requests through `spawn`, bounding concurrency to
    /// `max_parallel` chunks in request order, and returns the results in the
    /// original request order.
    ///
    /// Every child task transitions `pending -> running` before any spawn and
    /// reaches its terminal status from the result (`completed`, `failed`, or
    /// `cancelled`), with the bounded summary recorded.  A spawn error
    /// becomes a deterministic `failed` result.  Write-scope conflicts fail
    /// the conflicting child before its spawn function is called.
    pub async fn run<F, Fut>(&self, requests: Vec<SubagentRequest>, spawn: F) -> Vec<SubagentResult>
    where
        F: Fn(SubagentRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<SubagentResult, OrchestratorError>> + Send + 'static,
    {
        if requests.is_empty() {
            return Vec::new();
        }
        {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            for request in &requests {
                tasks
                    .transition(&request.child_task_id, TaskStatus::Running)
                    .expect("pending child task -> running");
            }
        }

        // Acquire write-scope locks before any spawn; a conflict fails the
        // conflicting child closed without invoking the spawn function.
        let mut conflicted: BTreeMap<usize, String> = BTreeMap::new();
        if let Some(detector) = &self.write_conflicts {
            let mut detector = detector.lock().expect("write-scope detector poisoned");
            for (index, request) in requests.iter().enumerate() {
                if request.write_scope.is_empty() {
                    continue;
                }
                let subagent = SubagentId::new(request.child_task_id.as_str());
                if let Err(error) = detector.acquire(&subagent, request.write_scope.clone()) {
                    conflicted.insert(index, error.to_string());
                }
            }
        }

        let max_parallel = self.max_parallel.max(1);
        let mut results = Vec::with_capacity(requests.len());
        let mut start = 0usize;
        while start < requests.len() {
            let end = (start + max_parallel).min(requests.len());
            let mut futures: Vec<SpawnOutcomeFuture> = Vec::with_capacity(end - start);
            for (index, request) in requests.iter().enumerate().take(end).skip(start) {
                if let Some(reason) = conflicted.get(&index) {
                    let reason = reason.clone();
                    futures.push(Box::pin(
                        async move { Err(OrchestratorError::PolicyDenied(reason)) },
                    ));
                } else {
                    let request = request.clone();
                    futures.push(Box::pin(spawn(request)));
                }
            }
            let outcomes = futures::future::join_all(futures).await;
            for (outcome, request) in outcomes.into_iter().zip(&requests[start..end]) {
                let result = match outcome {
                    Ok(result) => match result.validate_against(
                        request.child_task_id.as_str(),
                        Some(&request.role.name),
                        None,
                    ) {
                        Ok(()) => result,
                        Err(error) => failed_result(request, error),
                    },
                    Err(error) => failed_result(request, error),
                };
                results.push(result);
            }
            start = end;
        }

        // Apply terminal statuses and bounded summaries, then release every
        // write-scope lock the coordinator acquired.
        {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            for (request, result) in requests.iter().zip(&results) {
                let (status, summary) = match result.handoff.status {
                    SubagentStatus::Completed => {
                        (TaskStatus::Completed, result.handoff.summary.clone())
                    }
                    SubagentStatus::Failed => (
                        TaskStatus::Failed,
                        result.error_summary.clone().unwrap_or_else(|| "subagent failed".into()),
                    ),
                    SubagentStatus::Cancelled => (TaskStatus::Cancelled, String::new()),
                };
                tasks.transition(&request.child_task_id, status).expect("child task terminates");
                if !summary.is_empty() {
                    let _ = tasks.set_result_summary(&request.child_task_id, summary);
                }
            }
        }
        if let Some(detector) = &self.write_conflicts {
            let mut detector = detector.lock().expect("write-scope detector poisoned");
            for request in &requests {
                if !request.write_scope.is_empty() {
                    detector.release(&SubagentId::new(request.child_task_id.as_str()));
                }
            }
        }
        results
    }

    /// Appends structured handoffs of completed children to the parent transcript
    /// as untrusted `Subagent` messages, in deterministic result order.
    /// Failed and cancelled children contribute nothing.
    pub fn merge_summaries(&self, transcript: &mut Vec<ModelMessage>, results: &[SubagentResult]) {
        for result in results {
            if result.handoff.status == SubagentStatus::Completed
                && !result.handoff.summary.is_empty()
                && let Ok(handoff) = result.handoff.to_json()
            {
                transcript.push(ModelMessage::text(ModelRole::Subagent, handoff));
            }
        }
    }

    /// Marks the parent task blocked when any required child failed or was
    /// cancelled.  Returns whether the parent was blocked.
    pub fn mark_parent_blocked(
        &self,
        parent: &TaskId,
        results: &[SubagentResult],
    ) -> Result<bool, OrchestratorError> {
        let failed = results.iter().any(|result| {
            matches!(result.handoff.status, SubagentStatus::Failed | SubagentStatus::Cancelled)
        });
        if failed {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            tasks.transition(parent, TaskStatus::Blocked)?;
        }
        Ok(failed)
    }
}

fn failed_result(request: &SubagentRequest, error: OrchestratorError) -> SubagentResult {
    SubagentResult {
        subagent_id: SubagentId::new(request.child_task_id.as_str()),
        handoff: SubagentHandoff::terminal(
            &request.role.name,
            request.child_task_id.as_str(),
            SubagentStatus::Failed,
        ),
        produced_memory_items: Vec::new(),
        tool_call_count: 0,
        error_summary: Some(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::subagents::SubagentId;
    use crate::write_conflicts::WriteScopeConflictDetector;

    fn harness() -> (Arc<Mutex<TaskGraph>>, FanOutFanInCoordinator) {
        let tasks = Arc::new(Mutex::new(TaskGraph::new()));
        let coordinator = FanOutFanInCoordinator::new(2, tasks.clone());
        (tasks, coordinator)
    }

    fn root_with_children(tasks: &Arc<Mutex<TaskGraph>>, count: usize) -> (TaskId, Vec<TaskNode>) {
        let mut graph = tasks.lock().expect("graph");
        let root = graph.create_root("parent", "parent task");
        let mut children = Vec::new();
        for index in 0..count {
            let child =
                graph.create_child(&root.id, "worker", &format!("child {index}")).expect("child");
            children.push(child);
        }
        (root.id.clone(), children)
    }

    fn completed(summary: &str, subagent_id: &SubagentId) -> SubagentResult {
        completed_for_role(summary, subagent_id, "summarizer")
    }

    fn completed_for_role(summary: &str, subagent_id: &SubagentId, role: &str) -> SubagentResult {
        let output = serde_json::json!({
            "schema_version": 1,
            "summary": summary,
            "findings": [],
            "citations": {"files": [], "tools": []},
            "unresolved": [],
            "recommended_actions": []
        })
        .to_string();
        SubagentResult {
            subagent_id: subagent_id.clone(),
            handoff: SubagentHandoff::from_completed_output(
                role,
                subagent_id.as_str(),
                &output,
                crate::subagent_verifier::SubagentEvidence::default(),
            ),
            produced_memory_items: Vec::new(),
            tool_call_count: 1,
            error_summary: None,
        }
    }

    fn failed(subagent_id: &SubagentId, reason: &str) -> SubagentResult {
        SubagentResult {
            subagent_id: subagent_id.clone(),
            handoff: SubagentHandoff::terminal(
                "worker",
                subagent_id.as_str(),
                SubagentStatus::Failed,
            ),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: Some(reason.into()),
        }
    }

    #[test]
    fn ready_children_and_plan_requests_are_stable_and_independent() {
        let (tasks, coordinator) = harness();
        let (root, children) = root_with_children(&tasks, 3);
        // The third child depends on the first, so it is not ready yet.
        tasks
            .lock()
            .expect("graph")
            .add_dependency(&children[2].id, &children[0].id)
            .expect("dependency");

        let ready = coordinator.ready_children(&root);
        assert_eq!(ready.len(), 2);
        assert_eq!(ready[0].id, children[0].id);
        assert_eq!(ready[1].id, children[1].id);

        let role = crate::subagent_roles::BuiltinSubagentRole::Summarizer.role();
        let requests = coordinator.plan_requests(&root, role.clone(), None);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].child_task_id, children[0].id);
        assert_eq!(requests[1].child_task_id, children[1].id);
        assert_eq!(requests[0].scoped_prompt, "child 0");
        assert_eq!(requests[0].parent_task_id, root);
        assert_eq!(requests[0].role, role);
        assert!(requests[0].context_snapshot.is_empty());
        assert!(requests[0].write_scope.is_empty());
    }

    #[tokio::test]
    async fn parallel_fanout_bounds_concurrency_and_merges_in_order() {
        let (tasks, coordinator) = harness();
        let (root, children) = root_with_children(&tasks, 5);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );

        let concurrency = Arc::new(Mutex::new((0usize, 0usize)));
        let spawn = {
            let concurrency = concurrency.clone();
            move |request: SubagentRequest| {
                let concurrency = concurrency.clone();
                async move {
                    {
                        let mut state = concurrency.lock().expect("probe");
                        state.0 += 1;
                        state.1 = state.1.max(state.0);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    concurrency.lock().expect("probe").0 -= 1;
                    Ok(completed(
                        &format!("done {}", request.child_task_id),
                        &SubagentId::new(request.child_task_id.as_str()),
                    ))
                }
            }
        };
        let results = coordinator.run(requests.clone(), spawn).await;

        assert_eq!(results.len(), 5);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result.subagent_id.as_str(), requests[index].child_task_id.as_str());
            assert_eq!(result.handoff.status, SubagentStatus::Completed);
            assert!(result.handoff.summary.contains("done"));
        }
        assert_eq!(concurrency.lock().expect("probe").1, 2, "max parallel respected");

        let mut transcript = Vec::<ModelMessage>::new();
        coordinator.merge_summaries(&mut transcript, &results);
        assert_eq!(transcript.len(), 5, "completed summaries merged in order");
        for (index, message) in transcript.iter().enumerate() {
            assert_eq!(message.role, ModelRole::Subagent);
            assert!(
                message.text_content().contains(&results[index].handoff.summary),
                "merge order matches result order"
            );
        }

        let graph = tasks.lock().expect("graph");
        for child in &children {
            assert_eq!(graph.get(&child.id).expect("child").status, TaskStatus::Completed);
            assert!(
                graph
                    .get(&child.id)
                    .expect("child")
                    .result_summary
                    .as_ref()
                    .expect("summary")
                    .contains("done")
            );
        }
    }

    #[tokio::test]
    async fn child_failure_blocks_the_parent_task() {
        let (tasks, coordinator) = harness();
        let (root, children) = root_with_children(&tasks, 3);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );

        let spawn = {
            let broken = children[1].id.clone();
            move |request: SubagentRequest| {
                let broken = broken.clone();
                async move {
                    if request.child_task_id == broken {
                        Ok(failed(&SubagentId::new(request.child_task_id.as_str()), "child broke"))
                    } else {
                        Ok(completed(
                            &format!("done {}", request.child_task_id),
                            &SubagentId::new(request.child_task_id.as_str()),
                        ))
                    }
                }
            }
        };
        let results = coordinator.run(requests, spawn).await;
        let blocked = coordinator.mark_parent_blocked(&root, &results).expect("marks");
        assert!(blocked, "required child failure blocks the parent");

        let graph = tasks.lock().expect("graph");
        assert_eq!(graph.get(&root).expect("root").status, TaskStatus::Blocked);
        assert_eq!(graph.get(&children[0].id).expect("child").status, TaskStatus::Completed);
        assert_eq!(graph.get(&children[1].id).expect("child").status, TaskStatus::Failed);
        assert_eq!(graph.get(&children[2].id).expect("child").status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn all_children_completing_never_blocks_the_parent() {
        let (tasks, coordinator) = harness();
        let (root, _children) = root_with_children(&tasks, 2);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );
        let spawn = move |request: SubagentRequest| async move {
            Ok(completed(
                &format!("done {}", request.child_task_id),
                &SubagentId::new(request.child_task_id.as_str()),
            ))
        };
        let results = coordinator.run(requests, spawn).await;
        assert!(!coordinator.mark_parent_blocked(&root, &results).expect("not blocked"));
        assert_eq!(
            tasks.lock().expect("graph").get(&root).expect("root").status,
            TaskStatus::Running
        );
    }

    #[tokio::test]
    async fn spawn_errors_become_deterministic_failed_results() {
        let (tasks, coordinator) = harness();
        let (root, children) = root_with_children(&tasks, 2);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );
        let spawn = {
            let broken = children[1].id.clone();
            move |request: SubagentRequest| {
                let broken = broken.clone();
                async move {
                    if request.child_task_id == broken {
                        Err(OrchestratorError::SubagentFailure("spawn exploded".into()))
                    } else {
                        Ok(completed(
                            &format!("done {}", request.child_task_id),
                            &SubagentId::new(request.child_task_id.as_str()),
                        ))
                    }
                }
            }
        };
        let results = coordinator.run(requests, spawn).await;
        assert_eq!(results[1].handoff.status, SubagentStatus::Failed);
        assert!(results[1].error_summary.as_ref().expect("reason").contains("spawn exploded"));
        assert_eq!(
            tasks.lock().expect("graph").get(&children[1].id).expect("child").status,
            TaskStatus::Failed
        );
    }

    #[tokio::test]
    async fn injected_result_identity_role_or_format_mismatch_fails_closed() {
        let (tasks, coordinator) = harness();
        let (root, children) = root_with_children(&tasks, 3);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );
        let first = children[0].id.clone();
        let second = children[1].id.clone();
        let spawn = move |request: SubagentRequest| {
            let first = first.clone();
            let second = second.clone();
            async move {
                if request.child_task_id == first {
                    Ok(completed("forged id", &SubagentId::new("task-999")))
                } else if request.child_task_id == second {
                    Ok(completed_for_role(
                        "forged role",
                        &SubagentId::new(request.child_task_id.as_str()),
                        "implementer",
                    ))
                } else {
                    Ok(SubagentResult {
                        subagent_id: SubagentId::new(request.child_task_id.as_str()),
                        handoff: SubagentHandoff::terminal(
                            &request.role.name,
                            request.child_task_id.as_str(),
                            SubagentStatus::Completed,
                        ),
                        produced_memory_items: Vec::new(),
                        tool_call_count: 0,
                        error_summary: None,
                    })
                }
            }
        };

        let results = coordinator.run(requests, spawn).await;
        assert!(results.iter().all(|result| result.handoff.status == SubagentStatus::Failed));
        assert!(
            results[0].error_summary.as_deref().is_some_and(|error| error.contains("result id"))
        );
        assert!(results[1].error_summary.as_deref().is_some_and(|error| error.contains("role")));
        assert!(
            results[2]
                .error_summary
                .as_deref()
                .is_some_and(|error| error.contains("backend-terminal"))
        );
        assert!(children.iter().all(|child| {
            tasks.lock().expect("graph").get(&child.id).expect("child").status == TaskStatus::Failed
        }));
    }

    #[tokio::test]
    async fn overlapping_write_scopes_deny_spawn_and_locks_are_released() {
        let tasks = Arc::new(Mutex::new(TaskGraph::new()));
        let detector = Arc::new(Mutex::new(WriteScopeConflictDetector::new()));
        let coordinator =
            FanOutFanInCoordinator::new(2, tasks.clone()).with_write_conflicts(detector.clone());
        let (root, children) = root_with_children(&tasks, 2);

        let requests = coordinator
            .plan_requests(
                &root,
                crate::subagent_roles::BuiltinSubagentRole::Implementer.role(),
                None,
            )
            .into_iter()
            .map(|mut request| {
                request.write_scope = vec![PathBuf::from("/work/a.rs")];
                request
            })
            .collect::<Vec<_>>();

        let spawns = Arc::new(Mutex::new(0usize));
        let spawn = {
            let spawns = spawns.clone();
            move |request: SubagentRequest| {
                let spawns = spawns.clone();
                async move {
                    *spawns.lock().expect("spawns") += 1;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Ok(completed_for_role(
                        &format!("done {}", request.child_task_id),
                        &SubagentId::new(request.child_task_id.as_str()),
                        &request.role.name,
                    ))
                }
            }
        };
        let results = coordinator.run(requests, spawn).await;

        assert_eq!(*spawns.lock().expect("spawns"), 1, "conflicted child never spawns");
        assert_eq!(results[0].handoff.status, SubagentStatus::Completed);
        assert_eq!(results[1].handoff.status, SubagentStatus::Failed);
        assert!(results[1].error_summary.as_ref().expect("reason").contains("write scope overlap"));
        assert!(
            detector.lock().expect("detector").is_empty(),
            "locks released after the fan-out completes"
        );
        assert_eq!(
            tasks.lock().expect("graph").get(&children[1].id).expect("child").status,
            TaskStatus::Failed
        );
    }

    #[tokio::test]
    async fn disjoint_write_scopes_run_concurrently() {
        let tasks = Arc::new(Mutex::new(TaskGraph::new()));
        let detector = Arc::new(Mutex::new(WriteScopeConflictDetector::new()));
        let coordinator =
            FanOutFanInCoordinator::new(2, tasks.clone()).with_write_conflicts(detector.clone());
        let (root, _children) = root_with_children(&tasks, 2);

        let requests = coordinator
            .plan_requests(
                &root,
                crate::subagent_roles::BuiltinSubagentRole::Implementer.role(),
                None,
            )
            .into_iter()
            .enumerate()
            .map(|(index, mut request)| {
                request.write_scope = vec![PathBuf::from(format!("/work/{index}.rs"))];
                request
            })
            .collect::<Vec<_>>();

        let active = Arc::new(Mutex::new((0usize, 0usize)));
        let spawn = {
            let active = active.clone();
            move |request: SubagentRequest| {
                let active = active.clone();
                async move {
                    {
                        let mut state = active.lock().expect("probe");
                        state.0 += 1;
                        state.1 = state.1.max(state.0);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    active.lock().expect("probe").0 -= 1;
                    Ok(completed_for_role(
                        &format!("done {}", request.child_task_id),
                        &SubagentId::new(request.child_task_id.as_str()),
                        &request.role.name,
                    ))
                }
            }
        };
        let results = coordinator.run(requests, spawn).await;
        assert_eq!(active.lock().expect("probe").1, 2, "disjoint scopes run concurrently");
        assert!(results.iter().all(|result| result.handoff.status == SubagentStatus::Completed));
        assert!(detector.lock().expect("detector").is_empty());
    }

    #[tokio::test]
    async fn empty_requests_return_immediately() {
        let (tasks, coordinator) = harness();
        let (root, _) = root_with_children(&tasks, 0);
        let requests = coordinator.plan_requests(
            &root,
            crate::subagent_roles::BuiltinSubagentRole::Summarizer.role(),
            None,
        );
        let spawn = move |_request: SubagentRequest| async move {
            Ok(completed("never", &SubagentId::new("task-99")))
        };
        assert!(coordinator.run(requests, spawn).await.is_empty());
    }

    #[test]
    fn transcript_merge_skips_failed_and_empty_summaries() {
        let (_tasks, coordinator) = harness();
        let mut transcript = Vec::<ModelMessage>::new();
        let results = vec![
            completed("one", &SubagentId::new("task-2")),
            failed(&SubagentId::new("task-3"), "broke"),
            SubagentResult {
                subagent_id: SubagentId::new("task-4"),
                handoff: SubagentHandoff::terminal("worker", "task-4", SubagentStatus::Cancelled),
                produced_memory_items: Vec::new(),
                tool_call_count: 0,
                error_summary: Some("cancelled".into()),
            },
            completed("", &SubagentId::new("task-5")),
        ];
        coordinator.merge_summaries(&mut transcript, &results);
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].role, ModelRole::Subagent);
        let handoff: SubagentHandoff =
            serde_json::from_str(&transcript[0].text_content()).expect("structured handoff");
        assert_eq!(handoff.summary, "one");
    }
}
