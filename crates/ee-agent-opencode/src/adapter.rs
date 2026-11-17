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

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::ModelMessage;
use ee_agent_orchestrator::{
    CompactionOutcome, ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse,
    OrchestratorConfig, OrchestratorProviderConfig, RecoveryConfig, StreamSink, ToolCallControl,
    ToolDefinition,
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

/// Boxed future returned by one provider-native compaction call.
pub type CompactFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionOutcome, ProviderError>> + Send + 'a>>;

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

    /// Buffered round trip with the provider-reported prompt size when known.
    ///
    /// Codecs that fit an output bound override this; the default ignores the
    /// measurement and estimates from the transcript.
    fn complete_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        _prompt_tokens: Option<u64>,
        _options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        let _ = tool_control;
        self.complete(transcript, tools)
    }

    /// Streaming round trip with the provider-reported prompt size when known.
    fn complete_streaming_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        sink: DeltaSink<'a>,
        _prompt_tokens: Option<u64>,
        _options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        let _ = tool_control;
        self.complete_streaming(transcript, tools, sink)
    }

    /// Runs one provider-native compaction request.
    ///
    /// The default fails closed; a dialect that does not implement native
    /// compaction must say so instead of silently returning a local summary.
    fn compact<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        _session: Option<&'a ee_agent_orchestrator::SessionContext>,
    ) -> CompactFuture<'a> {
        let _ = (transcript, tools);
        Box::pin(async move {
            Err(ProviderError::UnsupportedCompaction(String::from(
                "this dialect does not implement provider compaction",
            )))
        })
    }
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

    fn complete_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ResponsesCodec::complete_controlled(
            self,
            transcript,
            tools,
            tool_control,
            prompt_tokens,
            options.generation,
            options.session,
        ))
    }

    fn complete_streaming_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        sink: DeltaSink<'a>,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ResponsesCodec::complete_streaming_controlled(
            self,
            transcript,
            tools,
            tool_control,
            sink,
            prompt_tokens,
            options,
        ))
    }

    fn compact<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        session: Option<&'a ee_agent_orchestrator::SessionContext>,
    ) -> CompactFuture<'a> {
        Box::pin(ResponsesCodec::compact(self, transcript, tools, None, session))
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

    fn complete_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(MessagesCodec::complete_controlled(
            self,
            transcript,
            tools,
            tool_control,
            prompt_tokens,
            options.generation,
            options.session,
        ))
    }

    fn complete_streaming_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        sink: DeltaSink<'a>,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(MessagesCodec::complete_streaming_controlled(
            self,
            transcript,
            tools,
            tool_control,
            sink,
            prompt_tokens,
            options,
        ))
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

    fn complete_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ChatCompletionsCodec::complete_controlled(
            self,
            transcript,
            tools,
            tool_control,
            prompt_tokens,
            options.generation,
            options.session,
        ))
    }

    fn complete_streaming_with_prompt<'a>(
        &'a self,
        transcript: &'a [ModelMessage],
        tools: &'a [ToolDefinition],
        tool_control: &'a ToolCallControl,
        sink: DeltaSink<'a>,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'a>,
    ) -> CodecFuture<'a> {
        Box::pin(ChatCompletionsCodec::complete_streaming_controlled(
            self,
            transcript,
            tools,
            tool_control,
            sink,
            prompt_tokens,
            options,
        ))
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
        let model_id = self.route.model_id;
        let sampling = self.route.entry.sampling;
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            credential?;
            ee_agent_orchestrator::validate_tool_names(&request.tools)
                .map_err(ModelError::Adapter)?;
            request.tool_control.validate(&request.tools).map_err(ModelError::Adapter)?;
            check_generation(model_id, sampling, &request)?;
            let completion = codec.complete_with_prompt(
                &request.transcript,
                &request.tools,
                &request.tool_control,
                request.prompt_tokens,
                ee_chat_completions::CallOptions {
                    generation: request.generation.as_ref(),
                    session: request.session.as_ref(),
                    continuation: request.continuation.as_ref(),
                },
            );
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
        let model_id = self.route.model_id;
        let sampling = self.route.entry.sampling;
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            credential?;
            ee_agent_orchestrator::validate_tool_names(&request.tools)
                .map_err(ModelError::Adapter)?;
            request.tool_control.validate(&request.tools).map_err(ModelError::Adapter)?;
            check_generation(model_id, sampling, &request)?;
            let mut sink = |delta: StreamDelta| match delta {
                StreamDelta::Text(text) => events.text(text).map_err(forward_error),
                StreamDelta::Reasoning(text) => events.reasoning(text).map_err(forward_error),
                StreamDelta::ToolInputStart { id, name } => {
                    events.tool_input_start(id, name).map_err(forward_error)
                }
                StreamDelta::ToolInputDelta { id, text, parsed } => {
                    events.tool_input_delta(id, text, parsed).map_err(forward_error)
                }
                StreamDelta::ToolInputEnd { id } => {
                    events.tool_input_end(id).map_err(forward_error)
                }
                StreamDelta::Usage(usage) => {
                    events.usage(ee_chat_completions::model_usage_of(usage)).map_err(forward_error)
                }
                StreamDelta::Raw { frame } => events.raw(frame).map_err(forward_error),
            };
            let completion = codec.complete_streaming_with_prompt(
                &request.transcript,
                &request.tools,
                &request.tool_control,
                &mut sink,
                request.prompt_tokens,
                ee_chat_completions::CallOptions {
                    generation: request.generation.as_ref(),
                    session: request.session.as_ref(),
                    continuation: request.continuation.as_ref(),
                },
            );
            tokio::select! {
                response = completion => response.map_err(adapter_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }

    fn compact(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<CompactionOutcome, ModelError>> {
        let codec = self.codec.clone();
        let credential = self.check_credential();
        let route = self.route;
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            credential?;
            if !route.entry.compaction {
                return Err(ModelError::UnsupportedCompaction(format!(
                    "OpenCode {} {} ({})",
                    route.surface.as_str(),
                    route.model_id,
                    route.dialect.as_str()
                )));
            }
            let completion =
                codec.compact(&request.transcript, &request.tools, request.session.as_ref());
            tokio::select! {
                response = completion => response.map_err(adapter_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }
}

/// Validates per-request sampling knobs before a body exists.
///
/// Session defaults are validated at startup; a request-level override still has
/// to respect the same ranges and the model's declared capabilities, so an
/// unsupported knob is a local configuration error rather than a billable
/// provider rejection.
fn check_generation(
    model_id: &str,
    sampling: ee_agent_orchestrator::SamplingSupport,
    request: &ModelRequest,
) -> Result<(), ModelError> {
    let Some(generation) = request.generation.as_ref() else {
        return Ok(());
    };
    generation.validate().map_err(ModelError::Adapter)?;
    match generation.unsupported(sampling) {
        Some(knob) => Err(ModelError::Adapter(format!(
            "model {model_id:?} does not document the {knob} sampling knob"
        ))),
        None => Ok(()),
    }
}

/// Maps a codec failure onto the orchestrator's error type.
///
/// Codec errors already name the surface, model, and dialect (through the
/// endpoint profile label) and never carry a credential or a provider body.
fn adapter_error(error: ProviderError) -> ModelError {
    match error {
        ProviderError::Cancellation => ModelError::Cancelled,
        other => ModelError::Provider(other),
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
    let session_log_root = config.session_log_root(&session_state_dir);
    let workspace_isolation = workspace_isolation_config(&session_state_dir);
    let approval_classifier = approval_classifier_config();
    let mut provider_config = OrchestratorProviderConfig {
        implementation: Implementation::new("ee-agent-opencode", env!("CARGO_PKG_VERSION"))
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
            workspace_isolation,
            approval_classifier,
            compaction: ee_agent_orchestrator::CompactionConfig {
                mode: config.compaction_mode,
                ..ee_agent_orchestrator::CompactionConfig::default()
            },
            instructions: ee_agent_orchestrator::InstructionSourceConfig::from_env(),
            // Mid-turn tool-list changes are legal only where the catalog says
            // so; anything else is frozen for the session.
            tool_list_updates: config.route.entry.capabilities.tool_updates,
            ..OrchestratorConfig::default()
        },
        session_state_dir: Some(session_state_dir),
        session_log_dir: session_log_root.clone(),
        remote_instruction_fetcher: Some(ee_chat_completions::http_instruction_fetcher()),
        ..OrchestratorProviderConfig::default()
    };
    if let Some(root) = session_log_root {
        provider_config.orchestrator.session_log =
            ee_agent_orchestrator::SessionLogConfig::durable(root.join("session-log"));
    }
    provider_config
}

/// Resolves the opt-in approval classifier.
///
/// `EE_APPROVAL_CLASSIFIER=auto` enables risk classification before host
/// approval; `EE_APPROVAL_CLASSIFIER_MODEL` selects a registered model id.
/// Unset or `off` keeps interactive host approval unchanged.
#[must_use]
pub fn approval_classifier_config() -> ee_agent_orchestrator::ApprovalClassifierConfig {
    let mode = std::env::var("EE_APPROVAL_CLASSIFIER")
        .ok()
        .and_then(|value| ee_agent_orchestrator::ApprovalClassifierMode::parse(&value))
        .unwrap_or_default();
    let model_id = std::env::var("EE_APPROVAL_CLASSIFIER_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    ee_agent_orchestrator::ApprovalClassifierConfig { mode, model_id, ..Default::default() }
}

/// Resolves the opt-in worktree isolation policy for a session state root.
///
/// `EE_WORKTREE_ISOLATION=1` enables isolated `implementer` children under
/// `<session_state_dir>/worktrees`; `EE_KEEP_WORKTREES=1` retains completed
/// worktrees for host apply instead of pruning them.
#[must_use]
pub fn workspace_isolation_config(
    session_state_dir: &std::path::Path,
) -> ee_agent_orchestrator::WorkspaceIsolationConfig {
    fn env_flag(name: &str) -> bool {
        std::env::var(name).ok().is_some_and(|value| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        })
    }
    ee_agent_orchestrator::WorkspaceIsolationConfig {
        enabled: env_flag("EE_WORKTREE_ISOLATION"),
        worktree_root: Some(session_state_dir.join("worktrees")),
        keep_worktrees: env_flag("EE_KEEP_WORKTREES"),
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
