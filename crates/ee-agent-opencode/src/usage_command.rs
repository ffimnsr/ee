//! OpenCode `/usage` slash command: ACP advertisement and prompt handling.
//!
//! ACP transports slash commands as ordinary `session/prompt` text, so this
//! provider wrapper intercepts the exact `/usage` command before the
//! orchestrator sees it and answers with a bounded text report. Only
//! `ee-agent-opencode` installs the wrapper, so other orchestrator-backed
//! agents never advertise the command.
//!
//! The orchestrator keeps the advertisement stable across its command-list
//! refreshes through `OrchestratorProviderConfig::additional_commands`; this
//! module owns command execution and formatting. The usage probe itself stays
//! explicit: no session startup path contacts the usage endpoints.

use std::collections::BTreeMap;
use std::sync::Arc;

use ee_agent_acp_server::{
    AgentProvider, ClientBridge, LoadSessionContext, LoginContext, NewSessionContext,
    PromptContext, PromptResult, ProviderError, ProviderFuture, SessionInit,
    SetConfigOptionContext, SetModeContext, SetModelContext, UpdateSink, env_or_dotenv,
};
use ee_agent_protocol::{
    AgentCapabilities, AvailableCommand, ContentBlock, EmbeddedResourceResource, Implementation,
    PromptResponse, SessionId, StopReason, parse_slash_command,
};
use tokio::sync::watch;

use crate::usage::{self, Budget, Meter, UsageProbe, UsageReport};

/// Advertised command name.
pub const USAGE_COMMAND_NAME: &str = "usage";
/// Message id used for the command's transcript reply.
const USAGE_MESSAGE_ID: &str = "usage";
/// Hard cap for the rendered report so one response cannot flood a transcript.
const MAX_USAGE_TEXT_BYTES: usize = 8 * 1024;
/// Cap for one provider-supplied field (ids, timestamps, notices).
const MAX_FIELD_CHARS: usize = 160;

/// The `/usage` command advertised to ACP clients.
#[must_use]
pub fn usage_available_command() -> AvailableCommand {
    AvailableCommand::new(
        USAGE_COMMAND_NAME,
        "Show OpenCode workspace budgets and Go rolling, weekly, and monthly usage.",
    )
}

/// Produces one usage report on demand.
///
/// Kept as a trait so tests can inject deterministic reports without network
/// access; the production implementation resolves the same environment,
/// `.env`, and config inputs as `--usage`.
pub trait UsageReporter: Send + Sync + 'static {
    /// Fetches the current usage report.
    fn report(&self) -> ProviderFuture<Result<UsageReport, String>>;
}

/// Production reporter backed by the resolved environment, `.env`, and agent
/// configuration file.
///
/// Constructing this reporter performs no network I/O; resolution happens once
/// when the provider is built.
#[derive(Clone)]
pub struct ConfigUsageReporter {
    probe: UsageProbe,
}

impl ConfigUsageReporter {
    /// Resolves the reporter with the same lookup order as `--usage`.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for a malformed agent configuration file.
    pub fn resolve(dotenv: &BTreeMap<String, String>) -> Result<Self, String> {
        let probe = UsageProbe::resolve(&mut |name| env_or_dotenv(name, dotenv))?;
        Ok(Self { probe })
    }
}

impl UsageReporter for ConfigUsageReporter {
    fn report(&self) -> ProviderFuture<Result<UsageReport, String>> {
        let probe = self.probe.clone();
        Box::pin(async move { usage::fetch_usage(&probe).await })
    }
}

/// Wraps an [`AgentProvider`] with the OpenCode `/usage` command.
///
/// Advertising happens on `session/new`, `session/load`, and `session/resume`;
/// prompts naming `/usage` are answered locally and never reach the inner
/// provider or the model.
pub struct UsageCommandProvider<P> {
    inner: P,
    reporter: Arc<dyn UsageReporter>,
}

impl<P> UsageCommandProvider<P> {
    /// Wraps a provider with the shared usage reporter.
    #[must_use]
    pub fn new(inner: P, reporter: Arc<dyn UsageReporter>) -> Self {
        Self { inner, reporter }
    }

    /// The wrapped provider, for provider-specific accessors in tests.
    #[must_use]
    pub fn inner(&self) -> &P {
        &self.inner
    }
}

impl<P: AgentProvider> AgentProvider for UsageCommandProvider<P> {
    fn info(&self) -> Implementation {
        self.inner.info()
    }

    fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities()
    }

    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let future = self.inner.new_session(ctx);
        Box::pin(async move { future.await.map(advertise_usage) })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let future = self.inner.load_session(ctx);
        Box::pin(async move { future.await.map(advertise_usage) })
    }

    fn resume_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let future = self.inner.resume_session(ctx);
        Box::pin(async move { future.await.map(advertise_usage) })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        if !is_usage_prompt(&ctx.prompt) {
            return self.inner.prompt(ctx, sink, client, cancel);
        }
        let reporter = Arc::clone(&self.reporter);
        Box::pin(async move { Ok(answer_usage(reporter, sink, cancel).await) })
    }

    fn set_mode(&self, ctx: SetModeContext) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.set_mode(ctx)
    }

    fn set_model(&self, ctx: SetModelContext) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.set_model(ctx)
    }

    fn set_config_option(
        &self,
        ctx: SetConfigOptionContext,
    ) -> ProviderFuture<Result<Vec<ee_agent_protocol::SessionConfigOption>, ProviderError>> {
        self.inner.set_config_option(ctx)
    }

    fn auth_methods(&self) -> Vec<ee_agent_protocol::v2::AuthMethod> {
        self.inner.auth_methods()
    }

    fn login(&self, ctx: LoginContext) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.login(ctx)
    }

    fn logout(&self) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.logout()
    }

    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.cancel_session(session_id)
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        self.inner.close_session(session_id)
    }
}

/// Adds `/usage` unless the inner provider already advertised it.
fn advertise_usage(mut init: SessionInit) -> SessionInit {
    if !init.commands.iter().any(|command| command.name == USAGE_COMMAND_NAME) {
        init.commands.push(usage_available_command());
    }
    init
}

/// Answers one `/usage` prompt without touching the inner provider.
async fn answer_usage(
    reporter: Arc<dyn UsageReporter>,
    sink: UpdateSink,
    mut cancel: watch::Receiver<bool>,
) -> PromptResult {
    let text = tokio::select! {
        result = reporter.report() => match result {
            Ok(report) => format_usage_report(&report),
            Err(error) => format_usage_error(&error),
        },
        () = wait_until_cancelled(&mut cancel) => {
            return PromptResponse::new(StopReason::Cancelled);
        }
    };
    let _ = sink.agent_message_chunk(USAGE_MESSAGE_ID, text);
    PromptResponse::new(StopReason::EndTurn)
}

/// Resolves only after the session's cancellation watch flips to `true`.
///
/// A dropped sender without a cancellation signal must not cancel the command;
/// it leaves this future pending so the usage fetch still decides the result.
async fn wait_until_cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|value| *value).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Whether the prompt starts with the exact `/usage` command.
fn is_usage_prompt(prompt: &[ContentBlock]) -> bool {
    let text = prompt.iter().filter_map(content_text).collect::<Vec<_>>().join(" ");
    parse_slash_command(&text).is_some_and(|command| command.name == USAGE_COMMAND_NAME)
}

fn content_text(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => Some(text.text.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// Renders one usage report as bounded, terminal-safe text.
#[must_use]
pub fn format_usage_report(report: &UsageReport) -> String {
    let mut out = BoundedLines::new(MAX_USAGE_TEXT_BYTES);
    out.push("OpenCode usage");
    if let Some(workspace) = report.workspace_id.as_deref() {
        out.push(&format!("workspace: {}", sanitize_field(workspace)));
    }
    out.push("");
    match &report.go {
        Some(go) => {
            out.push("Go allowance:");
            push_meter(&mut out, "rolling", &go.rolling);
            push_meter(&mut out, "weekly", &go.weekly);
            push_meter(&mut out, "monthly", &go.monthly);
        }
        None => out.push("Go allowance: not available"),
    }
    out.push("");
    if report.budgets.is_empty() {
        out.push("Budgets: none reported");
    } else {
        out.push("Budgets:");
        for budget in &report.budgets {
            out.push(&budget_line(budget));
            if out.truncated {
                break;
            }
        }
    }
    if report.go.is_none() && report.budgets.is_empty() {
        out.push("");
        out.push("No workspace budgets or Go allowance were reported.");
    }
    if !report.notices.is_empty() {
        out.push("");
        out.push("Notices:");
        for notice in &report.notices {
            out.push(&format!("- {}", sanitize_field(notice)));
            if out.truncated {
                break;
            }
        }
    }
    out.finish()
}

/// Renders one usage-probe diagnostic as bounded, terminal-safe text.
#[must_use]
pub fn format_usage_error(error: &str) -> String {
    format!("OpenCode usage unavailable: {}", sanitize_field(error))
}

fn push_meter(out: &mut BoundedLines, label: &str, meter: &Meter) {
    let mut line = format!("  {label}: {:.1}%", meter.percent);
    if meter.limited {
        line.push_str(" (rate-limited)");
    }
    if let Some(resets) = meter.resets_at.as_deref() {
        line.push_str(" resets ");
        line.push_str(&sanitize_field(resets));
    }
    out.push(&line);
}

fn budget_line(budget: &Budget) -> String {
    let mut line = format!("  {}", sanitize_field(&budget.id));
    if let Some(window) = budget.per.as_deref().or(budget.scope.as_deref()) {
        line.push_str(" [");
        line.push_str(&sanitize_field(window));
        line.push(']');
    }
    let mut flags = Vec::new();
    if budget.picked {
        flags.push("picked");
    }
    if budget.shared {
        flags.push("shared");
    }
    if !flags.is_empty() {
        line.push_str(" (");
        line.push_str(&flags.join(", "));
        line.push(')');
    }
    line.push_str(&format!(" ${:.2} of ${:.2}", budget.spent_usd, budget.limit_usd));
    if let Some(resets) = budget.resets_at.as_deref() {
        line.push_str(" resets ");
        line.push_str(&sanitize_field(resets));
    }
    line
}

/// Single-line, control-character-free provider data with a hard char cap.
fn sanitize_field(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    for _ in 0..MAX_FIELD_CHARS {
        match chars.next() {
            Some(character) if character.is_control() => out.push(' '),
            Some(character) => out.push(character),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

/// Line accumulator that never exceeds its byte budget.
struct BoundedLines {
    text: String,
    limit: usize,
    truncated: bool,
}

impl BoundedLines {
    fn new(limit: usize) -> Self {
        Self { text: String::new(), limit, truncated: false }
    }

    fn push(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        let separator = if self.text.is_empty() { 0 } else { 1 };
        let required = self.text.len().saturating_add(separator).saturating_add(line.len());
        if required > self.limit.saturating_sub(32) {
            self.truncated = true;
            return;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(line);
    }

    fn finish(mut self) -> String {
        if self.truncated {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str("... (truncated)");
        }
        self.text
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use ee_agent_acp_server::server::OutboundEvent;
    use ee_agent_protocol::{SessionUpdate, TextContent};
    use tokio::sync::mpsc;

    use super::*;
    use crate::usage::GoUsage;

    struct StubProvider {
        prompts: Arc<AtomicUsize>,
    }

    impl StubProvider {
        fn new() -> Self {
            Self { prompts: Arc::new(AtomicUsize::new(0)) }
        }
    }

    impl AgentProvider for StubProvider {
        fn info(&self) -> Implementation {
            Implementation::new("stub", "0.0.0")
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::default()
        }

        fn new_session(
            &self,
            _ctx: NewSessionContext,
        ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
            Box::pin(async { Ok(SessionInit::new("session-1")) })
        }

        fn load_session(
            &self,
            _ctx: LoadSessionContext,
        ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
            Box::pin(async { Ok(SessionInit::new("session-1")) })
        }

        fn prompt(
            &self,
            _ctx: PromptContext,
            _sink: UpdateSink,
            _client: ClientBridge,
            _cancel: watch::Receiver<bool>,
        ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
            self.prompts.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(PromptResponse::new(StopReason::EndTurn)) })
        }

        fn cancel_session(
            &self,
            _session_id: SessionId,
        ) -> ProviderFuture<Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }

        fn close_session(
            &self,
            _session_id: SessionId,
        ) -> ProviderFuture<Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct FixedReporter {
        result: Result<UsageReport, String>,
    }

    impl UsageReporter for FixedReporter {
        fn report(&self) -> ProviderFuture<Result<UsageReport, String>> {
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    struct PendingReporter;

    impl UsageReporter for PendingReporter {
        fn report(&self) -> ProviderFuture<Result<UsageReport, String>> {
            Box::pin(std::future::pending::<Result<UsageReport, String>>())
        }
    }

    fn plumbing(
        session_id: &str,
    ) -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new(session_id);
        (
            UpdateSink::new_for_test(session_id.clone(), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn prompt_context(text: &str) -> PromptContext {
        PromptContext::new(
            SessionId::new("session-1"),
            vec![ContentBlock::Text(TextContent::new(text))],
        )
    }

    fn sample_report() -> UsageReport {
        UsageReport {
            workspace_id: Some(String::from("org-1")),
            budgets: vec![Budget {
                id: String::from("rule-a"),
                scope: Some(String::from("workspace")),
                per: Some(String::from("week")),
                limit_usd: 10.0,
                spent_usd: 1.25,
                resets_at: Some(String::from("2026-12-01T00:00:00Z")),
                picked: true,
                shared: false,
                vendors: Vec::new(),
                models: Vec::new(),
            }],
            go: Some(GoUsage {
                rolling: Meter {
                    percent: 42.5,
                    limited: false,
                    resets_at: Some(String::from("2026-11-26T12:00:00Z")),
                },
                weekly: Meter {
                    percent: 100.0,
                    limited: true,
                    resets_at: Some(String::from("2026-11-28T00:00:00Z")),
                },
                monthly: Meter { percent: 0.0, limited: false, resets_at: None },
            }),
            notices: Vec::new(),
        }
    }

    fn reply_text(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> String {
        let mut text = String::new();
        while let Ok(event) = rx.try_recv() {
            if let OutboundEvent::Update { update, .. } = event
                && let SessionUpdate::AgentMessageChunk(chunk) = *update
                && let ContentBlock::Text(content) = chunk.content
            {
                text.push_str(&content.text);
            }
        }
        text
    }

    #[test]
    fn command_carries_name_and_description() {
        let command = usage_available_command();
        assert_eq!(command.name, USAGE_COMMAND_NAME);
        assert!(!command.description.is_empty());
        assert!(command.input.is_none());
    }

    #[tokio::test]
    async fn new_load_and_resume_sessions_advertise_usage() {
        let provider = UsageCommandProvider::new(
            StubProvider::new(),
            Arc::new(FixedReporter { result: Ok(sample_report()) }),
        );
        let cwd = std::env::temp_dir();

        let created =
            provider.new_session(NewSessionContext::new(cwd.clone())).await.expect("new session");
        let loaded = provider
            .load_session(LoadSessionContext::new("session-1", cwd.clone()))
            .await
            .expect("load session");
        let resumed = provider
            .resume_session(LoadSessionContext::new("session-1", cwd))
            .await
            .expect("resume session");

        for init in [created, loaded, resumed] {
            assert_eq!(
                init.commands.iter().filter(|command| command.name == USAGE_COMMAND_NAME).count(),
                1,
                "usage advertised exactly once"
            );
        }
    }

    #[tokio::test]
    async fn usage_prompt_is_answered_without_reaching_the_inner_provider() {
        let provider = UsageCommandProvider::new(
            StubProvider::new(),
            Arc::new(FixedReporter { result: Ok(sample_report()) }),
        );
        let (sink, client, mut rx) = plumbing("session-1");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = provider
            .prompt(prompt_context("/usage"), sink, client, cancel_rx)
            .await
            .expect("usage prompt");

        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(provider.inner().prompts.load(Ordering::Relaxed), 0);
        let text = reply_text(&mut rx);
        assert!(text.contains("rolling: 42.5%"), "{text}");
        assert!(text.contains("weekly: 100.0% (rate-limited)"), "{text}");
        assert!(text.contains("monthly: 0.0%"), "{text}");
    }

    #[tokio::test]
    async fn usage_prompt_reports_a_bounded_error() {
        let provider = UsageCommandProvider::new(
            StubProvider::new(),
            Arc::new(FixedReporter { result: Err(String::from("probe failed\u{1b}[31m")) }),
        );
        let (sink, client, mut rx) = plumbing("session-1");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = provider
            .prompt(prompt_context("/usage"), sink, client, cancel_rx)
            .await
            .expect("usage prompt");

        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let text = reply_text(&mut rx);
        assert!(text.starts_with("OpenCode usage unavailable: "), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[tokio::test]
    async fn usage_prompt_honors_cancellation() {
        let provider = UsageCommandProvider::new(StubProvider::new(), Arc::new(PendingReporter));
        let (sink, client, _rx) = plumbing("session-1");
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let result = provider
            .prompt(prompt_context("/usage"), sink, client, cancel_rx)
            .await
            .expect("cancelled prompt");

        assert_eq!(result.stop_reason, StopReason::Cancelled);
        assert_eq!(provider.inner().prompts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn ordinary_prompts_reach_the_inner_provider() {
        let provider = UsageCommandProvider::new(
            StubProvider::new(),
            Arc::new(FixedReporter { result: Ok(sample_report()) }),
        );
        let (sink, client, _rx) = plumbing("session-1");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let result = provider
            .prompt(prompt_context("show me the usage"), sink, client, cancel_rx)
            .await
            .expect("ordinary prompt");

        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(provider.inner().prompts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn report_formats_meters_and_budgets() {
        let text = format_usage_report(&sample_report());
        assert!(text.contains("workspace: org-1"), "{text}");
        assert!(text.contains("rolling: 42.5%"), "{text}");
        assert!(text.contains("weekly: 100.0% (rate-limited)"), "{text}");
        assert!(text.contains("monthly: 0.0%"), "{text}");
        assert!(text.contains("rule-a [week] (picked) $1.25 of $10.00"), "{text}");
        assert!(text.contains("resets 2026-12-01T00:00:00Z"), "{text}");
    }

    #[test]
    fn report_marks_absent_go_allowance() {
        let mut report = sample_report();
        report.go = None;
        report.budgets.clear();
        let text = format_usage_report(&report);
        assert!(text.contains("Go allowance: not available"), "{text}");
        assert!(text.contains("No workspace budgets or Go allowance were reported."), "{text}");
    }

    #[test]
    fn report_sanitizes_control_characters() {
        let mut report = sample_report();
        report.budgets[0].id = String::from("bad\u{1b}[31m");
        report.notices = vec![String::from("notice\u{7}")];
        let text = format_usage_report(&report);
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(!text.contains('\u{7}'), "{text}");
        assert!(text.contains("bad [31m"), "{text}");
    }

    #[test]
    fn report_stays_within_its_byte_budget() {
        let mut report = sample_report();
        report.budgets = (0..500)
            .map(|index| Budget {
                id: format!("rule-{index}-{}", "x".repeat(MAX_FIELD_CHARS)),
                scope: None,
                per: Some(String::from("week")),
                limit_usd: 1.0,
                spent_usd: 0.5,
                resets_at: None,
                picked: false,
                shared: true,
                vendors: Vec::new(),
                models: Vec::new(),
            })
            .collect();
        let text = format_usage_report(&report);
        assert!(text.len() <= MAX_USAGE_TEXT_BYTES, "{} bytes", text.len());
        assert!(text.ends_with("(truncated)"), "report should be truncated");
    }
}
