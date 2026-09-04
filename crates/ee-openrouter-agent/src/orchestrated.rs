//! Orchestrated OpenRouter mode: OpenRouter as a model adapter.
//!
//! [`OpenRouterModelAdapter`] implements
//! [`ModelAdapter`], so `ee-openrouter-agent` can run through
//! `ee_agent_orchestrator::OrchestratorProvider`:
//! the orchestrator owns the bounded model–tool loop, the task graph, memory,
//! budgets, and policy gates, while OpenRouter only answers chat-completions
//! round trips.
//!
//! The transcript is converted to OpenRouter messages and the registry's
//! tool definitions to an OpenRouter function schema; text, reasoning, tool
//! calls, and the `finish_reason` completion signal map back onto the
//! normalized [`ModelResponse`].  The API key appears only in the
//! Authorization header and never in the transcript, memory, or logs.
//!
//! The HTTP round trip is behind a completion client so tests stay
//! network-free with scripted responses.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{
    DEFAULT_MODEL_ID, ModelAdapter, ModelCapability, ModelContent, ModelError, ModelFamily,
    ModelFuture, ModelIdentity, ModelMessage, ModelRegistration, ModelRequest, ModelResponse,
    ModelRole, ModelTier, ModelUsage, OrchestratorConfig, OrchestratorProvider,
    OrchestratorProviderConfig, RUBBER_DUCK_ROLE, StreamSink, ToolDefinition, ToolIntent,
};
use ee_agent_protocol::Implementation;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::Config;
#[cfg(test)]
use crate::openrouter::openrouter_request_body_with_tools;
use crate::openrouter::{
    OpenRouterMessage, OpenRouterStreamDelta, call_openrouter_streaming_with_retry,
    call_openrouter_with_retry,
};

mod policy;

pub use policy::openrouter_orchestrated_policy;

/// Builds the production configuration for an orchestrated OpenRouter ACP provider.
#[must_use]
pub fn openrouter_orchestrator_config(
    config: &Config,
    session_state_dir: PathBuf,
) -> OrchestratorProviderConfig {
    OrchestratorProviderConfig {
        implementation: Implementation::new("ee-openrouter-agent", env!("CARGO_PKG_VERSION"))
            .title("OpenRouter"),
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
            // Recovery remains same-process only until EE_CHECKPOINT_DIR supplies
            // explicit durable storage; never imply crash recovery without it.
            recovery: match config.checkpoint_dir.clone() {
                Some(directory) => ee_agent_orchestrator::RecoveryConfig::durable(directory),
                None => ee_agent_orchestrator::RecoveryConfig::memory_only(),
            },
            ..OrchestratorConfig::default()
        },
        session_state_dir: Some(session_state_dir),
        ..OrchestratorProviderConfig::default()
    }
}

/// Builds the production OpenRouter adapter and orchestrator policy combination.
///
/// The concrete [`OpenRouterModelAdapter`] parameter prevents generic test models
/// from being mistaken for the production OpenRouter configuration.
#[must_use]
pub fn openrouter_orchestrated_provider(
    config: &Config,
    session_state_dir: PathBuf,
    adapter: OpenRouterModelAdapter,
) -> OrchestratorProvider {
    OrchestratorProvider::with_policy(
        openrouter_orchestrator_config(config, session_state_dir),
        Arc::new(adapter),
        openrouter_orchestrated_policy(),
    )
}

/// Builds the production provider with a bounded test-only turn deadline.
///
/// Production construction always uses [`openrouter_orchestrated_provider`].
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn openrouter_orchestrated_provider_with_turn_timeout(
    config: &Config,
    session_state_dir: PathBuf,
    adapter: OpenRouterModelAdapter,
    turn_timeout: std::time::Duration,
) -> OrchestratorProvider {
    let mut provider_config = openrouter_orchestrator_config(config, session_state_dir);
    provider_config.orchestrator.turn_timeout = turn_timeout;
    OrchestratorProvider::with_policy(
        provider_config,
        Arc::new(adapter),
        openrouter_orchestrated_policy(),
    )
}

/// Production registry build. Invalid or unsafe critic metadata degrades to
/// root-only operation and returns one bounded, non-secret diagnostic.
pub fn openrouter_multi_model_provider(
    config: &Config,
    session_state_dir: PathBuf,
) -> Result<(OrchestratorProvider, Option<String>), String> {
    let http = reqwest::Client::builder()
        .timeout(config.timeout)
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?;
    let root = OpenRouterModelAdapter::with_http(config.clone(), http.clone());
    let root_family = config
        .model_family
        .as_deref()
        .map(ModelFamily::from_str)
        .transpose()
        .map_err(|error| format!("invalid OPENROUTER_MODEL_FAMILY: {error}"));

    let mut registry = ee_agent_orchestrator::ModelRegistry::new();
    let declared_root_family = root_family
        .as_ref()
        .ok()
        .and_then(Clone::clone)
        .unwrap_or_else(|| ModelFamily::Other("undeclared".into()));
    let root_identity = ModelIdentity::new(
        config.model.clone(),
        "openrouter",
        declared_root_family.clone(),
        config.model.clone(),
        [ModelCapability::ChatCompletion, ModelCapability::Tools, ModelCapability::Streaming],
    )
    .map_err(|error| error.to_string())?;
    registry
        .register_model(
            DEFAULT_MODEL_ID,
            Arc::new(root),
            ModelRegistration::new(root_identity).tier(ModelTier::Strong),
        )
        .map_err(|error| error.to_string())?;

    let critic_warning = match (
        config.rubber_duck_model.as_deref(),
        config.rubber_duck_model_family.as_deref(),
        root_family,
    ) {
        (None, None, _) => Some("rubber duck unavailable: no critic model configured".to_string()),
        (Some(_), None, _) | (None, Some(_), _) => Some(
            "rubber duck unavailable: OPENROUTER_RUBBER_DUCK_MODEL and OPENROUTER_RUBBER_DUCK_MODEL_FAMILY must be set together"
                .to_string(),
        ),
        (Some(_), Some(_), Err(error)) => Some(format!(
            "rubber duck unavailable: invalid root model family metadata: {error}"
        )),
        (Some(model_id), Some(family), Ok(Some(root_family))) => {
            match ModelFamily::from_str(family) {
                Err(error) => Some(format!(
                    "rubber duck unavailable: invalid critic model family metadata: {error}"
                )),
                Ok(_) if model_id == config.model => Some(
                    "rubber duck unavailable: critic model id must differ from root model id"
                        .to_string(),
                ),
                Ok(critic_family) if critic_family == root_family => Some(
                    "rubber duck unavailable: critic model family must differ from root model family"
                        .to_string(),
                ),
                Ok(critic_family) => register_openrouter_critic(
                    &mut registry,
                    config,
                    model_id,
                    critic_family,
                    http,
                )
                .err()
                .map(|error| format!("rubber duck unavailable: {error}")),
            }
        }
        (Some(_), Some(_), Ok(None)) => Some(
            "rubber duck unavailable: OPENROUTER_MODEL_FAMILY must be set explicitly"
                .to_string(),
        ),
    };

    let provider = OrchestratorProvider::with_model_registry(
        openrouter_orchestrator_config(config, session_state_dir),
        registry,
        openrouter_orchestrated_policy(),
    )
    .map_err(|error| error.to_string())?;
    Ok((provider, critic_warning))
}

fn register_openrouter_critic(
    registry: &mut ee_agent_orchestrator::ModelRegistry,
    config: &Config,
    model_id: &str,
    family: ModelFamily,
    http: reqwest::Client,
) -> Result<(), String> {
    let identity = ModelIdentity::new(
        model_id,
        "openrouter",
        family,
        model_id,
        [ModelCapability::ChatCompletion, ModelCapability::Tools, ModelCapability::Streaming],
    )
    .map_err(|error| format!("invalid critic model metadata: {error}"))?;
    let critic = OpenRouterModelAdapter::with_http(
        Config { model: model_id.to_string(), ..config.clone() },
        http,
    );
    registry
        .register_model(
            RUBBER_DUCK_ROLE,
            Arc::new(critic),
            ModelRegistration::new(identity).for_roles(&[RUBBER_DUCK_ROLE]).tier(ModelTier::Strong),
        )
        .map_err(|error| format!("invalid critic model route: {error}"))
}

/// Boxed future returned by a completion client.
pub(crate) type OpenRouterCompletionFuture =
    Pin<Box<dyn Future<Output = Result<OpenRouterMessage, ProviderError>> + Send + 'static>>;

/// One chat-completions round trip, abstracted so tests stay network-free.
///
/// Arguments: `(config, api_key, messages, tools)`; the real client sends the
/// request body built from those parts.
pub(crate) type OpenRouterCompletionClient =
    dyn Fn(&Config, &str, &[Value], &[Value]) -> OpenRouterCompletionFuture + Send + Sync;

/// One streaming OpenRouter chat-completions round trip.
pub(crate) type OpenRouterStreamingClient = dyn Fn(&Config, &str, &[Value], &[Value], StreamSink) -> OpenRouterCompletionFuture
    + Send
    + Sync;

/// OpenRouter as a normalized [`ModelAdapter`].
pub struct OpenRouterModelAdapter {
    config: Config,
    completion: Arc<OpenRouterCompletionClient>,
    streaming: Option<Arc<OpenRouterStreamingClient>>,
}

impl OpenRouterModelAdapter {
    /// Builds an adapter with a real HTTP client honoring `config.timeout`.
    pub fn new(config: Config) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self::with_http(config, http))
    }

    fn with_http(config: Config, http: reqwest::Client) -> Self {
        Self {
            config,
            completion: real_completion(http.clone()),
            streaming: Some(real_streaming(http)),
        }
    }

    /// Builds an adapter with an injected completion client (tests).
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub(crate) fn with_completion(
        config: Config,
        completion: Arc<OpenRouterCompletionClient>,
    ) -> Self {
        Self { config, completion, streaming: None }
    }
}

/// The real completion client: one OpenRouter chat-completions round trip.
fn real_completion(http: reqwest::Client) -> Arc<OpenRouterCompletionClient> {
    Arc::new(move |config, api_key, messages, tools| {
        let http = http.clone();
        let config = config.clone();
        let api_key = api_key.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            call_openrouter_with_retry(&http, &config, &api_key, &messages, &tools).await
        })
    })
}

fn real_streaming(http: reqwest::Client) -> Arc<OpenRouterStreamingClient> {
    Arc::new(move |config, api_key, messages, tools, events| {
        let http = http.clone();
        let config = config.clone();
        let api_key = api_key.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            let mut on_delta = |delta: OpenRouterStreamDelta| match delta {
                OpenRouterStreamDelta::Text(text) => events.text(text).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to forward OpenRouter text stream: {error}"
                    ))
                }),
                OpenRouterStreamDelta::Reasoning(text) => events.reasoning(text).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to forward OpenRouter reasoning stream: {error}"
                    ))
                }),
            };
            call_openrouter_streaming_with_retry(
                &http,
                &config,
                &api_key,
                &messages,
                &tools,
                &mut on_delta,
            )
            .await
        })
    })
}

impl ModelAdapter for OpenRouterModelAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let config = self.config.clone();
        let completion = self.completion.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let Some(api_key) = config.api_key.clone() else {
                return Err(ModelError::Adapter(
                    "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
                ));
            };
            let messages = openrouter_messages_from_transcript(&config, &request.transcript);
            let tools = openrouter_tools_from_definitions(&request.tools);
            let completion = completion(&config, &api_key, &messages, &tools);
            let answer = tokio::select! {
                answer = completion => answer.map_err(|error| ModelError::Adapter(error.to_string()))?,
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            Ok(model_response_from_openrouter(answer))
        })
    }

    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let Some(streaming) = self.streaming.clone() else {
            let completion = self.complete(request, cancel);
            return Box::pin(async move {
                let response = completion.await?;
                if let Some(reasoning) =
                    response.reasoning.as_deref().filter(|text| !text.is_empty())
                {
                    events.reasoning(reasoning.to_string())?;
                }
                if !response.text.is_empty() {
                    events.text(response.text.clone())?;
                }
                Ok(response)
            });
        };
        let config = self.config.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let Some(api_key) = config.api_key.clone() else {
                return Err(ModelError::Adapter(
                    "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
                ));
            };
            let messages = openrouter_messages_from_transcript(&config, &request.transcript);
            let tools = openrouter_tools_from_definitions(&request.tools);
            let completion = streaming(&config, &api_key, &messages, &tools, events);
            let answer = tokio::select! {
                answer = completion => answer.map_err(|error| ModelError::Adapter(error.to_string()))?,
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            Ok(model_response_from_openrouter(answer))
        })
    }
}

async fn wait_cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
}

/// Converts a normalized transcript into OpenRouter chat messages, prepending
/// the configured system prompt.  Tool observations carry their stable
/// tool-call id; subagent summaries map to user content.
pub(crate) fn openrouter_messages_from_transcript(
    config: &Config,
    transcript: &[ModelMessage],
) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": config.system_prompt })];
    for message in transcript {
        let role = match message.role {
            ModelRole::System => "system",
            ModelRole::User | ModelRole::Subagent => "user",
            ModelRole::Assistant => "assistant",
            ModelRole::Tool => "tool",
        };
        let content = message_content_text(&message.content);
        let entry = if role == "tool" {
            json!({
                "role": "tool",
                "tool_call_id": tool_call_id_of(&message.content),
                "content": content,
            })
        } else {
            json!({ "role": role, "content": content })
        };
        messages.push(entry);
    }
    messages
}

/// Renders one message's content blocks as text.
fn message_content_text(content: &[ModelContent]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => parts.push(text.clone()),
            ModelContent::ToolResult { result, .. } => parts.push(result.summary_text()),
            ModelContent::FileReference { path } => parts.push(format!("[file:{path}]")),
            ModelContent::TerminalReference { terminal_id } => {
                parts.push(format!("[terminal:{terminal_id}]"))
            }
            _ => {} // future content kinds stay out of the OpenRouter text view
        }
    }
    parts.join("\n")
}

/// Stable tool-call id of a tool observation message.
fn tool_call_id_of(content: &[ModelContent]) -> String {
    content
        .iter()
        .find_map(|block| match block {
            ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Converts normalized tool definitions into an OpenRouter function schema.
pub(crate) fn openrouter_tools_from_definitions(definitions: &[ToolDefinition]) -> Vec<Value> {
    definitions
        .iter()
        .map(|definition| {
            json!({
                "type": "function",
                "function": {
                    "name": definition.name,
                    "description": definition.description,
                    "parameters": definition.input_schema,
                }
            })
        })
        .collect()
}

/// Maps a model tool-call name onto the registry's tool name.  The historical
/// `tool_read_file` alias maps to the built-in `read_file` tool.
fn map_tool_name(name: &str) -> String {
    match name {
        "tool_read_file" => "read_file".to_string(),
        other => other.to_string(),
    }
}

/// Converts a decoded OpenRouter assistant message into a normalized
/// [`ModelResponse`]: text, reasoning, tool intents, and the completion
/// signal derived from `finish_reason`.
pub(crate) fn model_response_from_openrouter(answer: OpenRouterMessage) -> ModelResponse {
    let completed =
        answer.tool_calls.is_empty() && answer.finish_reason.as_deref().unwrap_or("stop") == "stop";
    let intents = answer
        .tool_calls
        .into_iter()
        .map(|call| ToolIntent::new(call.id, map_tool_name(&call.name), call.arguments))
        .collect();
    let openrouter_usage = answer.usage.unwrap_or_default();
    let mut usage = ModelUsage::new();
    usage.input_tokens =
        openrouter_usage.input_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    usage.output_tokens =
        openrouter_usage.output_tokens.and_then(|tokens| usize::try_from(tokens).ok());
    let mut response =
        ModelResponse::new().text(answer.content).tool_intents(intents).with_usage(usage);
    if !answer.reasoning.is_empty() {
        response = response.reasoning(answer.reasoning);
    }
    if completed {
        response = response.completed();
    }
    response
}

/// Builds the OpenRouter request body for a completion round (tests).
#[cfg(test)]
pub(crate) fn openrouter_body_for_request(
    config: &Config,
    transcript: &[ModelMessage],
    definitions: &[ToolDefinition],
) -> Value {
    openrouter_request_body_with_tools(
        config,
        &openrouter_messages_from_transcript(config, transcript),
        &openrouter_tools_from_definitions(definitions),
    )
}

/// Hermetic OpenRouter completion fixture for cross-crate integration tests.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_support {
    use std::collections::VecDeque;
    use std::future;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Config, OpenRouterCompletionClient, OpenRouterModelAdapter};

    #[derive(Clone)]
    enum Script {
        Steps(VecDeque<ScriptStep>),
        Never,
    }

    #[derive(Clone)]
    enum ScriptStep {
        PauseClock,
        Response(Value),
        Pending,
        AdvanceClockThenPending(Duration),
    }

    /// Replays canned OpenRouter response envelopes and records normalized requests.
    #[derive(Clone)]
    pub struct ScriptedOpenRouterCompletion {
        script: Arc<Mutex<Script>>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    impl ScriptedOpenRouterCompletion {
        /// Creates a finite response script. Each item is an OpenRouter response envelope.
        #[must_use]
        pub fn new(responses: Vec<Value>) -> Self {
            Self {
                script: Arc::new(Mutex::new(Script::Steps(
                    responses.into_iter().map(ScriptStep::Response).collect(),
                ))),
                bodies: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Creates a completion script that remains pending until cancellation.
        #[must_use]
        pub fn never() -> Self {
            Self {
                script: Arc::new(Mutex::new(Script::Never)),
                bodies: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Creates a script that pauses one model call, then serves resume responses.
        ///
        /// The pending call advances the script before it awaits forever, so a
        /// resumed turn deterministically consumes `resume_responses`.
        #[must_use]
        pub fn pause_then(responses: Vec<Value>, resume_responses: Vec<Value>) -> Self {
            let mut steps =
                responses.into_iter().map(ScriptStep::Response).collect::<VecDeque<_>>();
            steps.push_back(ScriptStep::Pending);
            steps.extend(resume_responses.into_iter().map(ScriptStep::Response));
            Self {
                script: Arc::new(Mutex::new(Script::Steps(steps))),
                bodies: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Creates a deterministic timeout script for a dedicated current-thread Tokio runtime.
        ///
        /// Runtime time pauses on the first model request, keeping setup and host approval outside
        /// the deadline. The pending model request then advances past `turn_timeout` before it
        /// awaits, so the owning turn times out only after that request has started.
        #[must_use]
        pub fn pause_then_with_virtual_timeout(
            responses: Vec<Value>,
            resume_responses: Vec<Value>,
            turn_timeout: Duration,
        ) -> Self {
            let mut steps = VecDeque::new();
            steps.push_back(ScriptStep::PauseClock);
            steps.extend(responses.into_iter().map(ScriptStep::Response));
            steps.push_back(ScriptStep::AdvanceClockThenPending(
                turn_timeout.saturating_add(Duration::from_nanos(1)),
            ));
            steps.extend(resume_responses.into_iter().map(ScriptStep::Response));
            Self {
                script: Arc::new(Mutex::new(Script::Steps(steps))),
                bodies: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Builds a concrete OpenRouter adapter backed by this scripted client.
        #[must_use]
        pub fn adapter(&self, config: Config) -> OpenRouterModelAdapter {
            OpenRouterModelAdapter::with_completion(config, self.client())
        }

        /// Returns normalized OpenRouter request bodies observed by this fixture.
        #[must_use]
        pub fn request_bodies(&self) -> Vec<Value> {
            self.bodies.lock().expect("bodies poisoned").clone()
        }

        #[cfg(test)]
        pub(crate) fn bodies(&self) -> Vec<Value> {
            self.request_bodies()
        }

        async fn next_response(&self) -> Value {
            loop {
                let step = match &mut *self.script.lock().expect("script poisoned") {
                    Script::Steps(steps) => steps.pop_front().unwrap_or_else(|| {
                        ScriptStep::Response(json!({
                            "choices": [{ "message": { "content": "" }, "finish_reason": "stop" }]
                        }))
                    }),
                    Script::Never => ScriptStep::Pending,
                };
                match step {
                    ScriptStep::PauseClock => tokio::time::pause(),
                    ScriptStep::Response(response) => return response,
                    ScriptStep::Pending => return future::pending().await,
                    ScriptStep::AdvanceClockThenPending(advance) => {
                        tokio::time::advance(advance).await;
                        return future::pending().await;
                    }
                }
            }
        }

        pub(crate) fn client(&self) -> Arc<OpenRouterCompletionClient> {
            let scripted = self.clone();
            Arc::new(move |_config, _api_key, messages, tools| {
                let scripted = scripted.clone();
                let messages = messages.to_vec();
                let tools = tools.to_vec();
                Box::pin(async move {
                    scripted
                        .bodies
                        .lock()
                        .expect("bodies poisoned")
                        .push(json!({ "messages": messages, "tools": tools }));
                    let response = scripted.next_response().await;
                    crate::openrouter::extract_openrouter_message(&response).ok_or_else(|| {
                        ee_acp_agent_server::ProviderError::BackendFailure(
                            "scripted OpenRouter response has no assistant message".into(),
                        )
                    })
                })
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn virtual_timeout_advances_only_at_pending_model_request() {
            let turn_timeout = Duration::from_secs(1);
            let initial = json!({ "stage": "initial" });
            let resumed = json!({ "stage": "resumed" });
            let scripted = ScriptedOpenRouterCompletion::pause_then_with_virtual_timeout(
                vec![initial.clone()],
                vec![resumed.clone()],
                turn_timeout,
            );

            assert_eq!(scripted.next_response().await, initial);
            let interrupted = tokio::time::timeout(turn_timeout, scripted.next_response()).await;
            assert!(interrupted.is_err(), "pending step must advance the virtual turn deadline");
            assert_eq!(scripted.next_response().await, resumed);
        }
    }
}

#[cfg(test)]
mod tests;
