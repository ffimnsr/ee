//! OpenRouter provider implementation.
//!
//! [`OpenRouterProvider`] implements the framework's [`AgentProvider`] trait:
//! session history lives behind a mutex, prompt turns run the bounded
//! OpenRouter tool loop, reasoning and answers stream through the
//! [`UpdateSink`], and file reads go through the framework's
//! [`ClientBridge`].  Protocol handling, sessions, updates, and errors are
//! all owned by the framework — this module holds no JSON-RPC or
//! stdin/stdout code.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{
    AgentProvider, ClientBridge, LoadSessionContext, NewSessionContext, PromptContext,
    PromptResult, ProviderError, ProviderFuture, SessionIdGenerator, SessionInit, UpdateSink,
};
use ee_agent_protocol::{
    AgentCapabilities, ContentBlock, Implementation, PromptResponse, SessionId, StopReason,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::Config;
use crate::openrouter::{call_openrouter, openrouter_tools};
use crate::tools::handle_tool_call;

/// Maximum number of model tool rounds in one prompt turn.
pub const MAX_TOOL_ROUNDS: usize = 6;

/// Per-session provider state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionData {
    /// Absolute session working directory.
    pub(crate) cwd: Option<String>,
    /// Message history fed to the OpenRouter request body.
    pub(crate) messages: Vec<Value>,
}

/// Owned state for one prompt turn (cloned out of the provider so the boxed
/// future is `'static`).
struct PromptTurn {
    config: Config,
    http: reqwest::Client,
    sessions: Arc<Mutex<BTreeMap<String, SessionData>>>,
    next_message: Arc<AtomicU64>,
}

/// OpenRouter agent: configuration plus per-session history.
///
/// All mutable state lives behind [`Arc`]s so the provider stays
/// `Send + Sync` and prompt turns can own their state independently.
pub struct OpenRouterProvider {
    config: Config,
    http: reqwest::Client,
    sessions: Arc<Mutex<BTreeMap<String, SessionData>>>,
    /// Monotonic session-id generator (framework-owned, `openrouter-N`).
    ids: Arc<Mutex<SessionIdGenerator>>,
    /// Monotonic update message-id counter.
    next_message: Arc<AtomicU64>,
}

impl OpenRouterProvider {
    /// Builds a provider with an HTTP client honoring `config.timeout`.
    pub fn new(config: Config) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self {
            config,
            http,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            ids: Arc::new(Mutex::new(SessionIdGenerator::new("openrouter"))),
            next_message: Arc::new(AtomicU64::new(1)),
        })
    }
}

impl AgentProvider for OpenRouterProvider {
    fn info(&self) -> Implementation {
        Implementation::new("ee-openrouter-agent", env!("CARGO_PKG_VERSION")).title("OpenRouter")
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let cwd = ctx.cwd.to_string_lossy().to_string();
        let session_id = self.ids.lock().expect("provider session ids poisoned").next_id();
        let sessions = self.sessions.clone();
        Box::pin(async move {
            sessions.lock().expect("openrouter sessions poisoned").insert(
                session_id.to_string(),
                SessionData { cwd: Some(cwd), messages: Vec::new() },
            );
            Ok(SessionInit::new(session_id))
        })
    }

    fn load_session(
        &self,
        _ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::InvalidRequest(
                "session loading is not supported by ee-openrouter-agent".into(),
            ))
        })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        let turn = PromptTurn {
            config: self.config.clone(),
            http: self.http.clone(),
            sessions: self.sessions.clone(),
            next_message: self.next_message.clone(),
        };
        Box::pin(async move { run_prompt(turn, ctx, sink, client, cancel).await })
    }

    fn cancel_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        // The framework flips the prompt's cancellation signal; there is no
        // per-session provider state to cancel here.
        Box::pin(async { Ok(()) })
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let sessions = self.sessions.clone();
        Box::pin(async move {
            sessions.lock().expect("openrouter sessions poisoned").remove(&session_id.to_string());
            Ok(())
        })
    }
}

/// One prompt turn: extract text, then run the bounded OpenRouter tool loop.
///
/// Reasoning streams as thought chunks, the final answer as message chunks,
/// and file reads execute through the [`ClientBridge`] with in-progress /
/// completed / failed tool updates.  Cancellation is observed between
/// rounds.
async fn run_prompt(
    turn: PromptTurn,
    ctx: PromptContext,
    sink: UpdateSink,
    client: ClientBridge,
    cancel: watch::Receiver<bool>,
) -> Result<PromptResult, ProviderError> {
    let prompt_text = extract_prompt_text(&ctx.prompt);
    if prompt_text.trim().is_empty() {
        return Err(ProviderError::InvalidRequest("prompt has no text content".into()));
    }
    let Some(api_key) = turn.config.api_key.clone() else {
        return Err(ProviderError::BackendFailure(
            "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
        ));
    };

    let session_key = ctx.session_id.to_string();
    let history = turn
        .sessions
        .lock()
        .expect("openrouter sessions poisoned")
        .get(&session_key)
        .map(|session| session.messages.clone());
    let mut messages =
        openrouter_messages(&turn.config, history.as_deref().unwrap_or_default(), &prompt_text);
    let mut pending_history = vec![json!({ "role": "user", "content": prompt_text })];

    for round in 0..=MAX_TOOL_ROUNDS {
        if *cancel.borrow() {
            return Err(ProviderError::Cancellation);
        }
        let answer =
            call_openrouter(&turn.http, &turn.config, &api_key, &messages, &openrouter_tools())
                .await?;

        if !answer.reasoning.is_empty() {
            let message_id = next_message_id(&turn.next_message, "thought");
            sink.agent_thought_chunk(message_id, &answer.reasoning).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to emit thought update: {error}"))
            })?;
        }

        if answer.tool_calls.is_empty() {
            if !answer.content.is_empty() {
                let message_id = next_message_id(&turn.next_message, "message");
                sink.agent_message_chunk(message_id, &answer.content).map_err(|error| {
                    ProviderError::BackendFailure(format!("failed to emit message update: {error}"))
                })?;
            }
            pending_history.push(json!({ "role": "assistant", "content": answer.content }));
            if let Some(session) =
                turn.sessions.lock().expect("openrouter sessions poisoned").get_mut(&session_key)
            {
                session.messages.extend(pending_history);
            }
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }

        if round == MAX_TOOL_ROUNDS {
            return Err(ProviderError::BackendFailure(
                "OpenRouter tool loop exceeded maximum rounds".into(),
            ));
        }

        messages.push(answer.raw.clone());
        pending_history.push(answer.raw);
        let cwd = turn
            .sessions
            .lock()
            .expect("openrouter sessions poisoned")
            .get(&session_key)
            .and_then(|session| session.cwd.clone());
        for tool_call in answer.tool_calls {
            let result =
                handle_tool_call(&ctx.session_id, cwd.as_deref(), &tool_call, &sink, &client).await;
            let tool_message = json!({
                "role": "tool",
                "tool_call_id": tool_call.id,
                "content": result,
            });
            messages.push(tool_message.clone());
            pending_history.push(tool_message);
        }
    }
    unreachable!("tool loop returns inside bounded range")
}

/// Builds the conversation for one round: system prompt, stored history, and
/// the current user prompt.
fn openrouter_messages(config: &Config, history: &[Value], prompt_text: &str) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": config.system_prompt })];
    messages.extend_from_slice(history);
    messages.push(json!({ "role": "user", "content": prompt_text }));
    messages
}

/// Concatenates the text content blocks of a prompt.
pub(crate) fn extract_prompt_text(prompt: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in prompt {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    parts.join("\n")
}

fn next_message_id(next: &AtomicU64, kind: &str) -> String {
    format!("openrouter-{kind}-{}", next.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::TextContent;
    use std::path::PathBuf;
    use std::time::Duration;

    fn test_config() -> Config {
        Config {
            model: String::from("test/model"),
            api_url: String::from("http://localhost:1/v1/chat/completions"),
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated: false,
        }
    }

    fn session_ctx(cwd: &str) -> NewSessionContext {
        NewSessionContext::new(PathBuf::from(cwd))
    }

    #[test]
    fn extracts_text_prompt_blocks() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::Text(TextContent::new("world")),
        ];

        assert_eq!(extract_prompt_text(&prompt), "hello\nworld");
    }

    #[test]
    fn non_text_blocks_and_empty_prompts_yield_empty_text() {
        assert_eq!(extract_prompt_text(&[ContentBlock::Text(TextContent::new("keep"))]), "keep");
        assert_eq!(extract_prompt_text(&[]), "");
    }

    #[tokio::test]
    async fn new_session_stores_cwd_and_generates_openrouter_ids() {
        let provider = OpenRouterProvider::new(test_config()).unwrap();

        let init = provider.new_session(session_ctx("/work")).await.unwrap();

        assert_eq!(init.session_id.to_string(), "openrouter-1");
        let session = provider.sessions.lock().unwrap().get("openrouter-1").cloned().unwrap();
        assert_eq!(session.cwd.as_deref(), Some("/work"));
        assert!(session.messages.is_empty());
    }

    #[tokio::test]
    async fn session_ids_are_monotonic() {
        let provider = OpenRouterProvider::new(test_config()).unwrap();

        let a = provider.new_session(session_ctx("/a")).await.unwrap();
        let b = provider.new_session(session_ctx("/b")).await.unwrap();

        assert_eq!(a.session_id.to_string(), "openrouter-1");
        assert_eq!(b.session_id.to_string(), "openrouter-2");
    }

    #[tokio::test]
    async fn load_session_is_unsupported() {
        let provider = OpenRouterProvider::new(test_config()).unwrap();
        let ctx = LoadSessionContext::new(SessionId::new("openrouter-1"), PathBuf::from("/work"));

        let error = provider.load_session(ctx).await.unwrap_err();

        assert!(
            matches!(&error, ProviderError::InvalidRequest(message)
                if message.contains("session loading is not supported")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn close_session_removes_message_history() {
        let provider = OpenRouterProvider::new(test_config()).unwrap();
        let init = provider.new_session(session_ctx("/work")).await.unwrap();
        provider
            .sessions
            .lock()
            .unwrap()
            .get_mut("openrouter-1")
            .unwrap()
            .messages
            .push(json!({ "role": "user", "content": "hi" }));

        provider.close_session(init.session_id).await.unwrap();

        assert!(!provider.sessions.lock().unwrap().contains_key("openrouter-1"));
    }
}
