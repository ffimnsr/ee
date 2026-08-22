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
use ee_agent_orchestrator::SensitiveDataGuard;
use ee_agent_protocol::{
    AgentCapabilities, ContentBlock, Implementation, PromptResponse, SessionId, SessionUpdate,
    StopReason, Usage, UsageUpdate, compact_available_command, is_compact_command,
    parse_slash_command,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::compaction::{
    build_compaction_prompt, messages_serialized_bytes, redact_message, retained_tail,
    trim_history_to_budget,
};
use crate::config::Config;
use crate::openrouter::{
    OpenRouterStreamDelta, OpenRouterUsage, call_openrouter, call_openrouter_streaming_with_retry,
    openrouter_tools,
};
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
    /// Input-token count OpenRouter reported for the last completed request.
    /// Unknown usage remains unknown and never triggers automatic compaction.
    pub(crate) last_input_tokens: Option<u64>,
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
                SessionData { cwd: Some(cwd), messages: Vec::new(), last_input_tokens: None },
            );
            Ok(SessionInit::new(session_id).commands(vec![compact_available_command()]))
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

/// One prompt turn: extract text, then either run the bounded OpenRouter
/// tool loop or the `/compact` summary path (agent-advertised slash
/// commands arrive as ordinary prompt text — the provider owns the
/// detection and the history replacement).
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
    if is_compact_command(&prompt_text) {
        let instructions =
            parse_slash_command(&prompt_text).and_then(|command| command.instructions);
        return run_compact(&turn, ctx, sink, cancel, instructions, false).await;
    }
    let session_key = ctx.session_id.to_string();
    let last_input_tokens = turn
        .sessions
        .lock()
        .expect("openrouter sessions poisoned")
        .get(&session_key)
        .and_then(|session| session.last_input_tokens);
    if should_auto_compact(&turn.config, last_input_tokens) {
        run_compact(&turn, ctx.clone(), sink.clone(), cancel.clone(), None, true).await?;
    }
    let Some(api_key) = turn.config.api_key.clone() else {
        return Err(ProviderError::BackendFailure(
            "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
        ));
    };

    let history = turn
        .sessions
        .lock()
        .expect("openrouter sessions poisoned")
        .get(&session_key)
        .map(|session| session.messages.clone());
    let mut messages =
        openrouter_messages(&turn.config, history.as_deref().unwrap_or_default(), &prompt_text);
    let mut pending_history = vec![json!({ "role": "user", "content": prompt_text })];
    // Per-turn token usage, aggregated across every model round of the tool
    // loop; unknown rounds are skipped, never counted as zero.
    let mut turn_usage = OpenRouterUsage::default();

    for round in 0..=MAX_TOOL_ROUNDS {
        if *cancel.borrow() {
            return Err(ProviderError::Cancellation);
        }
        let mut thought_message_id = None;
        let mut answer_message_id = None;
        let mut on_delta = |delta: OpenRouterStreamDelta| match delta {
            OpenRouterStreamDelta::Text(text) => {
                let message_id = answer_message_id
                    .get_or_insert_with(|| next_message_id(&turn.next_message, "message"));
                sink.agent_message_chunk(message_id.clone(), text).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to emit streamed message update: {error}"
                    ))
                })
            }
            OpenRouterStreamDelta::Reasoning(text) => {
                let message_id = thought_message_id
                    .get_or_insert_with(|| next_message_id(&turn.next_message, "thought"));
                sink.agent_thought_chunk(message_id.clone(), text).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "failed to emit streamed thought update: {error}"
                    ))
                })
            }
        };
        let answer = call_openrouter_streaming_with_retry(
            &turn.http,
            &turn.config,
            &api_key,
            &messages,
            &openrouter_tools(),
            &mut on_delta,
        )
        .await?;
        merge_openrouter_usage(&mut turn_usage, answer.usage);
        emit_context_usage(&sink, answer.usage, turn.config.context_window);

        if answer.tool_calls.is_empty() {
            pending_history.push(json!({ "role": "assistant", "content": answer.content }));
            if let Some(session) =
                turn.sessions.lock().expect("openrouter sessions poisoned").get_mut(&session_key)
            {
                session.messages.extend(pending_history);
                session.last_input_tokens = answer.usage.and_then(|usage| usage.input_tokens);
            }
            return Ok(prompt_response_with_usage(StopReason::EndTurn, turn_usage));
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

/// Runs one `/compact` turn: ask the configured model for a continuation
/// summary of the stored history, then replace the history with the summary
/// plus a pair-consistent recent tail.
///
/// Manual compaction is a no-op below [`Config::compact_min_messages`].
/// Automatic compaction bypasses that message-count guard because OpenRouter
/// already reported a near-limit context. The serialized history is trimmed
/// to [`Config::compact_max_input_bytes`] (oldest first, tool pairs kept
/// consistent), the model call carries no tools, and cancellation is observed
/// before and after the call. Every message sent to the model and every status
/// text emitted is redacted; an empty summary rejects without touching history.
async fn run_compact(
    turn: &PromptTurn,
    ctx: PromptContext,
    sink: UpdateSink,
    cancel: watch::Receiver<bool>,
    instructions: Option<String>,
    automatic: bool,
) -> Result<PromptResult, ProviderError> {
    let session_key = ctx.session_id.to_string();
    let history = turn
        .sessions
        .lock()
        .expect("openrouter sessions poisoned")
        .get(&session_key)
        .map(|session| session.messages.clone())
        .unwrap_or_default();
    let min_messages = turn.config.compact_min_messages;
    if !automatic && history.len() < min_messages {
        let message_id = next_message_id(&turn.next_message, "message");
        let text = format!(
            "Session history is small ({} message{}); no compaction needed. `/compact` runs once the history reaches {} messages.",
            history.len(),
            if history.len() == 1 { "" } else { "s" },
            min_messages,
        );
        sink.agent_message_chunk(message_id, SensitiveDataGuard::new().redact(&text)).map_err(
            |error| {
                ProviderError::BackendFailure(format!("failed to emit compaction notice: {error}"))
            },
        )?;
        return Ok(PromptResponse::new(StopReason::EndTurn));
    }
    let Some(api_key) = turn.config.api_key.clone() else {
        return Err(ProviderError::BackendFailure(
            "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
        ));
    };
    if *cancel.borrow() {
        return Err(ProviderError::Cancellation);
    }

    let mut history = history;
    let compaction_text = build_compaction_prompt(instructions.as_deref());
    // Bound the whole `messages` member of the request: system prompt and
    // compaction prompt stay, oldest history messages drop first.
    let overhead =
        messages_serialized_bytes(&openrouter_messages(&turn.config, &[], &compaction_text));
    let budget = turn.config.compact_max_input_bytes.saturating_sub(overhead);
    let trimmed = trim_history_to_budget(&mut history, budget);
    let messages = openrouter_messages(&turn.config, &history, &compaction_text)
        .into_iter()
        .map(|message| redact_message(&message))
        .collect::<Vec<_>>();

    // No tools during compaction; one bounded, cancellable round trip.
    let answer = call_openrouter(&turn.http, &turn.config, &api_key, &messages, &[]).await?;
    emit_context_usage(&sink, answer.usage, turn.config.context_window);
    if *cancel.borrow() {
        return Err(ProviderError::Cancellation);
    }
    let guard = SensitiveDataGuard::new();
    let summary = answer.content.trim();
    if summary.is_empty() {
        return Err(ProviderError::BackendFailure(
            "OpenRouter returned an empty compaction summary; history unchanged".into(),
        ));
    }

    // Replace provider-owned history with the summary plus a safe recent
    // tail; the system prompt is re-added per request, so it is not stored.
    let mut replacement = vec![
        json!({ "role": "user", "content": format!("Session summary:\n{}", guard.redact(summary)) }),
    ];
    let tail = retained_tail(&history, turn.config.compact_retained_tail);
    let tail_count = tail.len();
    replacement.extend(tail);
    let before_bytes = messages_serialized_bytes(&history);
    let after_bytes = messages_serialized_bytes(&replacement);
    if let Some(session) =
        turn.sessions.lock().expect("openrouter sessions poisoned").get_mut(&session_key)
    {
        session.messages = replacement;
        session.last_input_tokens = None;
    }

    let message_id = next_message_id(&turn.next_message, "message");
    let status = format!(
        "Session {}compacted: {} messages ({} bytes) -> summary + {tail_count} tail messages ({} bytes); trimmed {trimmed} oldest message(s) for the input bound.",
        if automatic { "automatically " } else { "" },
        history.len(),
        before_bytes,
        after_bytes,
    );
    sink.agent_message_chunk(message_id, guard.redact(&status)).map_err(|error| {
        ProviderError::BackendFailure(format!("failed to emit compaction status: {error}"))
    })?;
    Ok(prompt_response_with_usage(StopReason::EndTurn, answer.usage.unwrap_or_default()))
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

fn should_auto_compact(config: &Config, last_input_tokens: Option<u64>) -> bool {
    matches!(
        (config.auto_compact_threshold_tokens(), last_input_tokens),
        (Some(threshold), Some(input_tokens)) if input_tokens >= threshold
    )
}

/// Reports the current context-window usage through the ACP `usage_update`
/// notification: `used` is the round's `prompt_tokens` (the full context sent
/// to the model), `size` the configured window.  Unknown usage emits nothing.
fn emit_context_usage(sink: &UpdateSink, usage: Option<OpenRouterUsage>, context_window: u64) {
    if let Some(input_tokens) = usage.and_then(|usage| usage.input_tokens) {
        let _ = sink
            .raw_update(SessionUpdate::UsageUpdate(UsageUpdate::new(input_tokens, context_window)));
    }
}

/// Sums one round's usage into the turn aggregate, skipping unknown fields.
fn merge_openrouter_usage(target: &mut OpenRouterUsage, usage: Option<OpenRouterUsage>) {
    let Some(usage) = usage else { return };
    target.input_tokens = add_opt(target.input_tokens, usage.input_tokens);
    target.output_tokens = add_opt(target.output_tokens, usage.output_tokens);
    target.total_tokens = add_opt(target.total_tokens, usage.total_tokens);
}

fn add_opt(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

/// Maps round-trip usage to the ACP per-turn usage; the total falls back to
/// `input + output` when OpenRouter did not report it. Returns `None` when
/// neither input nor output tokens are known — unknown stays unknown, never
/// counted as zero.
fn to_sdk_usage(usage: OpenRouterUsage) -> Option<Usage> {
    let input_tokens = usage.input_tokens?;
    let output_tokens = usage.output_tokens?;
    let total_tokens =
        usage.total_tokens.unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    Some(Usage::new(total_tokens, input_tokens, output_tokens))
}

/// Builds a prompt response, attaching reported token usage when known.
fn prompt_response_with_usage(stop_reason: StopReason, usage: OpenRouterUsage) -> PromptResponse {
    let mut response = PromptResponse::new(stop_reason);
    if let Some(usage) = to_sdk_usage(usage) {
        response = response.usage(usage);
    }
    response
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
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
            auto_compact_threshold_percent: 80,
            retry_max_attempts: crate::config::DEFAULT_RETRY_MAX_ATTEMPTS,
            retry_base_delay: std::time::Duration::from_millis(
                crate::config::DEFAULT_RETRY_BASE_DELAY_MS,
            ),
            retry_max_delay: std::time::Duration::from_millis(
                crate::config::DEFAULT_RETRY_MAX_DELAY_MS,
            ),
            checkpoint_dir: None,
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
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

    #[test]
    fn turn_usage_aggregates_known_rounds_and_skips_unknown() {
        let mut usage = OpenRouterUsage::default();
        merge_openrouter_usage(&mut usage, None);
        assert_eq!(usage, OpenRouterUsage::default(), "unknown rounds change nothing");
        merge_openrouter_usage(
            &mut usage,
            Some(OpenRouterUsage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
            }),
        );
        merge_openrouter_usage(
            &mut usage,
            Some(OpenRouterUsage {
                input_tokens: None,
                output_tokens: Some(25),
                total_tokens: None,
            }),
        );
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(75));
        assert_eq!(usage.total_tokens, Some(150), "unknown total keeps prior total");
    }

    #[test]
    fn turn_usage_unknown_never_maps_to_zero_tokens() {
        assert_eq!(to_sdk_usage(OpenRouterUsage::default()), None);
        let partial =
            OpenRouterUsage { input_tokens: Some(10), output_tokens: None, total_tokens: None };
        assert_eq!(to_sdk_usage(partial), None, "half-known usage stays unknown");
    }

    #[test]
    fn turn_usage_maps_to_sdk_usage_with_fallback_total() {
        let usage = OpenRouterUsage {
            input_tokens: Some(6120),
            output_tokens: Some(2311),
            total_tokens: None,
        };
        let sdk = to_sdk_usage(usage).expect("fully known usage maps");
        assert_eq!(sdk.input_tokens, 6120);
        assert_eq!(sdk.output_tokens, 2311);
        assert_eq!(sdk.total_tokens, 8431, "total falls back to input + output");
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
    async fn compact_observes_cancellation_before_the_model_call() {
        use ee_acp_agent_server::{ClientBridge, UpdateSink};
        use ee_agent_protocol::TextContent;
        use tokio::sync::mpsc;

        let provider = OpenRouterProvider::new(test_config()).unwrap();
        let session_key = "openrouter-1";
        provider.sessions.lock().unwrap().insert(
            session_key.to_string(),
            SessionData {
                cwd: Some("/work".into()),
                messages: vec![
                    json!({ "role": "user", "content": "a" }),
                    json!({ "role": "assistant", "content": "b" }),
                    json!({ "role": "user", "content": "c" }),
                    json!({ "role": "assistant", "content": "d" }),
                ],
                last_input_tokens: None,
            },
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = UpdateSink::new_for_test(SessionId::new(session_key), tx.clone());
        let client = ClientBridge::new_for_test(Duration::from_secs(1), tx);
        let ctx = PromptContext::new(
            SessionId::new(session_key),
            vec![ContentBlock::Text(TextContent::new("/compact"))],
        );
        let (_cancel_tx, cancel_rx) = watch::channel(true);
        let turn = PromptTurn {
            config: test_config(),
            http: provider.http.clone(),
            sessions: provider.sessions.clone(),
            next_message: provider.next_message.clone(),
        };

        let error = run_prompt(turn, ctx, sink, client, cancel_rx).await.expect_err("cancelled");

        assert!(matches!(error, ProviderError::Cancellation), "{error:?}");
        assert!(rx.try_recv().is_err(), "no updates may be emitted when cancelled");
        // The stored history is untouched and no HTTP request happened.
        let session = provider.sessions.lock().unwrap().get(session_key).cloned().unwrap();
        assert_eq!(session.messages.len(), 4);
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
