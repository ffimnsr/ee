//! Streaming model support.
//!
//! A streaming adapter emits partial [`StreamEvent`] chunks (text and
//! reasoning) through a cloneable [`StreamSink`]; the framework-side
//! [`StreamConsumer`] merges them into one [`StreamedTurn`] in arrival order,
//! forwarding each chunk to the ACP [`UpdateSink`] as it arrives.  The merged
//! turn converts into a [`ModelResponse`] that is guaranteed to match the
//! final transcript message, so streamed and non-streamed paths produce
//! identical transcript state.
//!
//! Cancellation stops consumption at the next chunk boundary: chunks already
//! emitted stay, later chunks are dropped, and the merged turn records that
//! it was cancelled.

use ee_acp_agent_server::UpdateSink;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::error::OrchestratorError;
use crate::model::{ModelError, ModelFuture, ModelRequest, ModelResponse};

/// One streamed partial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StreamEvent {
    /// A partial assistant text chunk.
    TextChunk(String),
    /// A partial reasoning chunk.
    ReasoningChunk(String),
}

/// Boxed future returned by [`StreamingModelAdapter::complete_streaming`].
pub type StreamingModelFuture = ModelFuture<Result<ModelResponse, ModelError>>;

/// Adapter trait for models that stream partial output.
///
/// Implementations emit chunks through the supplied [`StreamSink`], observe
/// cancellation through the watch channel, and return the final response
/// whose text/reasoning must match the merged chunks (transcript
/// consistency).
pub trait StreamingModelAdapter: Send + Sync + 'static {
    /// Runs one streaming completion.
    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> StreamingModelFuture;
}

/// Cloneable outbound handle for streamed chunks.
#[derive(Clone, Debug)]
pub struct StreamSink {
    tx: mpsc::UnboundedSender<StreamEvent>,
}

impl StreamSink {
    /// Emits one partial text chunk.
    ///
    /// Returns an adapter error when the consumer already closed the stream
    /// (e.g. after cancellation or transport close), so adapters stop early.
    pub fn text(&self, chunk: impl Into<String>) -> Result<(), ModelError> {
        self.send(StreamEvent::TextChunk(chunk.into()))
    }

    /// Emits one partial reasoning chunk.
    pub fn reasoning(&self, chunk: impl Into<String>) -> Result<(), ModelError> {
        self.send(StreamEvent::ReasoningChunk(chunk.into()))
    }

    fn send(&self, event: StreamEvent) -> Result<(), ModelError> {
        self.tx
            .send(event)
            .map_err(|_| ModelError::Adapter("stream consumer dropped the channel".to_string()))
    }
}

/// Inbound end of a stream channel.
#[derive(Debug)]
pub struct StreamReceiver {
    rx: mpsc::UnboundedReceiver<StreamEvent>,
}

impl StreamReceiver {
    /// Turns the receiver into a merging consumer.
    #[must_use]
    pub fn into_consumer(self) -> StreamConsumer {
        StreamConsumer { rx: self.rx }
    }
}

/// Creates a stream channel pair.
#[must_use]
pub fn stream_channel() -> (StreamSink, StreamReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    (StreamSink { tx }, StreamReceiver { rx })
}

/// Merges streamed chunks into one deterministic turn.
#[derive(Debug)]
pub struct StreamConsumer {
    rx: mpsc::UnboundedReceiver<StreamEvent>,
}

/// One fully merged streamed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StreamedTurn {
    /// Merged assistant text in chunk order.
    pub text: String,
    /// Merged reasoning in chunk order.
    pub reasoning: String,
    /// Number of text chunks consumed.
    pub text_chunks: usize,
    /// Number of reasoning chunks consumed.
    pub reasoning_chunks: usize,
    /// Whether consumption stopped early on cancellation.
    pub cancelled: bool,
}

impl StreamedTurn {
    /// Converts the merged turn into a normalized response whose text and
    /// reasoning equal the merged chunks (transcript consistency).
    #[must_use]
    pub fn into_model_response(self) -> ModelResponse {
        let mut response = ModelResponse::new().text(self.text);
        if !self.reasoning.is_empty() {
            response = response.reasoning(self.reasoning);
        }
        response
    }
}

impl StreamConsumer {
    /// Consumes the stream until it closes or `cancel` flips.
    ///
    /// Every text chunk is forwarded to `updates.agent_message_chunk` and
    /// every reasoning chunk to `updates.agent_thought_chunk` with the given
    /// message id, in arrival order, before merging into the returned turn.
    /// Chunks are always delivered to the update sink *before* the merged
    /// response is returned, so clients see streaming precede the final turn.
    pub async fn run(
        mut self,
        updates: Option<&UpdateSink>,
        cancel: &watch::Receiver<bool>,
        message_id: &str,
    ) -> StreamedTurn {
        let mut turn = StreamedTurn {
            text: String::new(),
            reasoning: String::new(),
            text_chunks: 0,
            reasoning_chunks: 0,
            cancelled: false,
        };
        loop {
            if *cancel.borrow() {
                turn.cancelled = true;
                break;
            }
            let Some(event) = self.rx.recv().await else {
                break;
            };
            // Honour cancellation at the next chunk boundary: a chunk that
            // arrives after the cancel flag is dropped, never processed.
            if *cancel.borrow() {
                turn.cancelled = true;
                break;
            }
            match event {
                StreamEvent::TextChunk(chunk) => {
                    let delivered = updates.is_none_or(|updates| {
                        updates.agent_message_chunk(message_id, chunk.clone()).is_ok()
                    });
                    if !delivered {
                        // The client is gone; stop consuming and let the
                        // adapter observe the closed stream.
                        turn.cancelled = true;
                        break;
                    }
                    turn.text.push_str(&chunk);
                    turn.text_chunks += 1;
                }
                StreamEvent::ReasoningChunk(chunk) => {
                    let delivered = updates.is_none_or(|updates| {
                        updates.agent_thought_chunk(message_id, chunk.clone()).is_ok()
                    });
                    if !delivered {
                        turn.cancelled = true;
                        break;
                    }
                    turn.reasoning.push_str(&chunk);
                    turn.reasoning_chunks += 1;
                }
            }
        }
        turn
    }
}

/// Streams a completion through a bounded channel: `call` receives the sink
/// and must send chunks and return the final response.
#[allow(clippy::type_complexity)]
pub async fn run_streaming<'a, F>(
    call: F,
    updates: Option<&'a UpdateSink>,
    cancel: &'a watch::Receiver<bool>,
    message_id: &str,
) -> Result<StreamedTurn, OrchestratorError>
where
    F: FnOnce(StreamSink) -> StreamingModelFuture,
{
    let (sink, receiver) = stream_channel();
    let future = call(sink);
    let consumer = receiver.into_consumer();
    let (turn, result) = tokio::join!(consumer.run(updates, cancel, message_id), future);
    result?;
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_agent_protocol::{SessionId, SessionUpdate};
    use tokio::sync::mpsc;

    fn plumbing() -> (UpdateSink, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (UpdateSink::new_for_test(SessionId::new("s-1"), tx), rx)
    }

    fn request() -> ModelRequest {
        ModelRequest {
            transcript: Vec::new(),
            tools: Vec::new(),
            budget: crate::budget::BudgetSnapshot {
                iterations_used: 0,
                iterations_max: 16,
                model_calls_used: 0,
                model_calls_max: 16,
                tool_calls_used: 0,
                tool_calls_max: 32,
                subagents_used: 0,
                subagents_max: 4,
                output_bytes_used: 0,
                output_bytes_max: 8_192,
                input_tokens_used: None,
                input_tokens_max: None,
                output_tokens_used: None,
                output_tokens_max: None,
            },
            task: crate::tasks::TaskNode::new(crate::tasks::TaskId::new("task-1"), "main", "root"),
            available_models: Vec::new(),
            model_id: None,
        }
    }

    /// Fake streaming adapter: emits scripted chunks, then returns the final
    /// response; observes cancellation and returns `Cancelled`.
    #[derive(Clone)]
    struct FakeStreamingModel {
        chunks: Vec<StreamEvent>,
        final_response: ModelResponse,
    }

    impl FakeStreamingModel {
        fn new(chunks: Vec<StreamEvent>, final_response: ModelResponse) -> Self {
            Self { chunks, final_response }
        }
    }

    impl StreamingModelAdapter for FakeStreamingModel {
        fn complete_streaming(
            &self,
            _request: ModelRequest,
            cancel: watch::Receiver<bool>,
            events: StreamSink,
        ) -> StreamingModelFuture {
            let chunks = self.chunks.clone();
            let final_response = self.final_response.clone();
            Box::pin(async move {
                for chunk in chunks {
                    if *cancel.borrow() {
                        return Err(ModelError::Cancelled);
                    }
                    match &chunk {
                        StreamEvent::TextChunk(text) => events.text(text)?,
                        StreamEvent::ReasoningChunk(reasoning) => events.reasoning(reasoning)?,
                    }
                }
                Ok(final_response)
            })
        }
    }

    async fn next_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> SessionUpdate {
        match rx.recv().await.expect("outbound event queued") {
            OutboundEvent::Update { update, .. } => *update,
            other => panic!("expected update event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streamed_text_chunks_merge_in_order_and_emit_updates() {
        let (sink, mut rx) = plumbing();
        let (_cancel_tx, cancel) = watch::channel(false);
        let model = FakeStreamingModel::new(
            vec![
                StreamEvent::TextChunk("Hello ".into()),
                StreamEvent::TextChunk("world".into()),
                StreamEvent::TextChunk("!".into()),
            ],
            ModelResponse::new().text("Hello world!").completed(),
        );
        let (sink_tx, receiver) = stream_channel();
        let adapter_future = model.complete_streaming(request(), cancel.clone(), sink_tx);
        let (turn, response) = tokio::join!(
            receiver.into_consumer().run(Some(&sink), &cancel, "msg-1"),
            adapter_future
        );
        let response = response.expect("adapter completes");

        assert_eq!(turn.text, "Hello world!");
        assert_eq!(turn.text_chunks, 3);
        assert_eq!(turn.reasoning_chunks, 0);
        assert!(!turn.cancelled);
        // Transcript consistency: merged turn equals the final response.
        assert_eq!(turn.clone().into_model_response().text, response.text);
        // Updates streamed before completion, in chunk order.
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
        assert!(rx.try_recv().is_err(), "no further outbound events");
    }

    #[tokio::test]
    async fn streamed_reasoning_chunks_merge_in_order() {
        let (sink, mut rx) = plumbing();
        let (_cancel_tx, cancel) = watch::channel(false);
        let model = FakeStreamingModel::new(
            vec![
                StreamEvent::ReasoningChunk("think ".into()),
                StreamEvent::ReasoningChunk("hard".into()),
                StreamEvent::TextChunk("Answer".into()),
            ],
            ModelResponse::new().reasoning("think hard").text("Answer").completed(),
        );
        let (sink_tx, receiver) = stream_channel();
        let adapter_future = model.complete_streaming(request(), cancel.clone(), sink_tx);
        let (turn, response) = tokio::join!(
            receiver.into_consumer().run(Some(&sink), &cancel, "msg-1"),
            adapter_future
        );
        let response = response.expect("adapter completes");

        assert_eq!(turn.reasoning, "think hard");
        assert_eq!(turn.reasoning_chunks, 2);
        assert_eq!(turn.text, "Answer");
        let merged = turn.into_model_response();
        assert_eq!(merged.reasoning.as_deref(), Some("think hard"));
        assert_eq!(merged.text, response.text);
        assert_eq!(merged.reasoning, response.reasoning);
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentThoughtChunk(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentThoughtChunk(_)));
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
    }

    #[tokio::test]
    async fn cancellation_stops_consumption_at_next_chunk_boundary() {
        let (sink, mut rx) = plumbing();
        let (cancel_tx, cancel) = watch::channel(false);
        let (sink_tx, receiver) = stream_channel();
        let run = tokio::spawn(async move {
            receiver.into_consumer().run(Some(&sink), &cancel, "msg-1").await
        });

        // Adapter side: chunk 1 arrives and is consumed + emitted.
        sink_tx.text("first ").expect("chunk 1");
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
        // Cancel, then send more chunks: the consumer stops at the next
        // boundary and never processes them.
        cancel_tx.send(true).expect("cancel");
        sink_tx.text("second").expect("chunk 2");
        sink_tx.text("third").expect("chunk 3");

        let turn = run.await.expect("consumer resolves");
        assert!(turn.cancelled, "cancellation recorded");
        assert_eq!(turn.text_chunks, 1, "consumption stopped at the next boundary");
        assert_eq!(turn.text, "first ");
        assert_eq!(turn.reasoning_chunks, 0);
        assert!(rx.try_recv().is_err(), "no update for cancelled chunks");
    }

    #[tokio::test]
    async fn sink_reports_closed_channel_to_adapter() {
        let (sink, _receiver) = stream_channel();
        drop(_receiver);
        assert!(sink.text("lost").is_err(), "adapter sees the closed stream");
    }

    #[tokio::test]
    async fn run_streaming_helper_returns_merged_turn() {
        let (sink, mut rx) = plumbing();
        let (_cancel_tx, cancel) = watch::channel(false);
        let model = FakeStreamingModel::new(
            vec![StreamEvent::TextChunk("hi".into())],
            ModelResponse::new().text("hi").completed(),
        );
        let turn = run_streaming(
            |events| model.complete_streaming(request(), cancel.clone(), events),
            Some(&sink),
            &cancel,
            "msg-1",
        )
        .await
        .expect("streams");
        assert_eq!(turn.text, "hi");
        assert!(matches!(next_update(&mut rx).await, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn stream_events_roundtrip_through_json() {
        let events =
            vec![StreamEvent::TextChunk("a".into()), StreamEvent::ReasoningChunk("b".into())];
        let json = serde_json::to_string(&events).expect("serializes");
        let restored: Vec<StreamEvent> = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, events);
    }

    #[test]
    fn merged_turn_into_model_response_is_complete() {
        let turn = StreamedTurn {
            text: "answer".into(),
            reasoning: "thinking".into(),
            text_chunks: 1,
            reasoning_chunks: 1,
            cancelled: false,
        };
        let response = turn.into_model_response();
        assert_eq!(response.text, "answer");
        assert_eq!(response.reasoning.as_deref(), Some("thinking"));
        let empty = StreamedTurn {
            text: String::new(),
            reasoning: String::new(),
            text_chunks: 0,
            reasoning_chunks: 0,
            cancelled: false,
        };
        let response = empty.into_model_response();
        assert_eq!(response.text, "");
        assert_eq!(response.reasoning, None);
    }
}
