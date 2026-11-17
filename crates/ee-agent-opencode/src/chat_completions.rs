//! OpenAI-compatible Chat Completions codec bound to one OpenCode route.
//!
//! The transport, SSE decoding, retry boundary, and normalization live in
//! `ee-chat-completions`, shared with OpenRouter; this module only binds an
//! authorized OpenCode endpoint to that shared client, so the third OpenCode
//! dialect has no private duplicate codec.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    GenerationOptions, ModelMessage, ModelResponse, ToolCallControl, ToolDefinition,
};
use ee_chat_completions::{
    ChatCompletionsClient, ChatRequest, ChatRequestParts, DeltaSink, EndpointProfile,
    EndpointTransport, chat_request_parts, response_from_turn,
};

/// Dialect every route served by this module speaks.
pub const DIALECT: crate::routes::OpenCodeDialect =
    crate::routes::OpenCodeDialect::OpenAiChatCompletions;

/// One Chat Completions round trip over an authorized OpenCode endpoint.
pub struct ChatCompletionsCodec {
    client: ChatCompletionsClient,
}

impl ChatCompletionsCodec {
    /// Wraps one authorized transport.
    #[must_use]
    pub fn new(transport: EndpointTransport) -> Self {
        Self { client: ChatCompletionsClient::from_transport(transport) }
    }

    /// Returns the provider profile this codec sends with.
    #[must_use]
    pub fn profile(&self) -> &EndpointProfile {
        self.client.profile()
    }

    /// Performs one buffered round trip under the profile retry policy.
    ///
    /// # Errors
    ///
    /// Returns a typed error for transport, status, or decode failures.
    pub async fn complete(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
    ) -> Result<ModelResponse, ProviderError> {
        self.complete_with_prompt(transcript, tools, None).await
    }

    /// Performs one buffered round trip using the provider-reported prompt size
    /// when the caller has one.
    pub(crate) async fn complete_with_prompt(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let control = ToolCallControl::auto();
        self.complete_controlled(transcript, tools, &control, prompt_tokens, None, None).await
    }

    /// Performs one buffered round trip with tool-call control.
    pub(crate) async fn complete_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        prompt_tokens: Option<u64>,
        generation: Option<&GenerationOptions>,
        session: Option<&ee_agent_orchestrator::SessionContext>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let (messages, tools) = chat_request_parts(&ChatRequestParts {
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            definitions: tools,
            cache: &profile.cache,
            model: &profile.model,
            reasoning_field: profile.reasoning_field.as_deref(),
            tools_strict: profile.tools_strict,
            freeform: &profile.freeform,
            prompt_tokens,
        });
        let turn = self
            .client
            .complete_request(&ChatRequest {
                messages,
                tools,
                tool_control: tool_control.clone(),
                generation: generation.cloned(),
                prompt_tokens,
                session: session.cloned(),
            })
            .await?;
        Ok(crate::dialect::finalize(response_from_turn(turn), profile))
    }

    /// Performs one streaming round trip under the profile retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet.
    ///
    /// # Errors
    ///
    /// Returns a typed error for transport, status, decode, or sink failures.
    pub async fn complete_streaming(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        sink: DeltaSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.complete_streaming_with_prompt(transcript, tools, sink, None).await
    }

    /// Performs one streaming round trip using the provider-reported prompt size
    /// when the caller has one.
    pub(crate) async fn complete_streaming_with_prompt(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
    ) -> Result<ModelResponse, ProviderError> {
        let control = ToolCallControl::auto();
        self.complete_streaming_controlled(
            transcript,
            tools,
            &control,
            sink,
            prompt_tokens,
            ee_chat_completions::CallOptions::default(),
        )
        .await
    }

    /// Performs one streaming round trip with tool-call control.
    pub(crate) async fn complete_streaming_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let profile = self.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let (messages, tools) = chat_request_parts(&ChatRequestParts {
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            definitions: tools,
            cache: &profile.cache,
            model: &profile.model,
            reasoning_field: profile.reasoning_field.as_deref(),
            tools_strict: profile.tools_strict,
            freeform: &profile.freeform,
            prompt_tokens,
        });
        let turn = self
            .client
            .complete_streaming_request(
                &ChatRequest {
                    messages,
                    tools,
                    tool_control: tool_control.clone(),
                    generation: options.generation.cloned(),
                    prompt_tokens,
                    session: options.session.cloned(),
                },
                sink,
            )
            .await?;
        Ok(crate::dialect::finalize(response_from_turn(turn), profile))
    }
}
