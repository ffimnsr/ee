//! Cancellation, budget, stuck-detection, and overflow coverage.

use super::*;

#[tokio::test]
async fn cancellation_stops_before_next_model_call() {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let flipping_tool = CancelFlippingTool {
        definition: ToolDefinition::new("flip", "flips the cancel signal")
            .side_effect_class(SideEffectClass::Read),
        cancel_tx,
    };
    let model = FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![ToolIntent::new("tc-1", "flip", json!({}))]),
        ModelResponse::new().text("never reached").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        Arc::new(flipping_tool),
        events.clone(),
    );

    let error = engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await;
    assert!(matches!(error, Err(OrchestratorError::Cancellation)));
    assert_eq!(model.call_count(), 1, "no model call after cancellation");
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "cancelled".into() })
    );
}

#[tokio::test]
async fn cancellation_before_model_call_prevents_adapter_invocation() {
    let model = FakeModel::new(vec![ModelResponse::new().text("never reached").completed()]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        echo_tool(),
        events.clone(),
    );

    let (cancel_tx, cancel_rx) = watch::channel(true);
    cancel_tx.send(true).expect("cancel receiver alive");
    let error = engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await;
    assert!(matches!(error, Err(OrchestratorError::Cancellation)));
    assert_eq!(model.call_count(), 0, "adapter must not be invoked");
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "cancelled".into() })
    );
}

#[tokio::test]
async fn cancellation_during_model_call_resolves_turn_cancellation() {
    let model = Arc::new(CancelAwaitingModel { calls: Arc::new(Mutex::new(0)) });
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let engine =
        engine_with(OrchestratorConfig::default(), model.clone(), echo_tool(), events.clone());

    let handle = tokio::spawn(async move {
        engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await
    });
    wait_until(|| *model.calls.lock().expect("calls poisoned") == 1).await;
    cancel_tx.send(true).expect("cancel receiver alive");

    let error = handle.await.expect("turn task completes");
    assert!(matches!(error, Err(OrchestratorError::Cancellation)));
    assert_eq!(*model.calls.lock().expect("calls poisoned"), 1, "no second call");
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "cancelled".into() })
    );
}

#[tokio::test]
async fn cancellation_during_tool_call_resolves_tool_cancellation() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let tool = CancelAwaitingTool {
        definition: ToolDefinition::new("block", "blocks until cancelled")
            .side_effect_class(SideEffectClass::Read),
        started: started_tx,
    };
    let model = FakeModel::new(vec![ModelResponse::new().tool_intents(vec![ToolIntent::new(
        "tc-1",
        "block",
        json!({}),
    )])]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        Arc::new(tool),
        events.clone(),
    );

    let handle = tokio::spawn(async move {
        engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await
    });
    started_rx.recv().await.expect("tool started");
    cancel_tx.send(true).expect("cancel receiver alive");

    let error = handle.await.expect("turn task completes");
    assert!(matches!(error, Err(OrchestratorError::Cancellation)));
    assert_eq!(model.call_count(), 1, "loop stopped before the next model call");
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "cancelled".into() })
    );
}

#[tokio::test]
async fn max_model_calls_stops_loop_before_next_adapter_call() {
    let model = FakeModel::new(vec![
        ModelResponse::new().text("one"),
        ModelResponse::new().text("two"),
        ModelResponse::new().text("three"),
    ]);
    let config = OrchestratorConfig { max_model_calls: Some(2), ..OrchestratorConfig::default() };
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(config, Arc::new(model.clone()), echo_tool(), events.clone());

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await;
    assert!(matches!(error, Err(OrchestratorError::BudgetExceeded(_))));
    assert_eq!(model.call_count(), 2, "third call was budget-denied before invocation");
    // The snapshot handed to the model carries the model-call cap.
    assert_eq!(model.requests()[0].budget.model_calls_max, Some(2));
    assert_eq!(model.requests()[1].budget.model_calls_used, 2);
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "budget_exceeded".into() })
    );
}

#[tokio::test]
async fn output_byte_budget_stops_loop_on_violating_response() {
    let model = FakeModel::new(vec![ModelResponse::new().text("x".repeat(200)).completed()]);
    let config =
        OrchestratorConfig { max_output_bytes: Some(100), ..OrchestratorConfig::default() };
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(config, Arc::new(model.clone()), echo_tool(), events.clone());

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await;
    assert!(matches!(
        error,
        Err(OrchestratorError::BudgetExceeded(ref reason)) if reason.contains("max output bytes")
    ));
    assert_eq!(model.call_count(), 1, "no further adapter calls after the violation");
}

/// Bounded wait that pumps the runtime until `condition` holds; used to
/// observe in-flight work without wall-clock sleeps.
async fn wait_until(condition: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition never satisfied");
}

/// Model that blocks until the cancellation watch flips, then reports
/// cancellation; proves cancellation reaches an in-flight adapter call.
struct CancelAwaitingModel {
    calls: Arc<Mutex<usize>>,
}

impl ModelAdapter for CancelAwaitingModel {
    fn complete(
        &self,
        _request: ModelRequest,
        mut cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            *calls.lock().expect("calls poisoned") += 1;
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let _ = cancel.changed().await;
            Err(ModelError::Cancelled)
        })
    }
}

/// Tool that blocks until the cancellation watch flips; proves the token
/// reaches a running tool and the executor resolves to cancellation.
struct CancelAwaitingTool {
    definition: ToolDefinition,
    started: mpsc::UnboundedSender<()>,
}

impl ServerTool for CancelAwaitingTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _arguments: serde_json::Value,
        _client: ClientBridge,
        mut cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let started = self.started.clone();
        Box::pin(async move {
            let _ = started.send(());
            if *cancel.borrow() {
                return ToolResult::success("already cancelled");
            }
            let _ = cancel.changed().await;
            ToolResult::success("released after cancellation")
        })
    }
}

#[tokio::test]
async fn repeated_empty_responses_stop_deterministically() {
    let model = FakeModel::new(vec![ModelResponse::new(), ModelResponse::new()]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        echo_tool(),
        events.clone(),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = engine.run(prompt("hello world"), sink, client, cancel_rx, task(), None).await;
    assert_eq!(result.expect("turn ends").stop_reason, StopReason::EndTurn);
    assert_eq!(model.call_count(), 2, "exactly two empties, never more");
    assert!(
        events
            .events()
            .contains(&OrchestratorEvent::TurnStopped { stop_reason: "end_turn".into() })
    );
}

#[tokio::test]
async fn length_finish_continues_with_a_bounded_notice() {
    let model = FakeModel::new(vec![
        ModelResponse::new().text("part one").with_finish_reason(Some(String::from("length"))),
        ModelResponse::new().text("part two").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        echo_tool(),
        EventRecorder::new(),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn ends");

    assert_eq!(model.call_count(), 2, "one continuation then stop");
    let second = &model.requests()[1].transcript;
    let last = second.last().expect("continuation notice");
    assert_eq!(last.role, ModelRole::User);
    match &last.content[0] {
        ModelContent::Text(text) => {
            assert!(text.contains("output token limit"), "notice text: {text}");
            assert!(text.contains("Break the remaining work"), "notice action: {text}");
        }
        other => panic!("expected text notice, got {other:?}"),
    }
}

#[tokio::test]
async fn length_continuations_stop_at_the_cap() {
    let length =
        || ModelResponse::new().text("partial").with_finish_reason(Some(String::from("length")));
    let model = FakeModel::new(vec![length(), length(), length()]);
    let (sink, client, _rx) = plumbing();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        echo_tool(),
        EventRecorder::new(),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn ends");

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(
        model.call_count(),
        MAX_OUTPUT_CONTINUATIONS + 1,
        "initial call plus one continuation per cap slot"
    );
}

#[tokio::test]
async fn memory_facts_seed_transcript_as_system_message() {
    let model = FakeModel::new(vec![ModelResponse::new().text("ok").completed()]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine =
        engine_with(OrchestratorConfig::default(), Arc::new(model.clone()), echo_tool(), events);

    let mut store = MemoryStore::new(1024);
    store.insert(MemoryItem::new("cwd", "/work")).expect("inserts");
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), store.compact_context())
        .await
        .expect("turn succeeds");

    let transcript = &model.requests()[0].transcript;
    assert_eq!(transcript.len(), 2, "system seed plus user message");
    assert_eq!(transcript[0].role, ModelRole::System);
    match &transcript[0].content[0] {
        ModelContent::Text(text) => {
            assert!(text.contains("Memory facts"), "seed header present");
            assert!(text.contains("cwd: /work"), "fact rendered");
        }
        other => panic!("expected text content, got {other:?}"),
    }
    assert_eq!(transcript[1].role, ModelRole::User);
    assert_eq!(transcript[1].content[0], ModelContent::Text("hello world".into()));
}

#[tokio::test]
async fn no_system_seed_without_memory() {
    let model = FakeModel::new(vec![ModelResponse::new().text("ok").completed()]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine =
        engine_with(OrchestratorConfig::default(), Arc::new(model.clone()), echo_tool(), events);

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn succeeds");

    let transcript = &model.requests()[0].transcript;
    assert_eq!(transcript.len(), 1, "no memory means no system seed");
    assert_eq!(transcript[0].role, ModelRole::User);
}

/// Tool that flips the cancellation watch sender during execution, used
/// to prove the loop stops before the next model call.
struct CancelFlippingTool {
    definition: ToolDefinition,
    cancel_tx: watch::Sender<bool>,
}

impl ServerTool for CancelFlippingTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _arguments: serde_json::Value,
        _client: ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let cancel_tx = self.cancel_tx.clone();
        Box::pin(async move {
            cancel_tx.send(true).expect("cancel receiver alive");
            ToolResult::success("flipped")
        })
    }
}

// ── Stuck detection integration ───────────────────────────────────────

fn stuck_error(error: OrchestratorError) -> (bool, String) {
    match error {
        OrchestratorError::Stuck(reason) => (true, reason),
        other => (false, other.to_string()),
    }
}

#[tokio::test]
async fn stuck_detection_repeated_model_responses_stop_loop() {
    let config = OrchestratorConfig::default();
    let model = FakeModel::new(vec![
        ModelResponse::new().text("still working"),
        ModelResponse::new().text("still working"),
        ModelResponse::new().text("still working"),
        ModelResponse::new().text("still working"),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(config, Arc::new(model.clone()), echo_tool(), events.clone());

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello"), sink, client, cancel_rx, task(), None).await;
    let (stuck, reason) = stuck_error(error.expect_err("identical responses are stuck"));
    assert!(stuck, "expected a stuck error, got: {reason}");
    assert!(reason.contains("repeated"));
    assert_eq!(model.call_count(), 4);
    assert!(
        events.events().contains(&OrchestratorEvent::TurnStopped { stop_reason: "stuck".into() })
    );
}

#[tokio::test]
async fn stuck_detection_repeated_tool_calls_stop_loop() {
    // The model response changes every iteration, so only the tool call
    // repeats: the repeated-tool-call detector must stop the loop.
    let intent = || ToolIntent::new("tc-1", "echo", json!({ "text": "hi" }));
    let model = FakeModel::new(vec![
        ModelResponse::new().text("one").tool_intents(vec![intent()]),
        ModelResponse::new().text("two").tool_intents(vec![intent()]),
        ModelResponse::new().text("three").tool_intents(vec![intent()]),
        ModelResponse::new().text("four").tool_intents(vec![intent()]),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let tool = echo_tool();
    let engine =
        engine_with(OrchestratorConfig::default(), Arc::new(model.clone()), tool.clone(), events);

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello"), sink, client, cancel_rx, task(), None).await;
    let (stuck, reason) = stuck_error(error.expect_err("identical tool calls are stuck"));
    assert!(stuck, "expected a stuck error, got: {reason}");
    assert!(reason.contains("echo"));
    assert_eq!(tool.call_count(), 4, "the fourth identical call trips the detector");
}

#[tokio::test]
async fn stuck_detection_repeated_failed_edits_stop_loop() {
    let intent = || ToolIntent::new("tc-1", "write_file", json!({ "path": "/tmp/x" }));
    let model = FakeModel::new(vec![
        ModelResponse::new().text("one").tool_intents(vec![intent()]),
        ModelResponse::new().text("two").tool_intents(vec![intent()]),
        ModelResponse::new().text("three").tool_intents(vec![intent()]),
    ]);
    let tool = Arc::new(FakeTool::new(
        ToolDefinition::new("write_file", "writes a file")
            .side_effect_class(SideEffectClass::Write),
        ToolResult::failure(crate::tools::ToolErrorKind::Backend, "edit failed"),
    ));
    let tools = Arc::new(Mutex::new(ToolRegistry::new()));
    tools.lock().expect("registry poisoned").register(tool.clone()).expect("registers");
    let config = OrchestratorConfig::default();
    let budget = Arc::new(Mutex::new(BudgetTracker::new(&config)));
    let events = EventRecorder::new();
    let engine = LoopEngine::new(
        config,
        Arc::new(model.clone()),
        tools,
        budget,
        crate::policy::PolicyEngine::new(crate::policy::ToolPolicy {
            allow_write: true,
            ..crate::policy::ToolPolicy::default()
        }),
        events.clone(),
        LoopOptions::default(),
    );

    let (sink, client, _rx) = plumbing();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello"), sink, client, cancel_rx, task(), None).await;
    let (stuck, reason) = stuck_error(error.expect_err("failed edits are stuck"));
    assert!(stuck, "expected a stuck error, got: {reason}");
    assert!(reason.contains("write_file"));
    assert!(reason.contains("failed"));
    assert_eq!(tool.call_count(), 3, "three consecutive failed edits trip the detector");
}

#[tokio::test]
async fn stuck_detection_no_progress_stops_spinning_turn() {
    // Model returns new text every iteration but never completes and
    // never calls tools: the no-progress detector stops the turn before
    // the iteration budget.
    let config =
        OrchestratorConfig { max_loop_iterations: Some(16), ..OrchestratorConfig::default() };
    let model = FakeModel::new(vec![
        ModelResponse::new().text("one"),
        ModelResponse::new().text("two"),
        ModelResponse::new().text("three"),
        ModelResponse::new().text("four"),
    ]);
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine = engine_with(config, Arc::new(model.clone()), echo_tool(), events);

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine.run(prompt("hello"), sink, client, cancel_rx, task(), None).await;
    let (stuck, reason) = stuck_error(error.expect_err("spinning turn is stuck"));
    assert!(stuck, "expected a stuck error, got: {reason}");
    assert!(reason.contains("progress"));
    assert_eq!(model.call_count(), 4, "stopped before the iteration budget");
}

#[tokio::test]
async fn context_overflow_shrinks_the_transcript_and_retries_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OverflowOnSecondCall {
        calls: Arc<AtomicUsize>,
    }

    impl ModelAdapter for OverflowOnSecondCall {
        fn complete(
            &self,
            _request: ModelRequest,
            _cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(ModelResponse::new().tool_intents(vec![ToolIntent::new(
                        "tc-1",
                        "echo",
                        json!({}),
                    )])),
                    1 => Err(ModelError::Provider(
                        ee_agent_acp_server::ProviderError::ContextOverflow(
                            "prompt too long".into(),
                        ),
                    )),
                    _ => Ok(ModelResponse::new().text("done").completed()),
                }
            })
        }
    }

    let model = Arc::new(OverflowOnSecondCall { calls: Arc::new(AtomicUsize::new(0)) });
    let (sink, client, _rx) = plumbing();
    let events = EventRecorder::new();
    let engine =
        engine_with(OrchestratorConfig::default(), model.clone(), echo_tool(), events.clone());

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect("overflow is recovered by compacting the transcript");

    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        3,
        "tool round, overflow, then the retried call"
    );
    let noticed = events.events().iter().any(
        |event| matches!(event, OrchestratorEvent::Error { error } if error.contains("compacted")),
    );
    assert!(noticed, "the overflow notice must report the compaction");
}

#[tokio::test]
async fn a_second_overflow_ends_the_turn_without_looping() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AlwaysOverflows {
        calls: Arc<AtomicUsize>,
    }

    impl ModelAdapter for AlwaysOverflows {
        fn complete(
            &self,
            _request: ModelRequest,
            _cancel: watch::Receiver<bool>,
        ) -> ModelFuture<Result<ModelResponse, ModelError>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(ModelResponse::new().tool_intents(vec![ToolIntent::new(
                        "tc-1",
                        "echo",
                        json!({}),
                    )]));
                }
                Err(ModelError::Provider(ee_agent_acp_server::ProviderError::ContextOverflow(
                    "still too long".into(),
                )))
            })
        }
    }

    let model = Arc::new(AlwaysOverflows { calls: Arc::new(AtomicUsize::new(0)) });
    let (sink, client, _rx) = plumbing();
    let engine = engine_with(
        OrchestratorConfig::default(),
        model.clone(),
        echo_tool(),
        EventRecorder::new(),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect_err("a second overflow ends the turn");
    assert!(matches!(error, OrchestratorError::ContextOverflow(_)), "{error:?}");
    assert_eq!(model.calls.load(Ordering::SeqCst), 3, "one retry, never a loop");
}

#[tokio::test]
async fn tool_loops_keep_the_assistant_call_and_its_native_state() {
    let native = crate::model::OpaqueProviderState::new(json!({
        "_ee": { "model": "model-a" },
        "thinking_blocks": [{ "type": "thinking", "thinking": "plan", "signature": "sig" }],
    }))
    .expect("fits");
    let model = FakeModel::new(vec![
        ModelResponse::new()
            .tool_intents(vec![ToolIntent::new("tc-1", "echo", json!({}))])
            .with_finish_reason(Some(String::from("tool_calls")))
            .with_native(Some(native.clone())),
        ModelResponse::new().text("done").completed(),
    ]);
    let (sink, client, _rx) = plumbing();
    let engine = engine_with(
        OrchestratorConfig::default(),
        Arc::new(model.clone()),
        echo_tool(),
        EventRecorder::new(),
    );

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    engine
        .run(prompt("hello world"), sink, client, cancel_rx, task(), None)
        .await
        .expect("turn ends");

    let second = &model.requests()[1].transcript;
    let assistant = second
        .iter()
        .find(|message| message.role == ModelRole::Assistant)
        .expect("the tool-call turn stays in the transcript");
    assert!(assistant.content.iter().any(|block| matches!(
        block,
        ModelContent::ToolCall { tool_call_id, .. } if tool_call_id == "tc-1"
    )));
    assert_eq!(
        assistant.native,
        Some(native),
        "provider reasoning state must survive into the next request"
    );
}
