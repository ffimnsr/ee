//! OpenCode model adapter: one resolved route, one dialect codec.
//!
//! The route is resolved before this module runs, and [`OpenCodeModelAdapter::new`]
//! builds only the codec the route's dialect selects, so exactly one endpoint and
//! one protocol shape exist per agent process. Requests, decoding, retries, and
//! SSE framing stay inside the codecs; this module owns the orchestrator contract:
//! cancellation, streamed update ordering, and error mapping that never carries a
//! credential, a raw request body, or an unbounded provider error body.
//!
//! [`crate::critic::opencode_multi_model_provider`] wires one or two of these
//! adapters into `ee-agent-orchestrator` with the same tool policy every
//! production ee agent uses ([`ee_agent_orchestrator::default_agent_policy`]),
//! so the OpenCode agent inherits the established approval, scope, cancellation,
//! and recovery boundaries instead of inventing its own.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::ModelMessage;
use ee_agent_orchestrator::{
    ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse, OrchestratorConfig,
    OrchestratorProviderConfig, RecoveryConfig, StreamSink, ToolDefinition,
};
use ee_agent_protocol::Implementation;
use ee_chat_completions::{DeltaSink, EndpointTransport, StreamDelta, TokenSource, wait_cancelled};
use tokio::sync::watch;

use crate::chat_completions::ChatCompletionsCodec;
use crate::config::Config;
use crate::messages::MessagesCodec;
use crate::responses::ResponsesCodec;
use crate::routes::{OpenCodeDialect, OpenCodeRoute};

/// Boxed future returned by a dialect codec.
pub type CodecFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelResponse, ProviderError>> + Send + 'a>>;

/// One dialect codec behind the dispatcher.
///
/// The three OpenCode dialects implement this, and tests implement it with
/// scripted answers so ACP flows stay network-free.
pub trait DialectCodec: Send + Sync + 'static {
    /// Dialect this codec speaks.
    fn dialect(&self) -> OpenCodeDialect;

    /// Performs one buffered round trip.
    fn complete<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
    ) -> CodecFuture<'a>;

    /// Performs one streaming round trip, forwarding deltas to `sink`.
    fn complete_streaming<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        sink: DeltaSink<'a>,
    ) -> CodecFuture<'a>;
}

impl DialectCodec for ResponsesCodec {
    fn dialect(&self) -> OpenCodeDialect {
        OpenCodeDialect::OpenAiResponses
    }

    fn complete<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
    ) -> CodecFuture<'a> {
        Box::pin(ResponsesCodec::complete(self, transcript, tools))
    }

    fn complete_streaming<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        sink: DeltaSink<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ResponsesCodec::complete_streaming(self, transcript, tools, sink))
    }
}

impl DialectCodec for MessagesCodec {
    fn dialect(&self) -> OpenCodeDialect {
        OpenCodeDialect::AnthropicMessages
    }

    fn complete<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
    ) -> CodecFuture<'a> {
        Box::pin(MessagesCodec::complete(self, transcript, tools))
    }

    fn complete_streaming<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        sink: DeltaSink<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(MessagesCodec::complete_streaming(self, transcript, tools, sink))
    }
}

impl DialectCodec for ChatCompletionsCodec {
    fn dialect(&self) -> OpenCodeDialect {
        OpenCodeDialect::OpenAiChatCompletions
    }

    fn complete<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
    ) -> CodecFuture<'a> {
        Box::pin(ChatCompletionsCodec::complete(self, transcript, tools))
    }

    fn complete_streaming<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        sink: DeltaSink<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ChatCompletionsCodec::complete_streaming(self, transcript, tools, sink))
    }
}

/// One OpenCode route bound to the codec its dialect selected.
pub struct OpenCodeModelAdapter {
    route: OpenCodeRoute,
    token: TokenSource,
    codec: Arc<dyn DialectCodec>,
}

impl OpenCodeModelAdapter {
    /// Builds the adapter for one configured route.
    ///
    /// Only the selected dialect's codec, and therefore only one endpoint, is
    /// constructed; no other dialect is reachable from this process.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the route's endpoint is unusable or the HTTP
    /// client cannot be built; no request is sent and no credential is read.
    pub fn new(config: &Config) -> Result<Self, String> {
        let profile = config.profile().map_err(|error| error.to_string())?;
        let transport = EndpointTransport::new(profile, config.token_source())?;
        let codec: Arc<dyn DialectCodec> = match config.route.dialect {
            OpenCodeDialect::OpenAiResponses => Arc::new(ResponsesCodec::new(transport)),
            OpenCodeDialect::AnthropicMessages => Arc::new(MessagesCodec::new(transport)),
            OpenCodeDialect::OpenAiChatCompletions => {
                Arc::new(ChatCompletionsCodec::new(transport))
            }
        };
        Ok(Self { route: config.route, token: config.token_source(), codec })
    }

    /// Builds an adapter over an injected codec (tests).
    ///
    /// The route still decides which dialect the adapter reports, so a scripted
    /// codec cannot make the agent claim a dialect its route does not serve.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub fn with_codec(
        route: OpenCodeRoute,
        token: TokenSource,
        codec: Arc<dyn DialectCodec>,
    ) -> Self {
        assert_eq!(
            codec.dialect(),
            route.dialect,
            "the injected codec must speak the routed dialect"
        );
        Self { route, token, codec }
    }

    /// Returns the resolved route this adapter serves.
    #[must_use]
    pub fn route(&self) -> &OpenCodeRoute {
        &self.route
    }

    /// Returns the dialect this adapter speaks.
    #[must_use]
    pub fn dialect(&self) -> OpenCodeDialect {
        self.route.dialect
    }

    /// Resolves the credential before any request exists.
    fn check_credential(&self) -> Result<(), ModelError> {
        (self.token)().map(|_| ()).map_err(ModelError::Adapter)
    }
}

impl ModelAdapter for OpenCodeModelAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let codec = self.codec.clone();
        let credential = self.check_credential();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            credential?;
            let completion = codec.complete(&request.transcript, &request.tools);
            tokio::select! {
                response = completion => response.map_err(adapter_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }

    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let codec = self.codec.clone();
        let credential = self.check_credential();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            credential?;
            let mut sink = |delta: StreamDelta| match delta {
                StreamDelta::Text(text) => events.text(text).map_err(forward_error),
                StreamDelta::Reasoning(text) => events.reasoning(text).map_err(forward_error),
            };
            let completion =
                codec.complete_streaming(&request.transcript, &request.tools, &mut sink);
            tokio::select! {
                response = completion => response.map_err(adapter_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }
}

/// Maps a codec failure onto the orchestrator's error type.
///
/// Codec errors already name the surface, model, and dialect (through the
/// endpoint profile label) and never carry a credential or a provider body.
fn adapter_error(error: ProviderError) -> ModelError {
    match error {
        ProviderError::Cancellation => ModelError::Cancelled,
        other => ModelError::Adapter(other.to_string()),
    }
}

/// Keeps a closed stream distinguishable from a transport failure without
/// leaking the dropped channel's contents.
fn forward_error(error: ModelError) -> ProviderError {
    ProviderError::BackendFailure(format!("streamed update was rejected: {error}"))
}

/// Builds the orchestrator configuration for one OpenCode session.
///
/// Recovery is durable only when a checkpoint directory is configured; without
/// one, recovery stays same-process so crash recovery is never implied. The
/// rubber-duck mode reaches the runtime unchanged, so an unusable or absent
/// critic is reported by the orchestrator as contrast unavailable rather than
/// silently ignored.
#[must_use]
pub fn opencode_orchestrator_config(
    config: &Config,
    session_state_dir: PathBuf,
) -> OrchestratorProviderConfig {
    OrchestratorProviderConfig {
        implementation: Implementation::new("ee-opencode-agent", env!("CARGO_PKG_VERSION"))
            .title("OpenCode"),
        orchestrator: OrchestratorConfig {
            context_window_tokens: config.context_window,
            max_loop_iterations: config.max_iterations,
            max_model_calls: config.max_iterations,
            rubber_duck: config.rubber_duck.clone(),
            rubber_duck_triggers: ee_agent_orchestrator::RubberDuckTriggerConfig {
                mode: if config.rubber_duck.mode == ee_agent_orchestrator::RubberDuckMode::Automatic
                {
                    ee_agent_orchestrator::RubberDuckTriggerMode::Automatic
                } else {
                    ee_agent_orchestrator::RubberDuckTriggerMode::ManualOnly
                },
            },
            recovery: match config.checkpoint_dir.clone() {
                Some(directory) => RecoveryConfig::durable(directory),
                None => RecoveryConfig::memory_only(),
            },
            ..OrchestratorConfig::default()
        },
        session_state_dir: Some(session_state_dir),
        ..OrchestratorProviderConfig::default()
    }
}

/// Builds the production provider: the OpenCode adapter plus the same tool
/// policy every production ee agent uses.
///
/// The concrete [`OpenCodeModelAdapter`] parameter keeps generic test adapters
/// from being mistaken for the production OpenCode configuration.
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn opencode_orchestrated_provider(
    config: &Config,
    session_state_dir: PathBuf,
    adapter: OpenCodeModelAdapter,
) -> ee_agent_orchestrator::OrchestratorProvider {
    ee_agent_orchestrator::OrchestratorProvider::with_policy(
        opencode_orchestrator_config(config, session_state_dir),
        Arc::new(adapter),
        ee_agent_orchestrator::default_agent_policy(),
    )
}

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

#[cfg(test)]
mod tests;
