//! Orchestrated OpenRouter mode: OpenRouter as a model adapter.
//!
//! [`OpenRouterModelAdapter`] implements
//! [`ModelAdapter`], so `ee-openrouter-agent` can run through
//! `ee_agent_orchestrator::OrchestratorProvider`:
//! the orchestrator owns the bounded model–tool loop, the task graph, memory,
//! budgets, and policy gates, while OpenRouter only answers chat-completions
//! round trips.
//!
//! The adapter is the shared `ee-chat-completions` adapter with an OpenRouter
//! profile, so the transcript, tool-schema, response, streaming, and retry
//! behavior is identical to every other OpenAI-compatible provider; only the
//! endpoint, attribution headers, reasoning shaping, and error wording are
//! OpenRouter's own.  The API key appears only in the Authorization header and
//! never in the transcript, memory, or logs.
//!
//! The HTTP round trip is behind an injected completion client so tests stay
//! network-free with scripted responses.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use ee_agent_orchestrator::{
    DEFAULT_MODEL_ID, ModelAdapter, ModelCapability, ModelError, ModelFamily, ModelFuture,
    ModelIdentity, ModelRegistration, ModelRequest, ModelResponse, ModelTier, OrchestratorConfig,
    OrchestratorProvider, OrchestratorProviderConfig, RUBBER_DUCK_ROLE, StreamSink,
};
use ee_agent_protocol::Implementation;
use ee_chat_completions::ChatCompletionsAdapter;
#[cfg(any(test, feature = "test-utils"))]
use ee_chat_completions::CompletionClient;
use tokio::sync::watch;

use crate::config::Config;
use crate::openrouter::{openrouter_profile, openrouter_token_source};

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
    let root = OpenRouterModelAdapter::with_http(config.clone(), http.clone())
        .map_err(|error| error.to_string())?;
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
    )
    .map_err(|error| format!("invalid critic model configuration: {error}"))?;
    registry
        .register_model(
            RUBBER_DUCK_ROLE,
            Arc::new(critic),
            ModelRegistration::new(identity).for_roles(&[RUBBER_DUCK_ROLE]).tier(ModelTier::Strong),
        )
        .map_err(|error| format!("invalid critic model route: {error}"))
}

/// OpenRouter as a normalized [`ModelAdapter`].
///
/// A thin wrapper over the shared Chat Completions adapter: the profile supplies
/// OpenRouter's endpoint, attribution headers, reasoning shaping, retry policy,
/// and error wording, while the shared adapter owns the protocol behavior.
pub struct OpenRouterModelAdapter(ChatCompletionsAdapter);

impl OpenRouterModelAdapter {
    /// Builds an adapter with a real HTTP client honoring `config.timeout`.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the configured endpoint or a header value is
    /// unusable, or when the HTTP client cannot be built.
    pub fn new(config: Config) -> Result<Self, String> {
        let profile = openrouter_profile(&config, &config.model)?;
        let adapter = ChatCompletionsAdapter::new(profile, openrouter_token_source(&config))?;
        Ok(Self(adapter))
    }

    /// Builds an adapter over an existing HTTP client (shared pools).
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the configured endpoint or a header value is
    /// unusable.
    fn with_http(config: Config, http: reqwest::Client) -> Result<Self, String> {
        let profile = openrouter_profile(&config, &config.model)?;
        Ok(Self(ChatCompletionsAdapter::with_http(profile, openrouter_token_source(&config), http)))
    }

    /// Builds an adapter with an injected completion client (tests).
    ///
    /// # Panics
    ///
    /// Panics when `config` carries an unusable endpoint or header value; test
    /// fixtures are expected to be valid configurations.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub(crate) fn with_completion(config: Config, completion: Arc<CompletionClient>) -> Self {
        let profile =
            openrouter_profile(&config, &config.model).expect("test OpenRouter profile builds");
        Self(ChatCompletionsAdapter::with_completion(
            profile,
            openrouter_token_source(&config),
            completion,
        ))
    }
}

impl ModelAdapter for OpenRouterModelAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        self.0.complete(request, cancel)
    }

    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        self.0.complete_streaming(request, cancel, events)
    }
}

/// Hermetic OpenRouter completion fixture for cross-crate integration tests.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_support {
    use std::collections::VecDeque;
    use std::future;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::ProviderError;
    use ee_chat_completions::{CompletionClient, decode_message};
    use serde_json::{Value, json};

    use super::{Config, OpenRouterModelAdapter};

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
        AdvanceClockThenStop(Duration),
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

        /// Like [`Self::pause_then_with_virtual_timeout`], but the model call whose
        /// deadline expires resolves with a plain assistant message instead of
        /// parking forever.
        ///
        /// The turn then continues on its own execution path: the next loop
        /// iteration's budget reservation compares the (advanced) virtual
        /// clock against the turn deadline and stops with
        /// `DeadlineExceeded`, which persists the recovery checkpoint. This
        /// avoids depending on the runtime's timer wheel waking a parked
        /// future — under shared-runtime contention that wake can be lost,
        /// leaving the turn blocked forever (`Running`) while the clock has
        /// in fact advanced (observed as a flaky pane test).
        #[must_use]
        pub fn pause_then_with_virtual_timeout_stop(
            responses: Vec<Value>,
            resume_responses: Vec<Value>,
            turn_timeout: Duration,
        ) -> Self {
            let mut steps = VecDeque::new();
            steps.push_back(ScriptStep::PauseClock);
            steps.extend(responses.into_iter().map(ScriptStep::Response));
            steps.push_back(ScriptStep::AdvanceClockThenStop(
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
                    ScriptStep::AdvanceClockThenStop(advance) => {
                        tokio::time::advance(advance).await;
                        // Resolve instead of parking: the owning turn must
                        // continue so its next budget reservation observes the
                        // advanced deadline (timer-wheel wake of a parked
                        // future is unreliable on the shared app runtime).
                        // `finish_reason: "length"` (not "stop") keeps
                        // `completed` false so the loop does not end the turn
                        // before the deadline check runs.
                        return json!({
                            "choices": [{
                                "message": { "content": "deadline reached" },
                                "finish_reason": "length"
                            }]
                        });
                    }
                }
            }
        }

        pub(crate) fn client(&self) -> Arc<CompletionClient> {
            let scripted = self.clone();
            Arc::new(move |messages, tools| {
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
                    decode_message(&response).ok_or_else(|| {
                        ProviderError::BackendFailure(
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
