//! Deterministic fakes for tests (feature `test-utils`).
//!
//! [`FakeModel`] replays a scripted sequence of [`ModelResponse`] values and
//! records every [`ModelRequest`]; when the script is exhausted it keeps
//! returning empty responses, which the loop stops on deterministically
//! (two consecutive empties end the turn).  [`FakeTool`] returns a canned
//! [`ToolResult`] and records every invocation's arguments.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::ClientBridge;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::model::{ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse};
use crate::tools::{
    ServerTool, ToolCallContext, ToolDefinition, ToolFuture, ToolIntent, ToolResult,
};

/// Scripted model adapter for deterministic tests.
#[derive(Clone)]
pub struct FakeModel {
    script: Arc<Mutex<VecDeque<ModelResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl FakeModel {
    /// Creates a model replaying the given responses in order.
    #[must_use]
    pub fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            script: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request the model received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("fake model requests poisoned").clone()
    }

    /// Number of completions invoked.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.requests.lock().expect("fake model requests poisoned").len()
    }
}

impl ModelAdapter for FakeModel {
    fn complete(
        &self,
        request: ModelRequest,
        _cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let requests = self.requests.clone();
        let script = self.script.clone();
        Box::pin(async move {
            requests.lock().expect("fake model requests poisoned").push(request);
            let response =
                script.lock().expect("fake model script poisoned").pop_front().unwrap_or_default();
            Ok(response)
        })
    }
}

/// Tool returning a canned result for deterministic tests.
#[derive(Clone)]
pub struct FakeTool {
    definition: ToolDefinition,
    result: ToolResult,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl FakeTool {
    /// Creates a tool with the given definition and canned result.
    #[must_use]
    pub fn new(definition: ToolDefinition, result: ToolResult) -> Self {
        Self { definition, result, calls: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Number of executions.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("fake tool calls poisoned").len()
    }

    /// Arguments of every execution, in order.
    #[must_use]
    pub fn call_arguments(&self) -> Vec<Value> {
        self.calls.lock().expect("fake tool calls poisoned").clone()
    }
}

impl ServerTool for FakeTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        arguments: Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let calls = self.calls.clone();
        let result = self.result.clone();
        Box::pin(async move {
            calls.lock().expect("fake tool calls poisoned").push(arguments);
            result
        })
    }
}

/// Script fixture: one completed assistant answer.
#[must_use]
pub fn simple_answer_script() -> Vec<ModelResponse> {
    vec![ModelResponse::new().text("hello world").completed()]
}

/// Script fixture: one `read_file` tool intent, then a completed answer.
#[must_use]
pub fn tool_then_answer_script() -> Vec<ModelResponse> {
    vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "read_file",
            json!({ "path": "/tmp/x" }),
        )]),
        ModelResponse::new().text("read it").completed(),
    ]
}

/// Script fixture: one `delegate_task` tool intent, then a completed answer.
#[must_use]
pub fn delegate_then_answer_script() -> Vec<ModelResponse> {
    vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new(
            "tc-1",
            "delegate_task",
            json!({ "prompt": "do the work" }),
        )]),
        ModelResponse::new().text("delegated").completed(),
    ]
}

/// Script fixture: an unbounded stream of `read_file` tool intents; the loop
/// must terminate it via the iteration budget.
#[must_use]
pub fn endless_tool_loop_script(calls: usize) -> Vec<ModelResponse> {
    (0..calls)
        .map(|index| {
            ModelResponse::new().tool_intents(vec![ToolIntent::new(
                format!("tc-{index}"),
                "read_file",
                json!({ "path": "/tmp/x" }),
            )])
        })
        .collect()
}

/// Deterministic fake semantic index for tests; returns its canned hits,
/// optionally narrowed to a query key when one matches.
#[derive(Debug, Clone)]
pub struct FakeSemanticMemoryAdapter {
    hits: Vec<crate::semantic_memory::SemanticMemoryHit>,
}

impl FakeSemanticMemoryAdapter {
    /// Creates an adapter returning the given hits in order.
    #[must_use]
    pub fn new(hits: Vec<crate::semantic_memory::SemanticMemoryHit>) -> Self {
        Self { hits }
    }
}

impl crate::semantic_memory::SemanticMemoryAdapter for FakeSemanticMemoryAdapter {
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::semantic_memory::SemanticMemoryHit>, crate::error::OrchestratorError>
    {
        let mut hits = if self.hits.iter().any(|hit| hit.key == query) {
            self.hits.iter().filter(|hit| hit.key == query).cloned().collect::<Vec<_>>()
        } else {
            self.hits.clone()
        };
        hits.truncate(limit);
        Ok(hits)
    }
}
