//! Scripted dialect codec and OpenCode fixtures for tests.
//!
//! Everything that exercises the dispatcher, the orchestrator wiring, or the ACP
//! loop uses this module, so no test needs a network, a live OpenCode account, or
//! a credential.
//!
//! The scripted codec sits exactly where the production codecs sit — behind
//! [`DialectCodec`] — so an ACP-level test still covers the cancellation,
//! streamed-update ordering, credential, and error-mapping paths
//! [`OpenCodeModelAdapter`] runs in production; only the wire encoding is
//! replaced.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{ModelMessage, ModelResponse, ToolDefinition, ToolIntent};
use ee_chat_completions::{BearerToken, DeltaSink, RetryPolicy, TokenSource, emit_finished_deltas};
use serde_json::{Value, json};

use super::{CodecFuture, DialectCodec, OpenCodeModelAdapter};
use crate::config::{
    Config, DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_RETRY_BASE_DELAY_MS, DEFAULT_RETRY_MAX_ATTEMPTS,
    DEFAULT_RETRY_MAX_DELAY_MS, DEFAULT_SYSTEM_PROMPT, DEFAULT_TIMEOUT_MS, MISSING_API_KEY,
};
use crate::routes::{self, OpenCodeDialect, OpenCodeRoute, OpenCodeSurface};

/// Placeholder credential for scripted adapters; never leaves the process.
pub const TEST_API_KEY: &str = "sk-opencode-scripted-test";

/// Bearer token source that always resolves.
#[must_use]
pub fn test_token_source() -> TokenSource {
    Arc::new(|| Ok(BearerToken::new(TEST_API_KEY)))
}

/// Bearer token source that fails the way a missing key fails in production.
#[must_use]
pub fn missing_token_source() -> TokenSource {
    Arc::new(|| Err(String::from(MISSING_API_KEY)))
}

/// Configuration for one documented route, with a scripted credential.
///
/// # Panics
///
/// Panics when `(surface, model_id)` is not a documented catalog entry; fixtures
/// are expected to name real routes.
#[must_use]
pub fn test_config(surface: OpenCodeSurface, model_id: &str) -> Config {
    let route = routes::resolve_route(surface, model_id).expect("fixture names a documented route");
    Config {
        route,
        api_key: Some(String::from(TEST_API_KEY)),
        critic_model: None,
        rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
        reasoning_effort: None,
        system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
        timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
        context_window: DEFAULT_CONTEXT_WINDOW_TOKENS,
        retry: RetryPolicy {
            max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS),
            max_delay: Duration::from_millis(DEFAULT_RETRY_MAX_DELAY_MS),
        },
        checkpoint_dir: None,
    }
}

/// One scripted codec outcome.
#[derive(Debug, Clone)]
pub enum ScriptedAnswer {
    /// Return this normalized response.
    Complete(Box<ModelResponse>),
    /// Never resolve, so a test observes a model call that is still in flight
    /// when cancellation or a deadline arrives.
    Pending,
    /// Fail the way a transport or provider-protocol failure does.
    Failure(String),
    /// Fail the way a cancelled request does.
    Cancellation,
}

impl ScriptedAnswer {
    /// Answers with assistant text that completes the turn.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self::Complete(Box::new(ModelResponse::new().text(text).completed()))
    }

    /// Answers with reasoning plus assistant text that completes the turn.
    #[must_use]
    pub fn reasoning(reasoning: &str, text: &str) -> Self {
        Self::Complete(Box::new(ModelResponse::new().reasoning(reasoning).text(text).completed()))
    }

    /// Answers with one tool intent; the orchestrator runs the tool and
    /// continues the turn, so the response is not completing.
    #[must_use]
    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        Self::Complete(Box::new(
            ModelResponse::new().tool_intents(vec![ToolIntent::new(id, name, arguments)]),
        ))
    }
}

/// One codec request, recorded before its scripted answer was chosen.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Transcript the adapter forwarded.
    pub transcript: Vec<ModelMessage>,
    /// Tool definitions the adapter forwarded.
    pub tools: Vec<ToolDefinition>,
}

impl RecordedRequest {
    /// Renders the recorded request the way a fixture assertion expects it.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({ "transcript": &self.transcript, "tools": &self.tools })
    }
}

/// Codec that replays scripted answers and records every request it receives.
///
/// Answers are consumed in order, one per model call. When the script runs out
/// the codec parks instead of inventing an answer, so a test that expects fewer
/// model calls than the runtime makes blocks (and its own cancellation or the
/// harness timeout reports it) rather than silently passing.
#[derive(Debug)]
pub struct ScriptedCodec {
    dialect: OpenCodeDialect,
    answers: Mutex<VecDeque<ScriptedAnswer>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl ScriptedCodec {
    /// Builds a codec that answers with `answers` in order.
    #[must_use]
    pub fn new(dialect: OpenCodeDialect, answers: Vec<ScriptedAnswer>) -> Self {
        Self {
            dialect,
            answers: Mutex::new(answers.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Builds a codec that never answers, for cancellation and timeout tests.
    #[must_use]
    pub fn pending(dialect: OpenCodeDialect) -> Self {
        Self::new(dialect, Vec::new())
    }

    /// Returns every request recorded so far, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("scripted requests poisoned").clone()
    }

    /// Returns the transcript of the most recent request, if any request was
    /// recorded.
    #[must_use]
    pub fn last_transcript(&self) -> Option<Vec<ModelMessage>> {
        self.requests
            .lock()
            .expect("scripted requests poisoned")
            .last()
            .map(|r| r.transcript.clone())
    }

    /// Returns the tool definitions of the most recent request, if any request
    /// was recorded.
    #[must_use]
    pub fn last_tools(&self) -> Option<Vec<ToolDefinition>> {
        self.requests.lock().expect("scripted requests poisoned").last().map(|r| r.tools.clone())
    }

    /// Builds the production dispatcher over this codec for `route`.
    ///
    /// # Panics
    ///
    /// Panics when the scripted dialect does not match `route.dialect`, which
    /// keeps a fixture from pretending the agent speaks a protocol its route
    /// does not serve.
    #[must_use]
    pub fn adapter(self: &Arc<Self>, route: OpenCodeRoute) -> OpenCodeModelAdapter {
        OpenCodeModelAdapter::with_codec(route, test_token_source(), self.clone())
    }

    /// Records one request and pops the answer that serves it.
    fn take_answer(&self, transcript: &[ModelMessage], tools: &[ToolDefinition]) -> ScriptedAnswer {
        self.requests
            .lock()
            .expect("scripted requests poisoned")
            .push(RecordedRequest { transcript: transcript.to_vec(), tools: tools.to_vec() });
        self.answers
            .lock()
            .expect("scripted answers poisoned")
            .pop_front()
            .unwrap_or(ScriptedAnswer::Pending)
    }
}

impl DialectCodec for ScriptedCodec {
    fn dialect(&self) -> OpenCodeDialect {
        self.dialect
    }

    fn complete<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
    ) -> CodecFuture<'a> {
        let answer = self.take_answer(transcript, tools);
        Box::pin(async move { resolve(answer).await })
    }

    fn complete_streaming<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        sink: DeltaSink<'a>,
    ) -> CodecFuture<'a> {
        let answer = self.take_answer(transcript, tools);
        Box::pin(async move {
            let response = resolve(answer).await?;
            // Mirrors the production codecs: the finished response reaches the
            // sink before the adapter returns it.
            emit_finished_deltas(&response, sink)?;
            Ok(response)
        })
    }
}

/// Resolves a scripted answer; a pending answer never completes.
async fn resolve(answer: ScriptedAnswer) -> Result<ModelResponse, ProviderError> {
    match answer {
        ScriptedAnswer::Complete(response) => Ok(*response),
        ScriptedAnswer::Pending => std::future::pending().await,
        ScriptedAnswer::Failure(detail) => Err(ProviderError::BackendFailure(detail)),
        ScriptedAnswer::Cancellation => Err(ProviderError::Cancellation),
    }
}
