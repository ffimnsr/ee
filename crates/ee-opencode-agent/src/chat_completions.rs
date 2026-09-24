//! OpenAI-compatible Chat Completions codec bound to one OpenCode route.
//!
//! The transport, SSE decoding, retry boundary, and normalization live in
//! `ee-chat-completions`, shared with OpenRouter; this module only binds an
//! authorized OpenCode endpoint to that shared client, so the third OpenCode
//! dialect has no private duplicate codec.

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{ModelMessage, ModelResponse, ToolDefinition};
use ee_chat_completions::{
    ChatCompletionsClient, DeltaSink, EndpointProfile, EndpointTransport, messages_from_transcript,
    response_from_turn, tools_from_definitions,
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
        let messages = messages_from_transcript(&self.profile().system_prompt, transcript);
        let tools = tools_from_definitions(tools);
        let turn = self.client.complete(&messages, &tools).await?;
        Ok(response_from_turn(turn))
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
        let messages = messages_from_transcript(&self.profile().system_prompt, transcript);
        let tools = tools_from_definitions(tools);
        let turn = self.client.complete_streaming(&messages, &tools, sink).await?;
        Ok(response_from_turn(turn))
    }
}
