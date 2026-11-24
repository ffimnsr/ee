//! Anthropic Messages round trips over an authorized endpoint.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    ModelMessage, ModelResponse, ToolCallControl, ToolDefinition, estimate_prompt_tokens,
};
use ee_chat_completions::{
    DeltaSink, EndpointRequest, EndpointTransport, StreamDelta, drive_sse_with_chunk_timeout,
    emit_finished_deltas, is_event_stream, with_buffered_retry, with_stream_retry,
};
use serde_json::Value;

use super::{
    DEFAULT_MAX_OUTPUT_TOKENS, MessagesBody, StreamDecoder, messages_body, response_from_buffered,
};

/// One streaming Messages round trip over an authorized endpoint.
pub struct MessagesCodec {
    transport: EndpointTransport,
}

impl MessagesCodec {
    /// Wraps one authorized transport.
    #[must_use]
    pub fn new(transport: EndpointTransport) -> Self {
        Self { transport }
    }

    /// Returns the transport this codec sends through.
    #[must_use]
    pub fn transport(&self) -> &EndpointTransport {
        &self.transport
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
        self.complete_controlled(
            transcript,
            tools,
            &control,
            prompt_tokens,
            ee_chat_completions::CallOptions::default(),
        )
        .await
    }

    /// Performs one buffered round trip with tool-call control.
    pub(crate) async fn complete_controlled(
        &self,
        transcript: &[ModelMessage],
        tools: &[ToolDefinition],
        tool_control: &ToolCallControl,
        prompt_tokens: Option<u64>,
        options: ee_chat_completions::CallOptions<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let transport = self.transport.with_session(options.session)?;
        let profile = transport.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile
            .fitted_output_limit_with(
                prompt,
                options.generation.and_then(|generation| generation.max_output_tokens),
            )
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let extensions =
            ee_chat_completions::merged_extensions(profile.extensions.as_ref(), options.extensions);
        let body = messages_body(&MessagesBody {
            model: &profile.model,
            provider: Some(profile.label.as_str()),
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: false,
            max_output_tokens,
            extensions: extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
            prompt_tokens,
            generation: options.generation,
        });
        let transport = &transport;
        let label = profile.label.as_str();
        with_buffered_retry(&profile.retry, || async {
            let mut response = EndpointRequest::new(transport, &body).send().await?;
            let value = transport.read_json(&mut response, "response").await?;
            response_from_buffered(label, &value)
        })
        .await
        .map(|response| crate::dialect::finalize(response, profile))
    }

    /// Performs one streaming round trip under the profile retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet. Tool input
    /// accumulates privately, so nothing a retry could duplicate ever reached
    /// the caller, and no tool intent exists until the stream finishes.
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
        let transport = self.transport.with_session(options.session)?;
        let profile = transport.profile();
        let transcript = crate::dialect::guarded_transcript(profile, transcript);
        let prompt = prompt_tokens.unwrap_or_else(|| {
            estimate_prompt_tokens(transcript.as_ref(), tools, &profile.system_prompt)
        });
        let max_output_tokens = profile
            .fitted_output_limit_with(
                prompt,
                options.generation.and_then(|generation| generation.max_output_tokens),
            )
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let extensions =
            ee_chat_completions::merged_extensions(profile.extensions.as_ref(), options.extensions);
        let body = messages_body(&MessagesBody {
            model: &profile.model,
            provider: Some(profile.label.as_str()),
            system_prompt: &profile.system_prompt,
            transcript: transcript.as_ref(),
            tools,
            stream: true,
            max_output_tokens,
            extensions: extensions.as_ref(),
            tool_control,
            cache: &profile.cache,
            prompt_tokens,
            generation: options.generation,
        });
        let mut attempt = MessagesStreamAttempt { transport: &transport, body: &body };
        with_stream_retry(&transport.profile().retry, sink, &mut attempt)
            .await
            .map(|response| crate::dialect::finalize(response, profile))
    }
}

/// One real streaming attempt for [`MessagesCodec`].
struct MessagesStreamAttempt<'a> {
    transport: &'a EndpointTransport,
    body: &'a Value,
}

impl ee_chat_completions::StreamAttempt<ModelResponse> for MessagesStreamAttempt<'_> {
    fn run<'a>(
        &'a mut self,
        sink: ee_chat_completions::DeltaSink<'a>,
    ) -> ee_chat_completions::StreamFuture<'a, ModelResponse> {
        Box::pin(async move {
            let transport = self.transport;
            let label = transport.profile().label.clone();
            let model = transport.profile().model.clone();
            let mut response = EndpointRequest::new(transport, self.body).send().await?;
            if !is_event_stream(response.headers()) {
                let value = transport.read_json(&mut response, "streaming response").await?;
                let decoded = response_from_buffered(&label, &value)?;
                emit_finished_deltas(&decoded, sink)?;
                return Ok(ee_chat_completions::seal_response_with_provider(
                    decoded,
                    Some(&label),
                    &model,
                ));
            }
            let mut decoder = StreamDecoder::new(&label);
            let raw = transport.profile().raw_stream_events;
            drive_sse_with_chunk_timeout(
                &label,
                &mut response,
                Some(transport.profile().timeouts.chunk),
                &mut |event| {
                    if raw {
                        sink(StreamDelta::Raw { frame: event.to_string() })?;
                    }
                    decoder.apply(event, sink)
                },
            )
            .await?;
            decoder.finish().map(|response| {
                ee_chat_completions::seal_response_with_provider(response, Some(&label), &model)
            })
        })
    }
}
