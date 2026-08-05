//! Deterministic stuck detection for the agent loop.
//!
//! [`StuckDetector`] watches one loop run for four kinds of spinning:
//! repeated identical model responses, repeated identical tool calls,
//! consecutive failed edit (write-class) attempts, and iterations that make
//! no task-graph progress.  Each observation returns a [`StuckReason`] the
//! moment its configured threshold is reached, so the loop can stop with a
//! deterministic `stuck` stop reason instead of running to the iteration cap.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ModelResponse;
use crate::tasks::TaskGraph;
use crate::tools::{SideEffectClass, ToolIntent, ToolResult};

/// Default maximum consecutive identical model responses before stopping.
pub const DEFAULT_MAX_REPEATED_MODEL_RESPONSES: usize = 4;
/// Default maximum consecutive identical tool calls before stopping.
pub const DEFAULT_MAX_REPEATED_TOOL_CALLS: usize = 4;
/// Default maximum consecutive failed write-class attempts before stopping.
pub const DEFAULT_MAX_FAILED_EDIT_ATTEMPTS: usize = 3;
/// Default maximum no-progress iterations (no tools, unchanged task graph)
/// before stopping.
pub const DEFAULT_MAX_NO_PROGRESS_ITERATIONS: usize = 4;

/// Thresholds for stuck detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckConfig {
    /// Consecutive identical model responses that trigger a stop.
    pub max_repeated_model_responses: usize,
    /// Consecutive identical tool calls (same name and arguments) that
    /// trigger a stop.
    pub max_repeated_tool_calls: usize,
    /// Consecutive failed write-class tool calls that trigger a stop.
    pub max_failed_edit_attempts: usize,
    /// Consecutive iterations with no tool execution and an unchanged task
    /// graph that trigger a stop.
    pub max_no_progress_iterations: usize,
}

impl Default for StuckConfig {
    fn default() -> Self {
        Self {
            max_repeated_model_responses: DEFAULT_MAX_REPEATED_MODEL_RESPONSES,
            max_repeated_tool_calls: DEFAULT_MAX_REPEATED_TOOL_CALLS,
            max_failed_edit_attempts: DEFAULT_MAX_FAILED_EDIT_ATTEMPTS,
            max_no_progress_iterations: DEFAULT_MAX_NO_PROGRESS_ITERATIONS,
        }
    }
}

/// Why a loop was judged stuck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StuckReason {
    /// The model returned the same response `count` times in a row.
    RepeatedModelResponse {
        /// Number of consecutive identical responses.
        count: usize,
    },
    /// The model issued the same tool call (name + arguments) `count` times.
    RepeatedToolCall {
        /// Tool name and serialized arguments fingerprint.
        signature: String,
        /// Number of consecutive identical calls.
        count: usize,
    },
    /// `count` consecutive write-class calls failed.
    RepeatedFailedEdit {
        /// The failing tool name.
        tool_name: String,
        /// Number of consecutive failed attempts.
        count: usize,
    },
    /// `iterations` consecutive loop iterations made no task-graph progress.
    NoTaskProgress {
        /// Number of consecutive no-progress iterations.
        iterations: usize,
    },
}

impl StuckReason {
    /// Stable machine-readable code, used in stop reasons.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RepeatedModelResponse { .. } => "repeated-model-response",
            Self::RepeatedToolCall { .. } => "repeated-tool-call",
            Self::RepeatedFailedEdit { .. } => "repeated-failed-edit",
            Self::NoTaskProgress { .. } => "no-task-progress",
        }
    }
}

impl fmt::Display for StuckReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepeatedModelResponse { count } => {
                write!(f, "model repeated an identical response {count} times")
            }
            Self::RepeatedToolCall { signature, count } => {
                write!(f, "tool call {signature} repeated {count} times")
            }
            Self::RepeatedFailedEdit { tool_name, count } => {
                write!(f, "write tool {tool_name} failed {count} consecutive times")
            }
            Self::NoTaskProgress { iterations } => {
                write!(f, "no task-graph progress for {iterations} iterations")
            }
        }
    }
}

/// Per-turn stuck detector.  Stateless across turns: construct one per loop
/// run so state never leaks between sessions.
#[derive(Debug, Clone)]
pub struct StuckDetector {
    config: StuckConfig,
    last_model_fingerprint: Option<String>,
    repeated_model_count: usize,
    last_tool_fingerprint: Option<String>,
    repeated_tool_count: usize,
    failed_edit_count: usize,
    last_graph_signature: Option<String>,
    no_progress_iterations: usize,
    tools_this_iteration: bool,
}

impl StuckDetector {
    /// Creates a detector with the given thresholds.
    #[must_use]
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            last_model_fingerprint: None,
            repeated_model_count: 0,
            last_tool_fingerprint: None,
            repeated_tool_count: 0,
            failed_edit_count: 0,
            last_graph_signature: None,
            no_progress_iterations: 0,
            tools_this_iteration: false,
        }
    }

    /// Observes one model response; returns a reason when the repeated-model
    /// threshold is reached.
    pub fn observe_model_response(&mut self, response: &ModelResponse) -> Option<StuckReason> {
        let fingerprint = model_fingerprint(response);
        if self.last_model_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            self.repeated_model_count += 1;
        } else {
            self.last_model_fingerprint = Some(fingerprint);
            self.repeated_model_count = 1;
        }
        (self.repeated_model_count >= self.config.max_repeated_model_responses)
            .then_some(StuckReason::RepeatedModelResponse { count: self.repeated_model_count })
    }

    /// Observes one executed tool call; returns a reason when the repeated
    /// tool-call or repeated failed-edit threshold is reached.  `class` is the
    /// registered side-effect class (`None` for unknown tools).
    pub fn observe_tool_call(
        &mut self,
        intent: &ToolIntent,
        class: Option<SideEffectClass>,
        result: &ToolResult,
    ) -> Option<StuckReason> {
        self.tools_this_iteration = true;
        let fingerprint = tool_fingerprint(intent);
        if self.last_tool_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            self.repeated_tool_count += 1;
        } else {
            self.last_tool_fingerprint = Some(fingerprint.clone());
            self.repeated_tool_count = 1;
        }
        if class == Some(SideEffectClass::Write) {
            if result.success {
                self.failed_edit_count = 0;
            } else {
                self.failed_edit_count += 1;
            }
        }
        if self.repeated_tool_count >= self.config.max_repeated_tool_calls {
            return Some(StuckReason::RepeatedToolCall {
                signature: fingerprint,
                count: self.repeated_tool_count,
            });
        }
        if class == Some(SideEffectClass::Write)
            && !result.success
            && self.failed_edit_count >= self.config.max_failed_edit_attempts
        {
            return Some(StuckReason::RepeatedFailedEdit {
                tool_name: intent.name.clone(),
                count: self.failed_edit_count,
            });
        }
        None
    }

    /// Marks the end of one loop iteration.  Returns a reason when the
    /// no-progress threshold is reached: no tool executed during the
    /// iteration and (when a graph handle is available) the task graph
    /// signature is unchanged from the previous iteration.  With no graph
    /// handle, no-tool iterations count as no-progress.
    pub fn observe_iteration(&mut self, graph: Option<&TaskGraph>) -> Option<StuckReason> {
        let unchanged = match graph {
            Some(graph) => {
                let signature = graph_signature(graph);
                let same = self.last_graph_signature.as_deref() == Some(signature.as_str());
                self.last_graph_signature = Some(signature);
                same
            }
            None => true,
        };
        if !self.tools_this_iteration && unchanged {
            self.no_progress_iterations += 1;
        } else {
            self.no_progress_iterations = 0;
        }
        self.tools_this_iteration = false;
        (self.no_progress_iterations >= self.config.max_no_progress_iterations)
            .then_some(StuckReason::NoTaskProgress { iterations: self.no_progress_iterations })
    }
}

/// Deterministic fingerprint of one model response (text, reasoning, tool and
/// subagent intents, completion flag).  Usage counters are included so budget
/// recording stays observable but identical outputs still compare equal.
fn model_fingerprint(response: &ModelResponse) -> String {
    serde_json::to_string(response).expect("model response serializes")
}

/// Deterministic fingerprint of one tool intent: name plus serialized
/// arguments.
fn tool_fingerprint(intent: &ToolIntent) -> String {
    format!("{} {}", intent.name, serde_json::to_string(&intent.arguments).unwrap_or_default())
}

/// Deterministic signature of a task graph: `(id, status)` pairs in stable
/// order.
fn graph_signature(graph: &TaskGraph) -> String {
    let pairs: Vec<(String, Value)> = graph
        .list()
        .iter()
        .map(|task| {
            (task.id.as_str().to_string(), serde_json::to_value(task.status).expect("status"))
        })
        .collect();
    serde_json::to_string(&pairs).expect("graph signature serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskStatus;
    use crate::tools::{ToolErrorKind, ToolResult};
    use serde_json::json;

    fn response(text: &str) -> ModelResponse {
        ModelResponse::new().text(text)
    }

    fn tool_intent(name: &str, path: &str) -> ToolIntent {
        ToolIntent::new("tc-1", name, json!({ "path": path }))
    }

    fn write_result(success: bool) -> ToolResult {
        if success {
            ToolResult::success("written")
        } else {
            ToolResult::failure(ToolErrorKind::Backend, "edit failed")
        }
    }

    #[test]
    fn stuck_detection_repeated_model_responses_stop() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        for _ in 0..3 {
            assert_eq!(detector.observe_model_response(&response("still working")), None);
        }
        let reason = detector.observe_model_response(&response("still working")).expect("stuck");
        assert_eq!(reason, StuckReason::RepeatedModelResponse { count: 4 });
        assert_eq!(reason.code(), "repeated-model-response");
        // A different response resets the streak.
        assert_eq!(detector.observe_model_response(&response("different")), None);
    }

    #[test]
    fn stuck_detection_repeated_tool_calls_stop() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        let intent = tool_intent("read_file", "/tmp/x");
        for _ in 0..3 {
            assert_eq!(
                detector.observe_tool_call(
                    &intent,
                    Some(SideEffectClass::Read),
                    &write_result(true)
                ),
                None
            );
        }
        let reason = detector
            .observe_tool_call(&intent, Some(SideEffectClass::Read), &write_result(true))
            .expect("stuck");
        assert!(matches!(reason, StuckReason::RepeatedToolCall { count: 4, .. }));
        assert_eq!(reason.code(), "repeated-tool-call");
        // A different tool call resets the streak.
        assert_eq!(
            detector.observe_tool_call(
                &tool_intent("read_file", "/tmp/y"),
                Some(SideEffectClass::Read),
                &write_result(true)
            ),
            None
        );
    }

    #[test]
    fn stuck_detection_repeated_failed_edits_stop() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        let intent = tool_intent("write_file", "/tmp/out.rs");
        for _ in 0..2 {
            assert_eq!(
                detector.observe_tool_call(
                    &intent,
                    Some(SideEffectClass::Write),
                    &write_result(false)
                ),
                None
            );
        }
        let reason = detector
            .observe_tool_call(&intent, Some(SideEffectClass::Write), &write_result(false))
            .expect("stuck");
        assert_eq!(
            reason,
            StuckReason::RepeatedFailedEdit { tool_name: "write_file".into(), count: 3 }
        );
        assert_eq!(reason.code(), "repeated-failed-edit");
        // A successful write resets the failed-edit streak.
        let mut detector = StuckDetector::new(StuckConfig::default());
        for _ in 0..2 {
            detector.observe_tool_call(&intent, Some(SideEffectClass::Write), &write_result(false));
        }
        assert_eq!(
            detector.observe_tool_call(&intent, Some(SideEffectClass::Write), &write_result(true)),
            None,
            "successful edit breaks the failed streak"
        );
    }

    #[test]
    fn stuck_detection_non_write_failures_do_not_count_as_edits() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        for index in 0..5 {
            let intent = tool_intent("read_file", &format!("/tmp/x{index}"));
            let result = detector.observe_tool_call(
                &intent,
                Some(SideEffectClass::Read),
                &write_result(false),
            );
            assert!(
                !matches!(result, Some(StuckReason::RepeatedFailedEdit { .. })),
                "read failures are not failed edits: {result:?}"
            );
        }
    }

    #[test]
    fn stuck_detection_no_progress_iterations_stop() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        let mut graph = TaskGraph::new();
        graph.create_root("root", "work");
        // The first call establishes the baseline signature.
        for _ in 0..4 {
            assert_eq!(detector.observe_iteration(Some(&graph)), None);
        }
        let reason = detector.observe_iteration(Some(&graph)).expect("stuck");
        assert_eq!(reason, StuckReason::NoTaskProgress { iterations: 4 });
        assert_eq!(reason.code(), "no-task-progress");
    }

    #[test]
    fn stuck_detection_tool_use_resets_no_progress() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        let mut graph = TaskGraph::new();
        graph.create_root("root", "work");
        detector.observe_iteration(Some(&graph));
        detector.observe_iteration(Some(&graph));
        let intent = tool_intent("read_file", "/tmp/x");
        detector.observe_tool_call(&intent, Some(SideEffectClass::Read), &write_result(true));
        assert_eq!(detector.observe_iteration(Some(&graph)), None, "tool use resets the streak");
    }

    #[test]
    fn stuck_detection_graph_change_resets_no_progress() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        let mut graph = TaskGraph::new();
        let root = graph.create_root("root", "work");
        detector.observe_iteration(Some(&graph));
        detector.observe_iteration(Some(&graph));
        graph.transition(&root.id, TaskStatus::Blocked).expect("blocks");
        assert_eq!(detector.observe_iteration(Some(&graph)), None, "graph change resets");
    }

    #[test]
    fn stuck_detection_works_without_graph_handle() {
        let mut detector = StuckDetector::new(StuckConfig::default());
        for _ in 0..3 {
            assert_eq!(detector.observe_iteration(None), None);
        }
        assert_eq!(
            detector.observe_iteration(None),
            Some(StuckReason::NoTaskProgress { iterations: 4 })
        );
    }

    #[test]
    fn stuck_reasons_serialize_deterministically() {
        for reason in [
            StuckReason::RepeatedModelResponse { count: 4 },
            StuckReason::RepeatedToolCall {
                signature: "read_file {\"path\":\"/tmp/x\"}".into(),
                count: 4,
            },
            StuckReason::RepeatedFailedEdit { tool_name: "write_file".into(), count: 3 },
            StuckReason::NoTaskProgress { iterations: 4 },
        ] {
            let json = serde_json::to_string(&reason).expect("serializes");
            let restored: StuckReason = serde_json::from_str(&json).expect("parses");
            assert_eq!(restored, reason);
            assert!(!reason.to_string().is_empty());
        }
    }
}
