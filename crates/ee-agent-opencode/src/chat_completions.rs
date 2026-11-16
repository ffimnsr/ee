//! OpenAI-compatible Chat Completions codec bound to one OpenCode route.
//!
//! The transport, SSE decoding, retry boundary, and normalization live in
//! `ee-chat-completions`, shared with OpenRouter; this module only binds an
//! authorized OpenCode endpoint to that shared client, so the third OpenCode
//! dialect has no private duplicate codec.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{ModelMessage, ModelResponse, ToolDefinition};
use ee_chat_completions::{
    ChatCompletionsClient, DeltaSink, EndpointProfile, EndpointTransport,
    messages_from_transcript_replaying, response_from_turn, seal_response, tools_from_definitions,
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
        let profile = self.profile();
        let messages = messages_from_transcript_replaying(
            &profile.system_prompt,
            transcript,
            profile.prompt_cache,
            &profile.model,
            profile.reasoning_field.as_deref(),
        );
        let tools = tools_from_definitions(tools);
        let turn = self.client.complete_with_prompt(&messages, &tools, prompt_tokens).await?;
        Ok(seal_response(response_from_turn(turn), &profile.model))
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
        let profile = self.profile();
        let messages = messages_from_transcript_replaying(
            &profile.system_prompt,
            transcript,
            profile.prompt_cache,
            &profile.model,
            profile.reasoning_field.as_deref(),
        );
        let tools = tools_from_definitions(tools);
        let turn = self
            .client
            .complete_streaming_with_prompt(&messages, &tools, sink, prompt_tokens)
            .await?;
        Ok(seal_response(response_from_turn(turn), &profile.model))
    }
}
