//! Classified retries for transient tool failures.
//!
//! Timeout and backend failures (which cover rate limits and temporary I/O
//! errors surfaced through the client bridge) are transient and may be
//! retried under a small budget; invalid arguments, policy denials,
//! permission denials, and cancellation are permanent and are never retried.
//! [`ToolRetrier`] wraps a [`ToolExecutor`] and re-executes a failed intent
//! up to [`RetryPolicy::max_retries`] times, sleeping an exponentially
//! growing, capped backoff between attempts on tokio's testable clock.

use std::collections::HashSet;
use std::time::Duration;

use ee_acp_agent_server::{ClientBridge, UpdateSink};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::error::OrchestratorError;
use crate::model::ModelMessage;
use crate::tasks::TaskNode;
use crate::tools::{ToolErrorKind, ToolExecutor, ToolIntent, ToolResult};

/// Whether an error kind may recover on a retried attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RetryErrorClass {
    /// Rate limits, timeouts, and temporary I/O failures: retryable.
    Transient,
    /// Invalid arguments, policy/permission denials, cancellation: never
    /// retried.
    Permanent,
}

impl RetryErrorClass {
    /// Stable lowercase name for diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }
}

/// Classifies one tool error kind for retry decisions.
#[must_use]
pub fn classify_tool_error(kind: ToolErrorKind) -> RetryErrorClass {
    match kind {
        ToolErrorKind::Timeout | ToolErrorKind::Backend => RetryErrorClass::Transient,
        ToolErrorKind::InvalidArguments
        | ToolErrorKind::PermissionDenied
        | ToolErrorKind::Cancelled => RetryErrorClass::Permanent,
    }
}

/// Exponential backoff with a hard cap, driven by tokio's clock so tests can
/// pause and advance time deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackoffStrategy {
    /// Delay before the first retry (attempt 0).
    pub base_delay: Duration,
    /// Hard cap on any single delay.
    pub max_delay: Duration,
    /// Multiplier applied to the delay after every retry.
    pub multiplier: u32,
}

impl Default for BackoffStrategy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            multiplier: 2,
        }
    }
}

impl BackoffStrategy {
    /// Creates a strategy with the given base, cap, and multiplier.
    #[must_use]
    pub fn new(base_delay: Duration, max_delay: Duration, multiplier: u32) -> Self {
        Self { base_delay, max_delay, multiplier }
    }

    /// Delay before the attempt-th retry: `base * multiplier^attempt`,
    /// capped at `max_delay` (saturating, never overflowing).
    #[must_use]
    pub fn delay_for(&self, attempt: usize) -> Duration {
        let mut delay = self.base_delay;
        for _ in 0..attempt {
            match delay.checked_mul(self.multiplier) {
                Some(next) => delay = next,
                None => return self.max_delay,
            }
            if delay >= self.max_delay {
                return self.max_delay;
            }
        }
        delay.min(self.max_delay)
    }

    /// Sleeps for the delay before the attempt-th retry.
    pub async fn sleep_for(&self, attempt: usize) {
        tokio::time::sleep(self.delay_for(attempt)).await;
    }
}

/// Retry budget and classification for one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum retries after the first attempt; `0` disables retries.
    pub max_retries: usize,
    /// Error kinds considered transient and therefore retryable.
    pub transient_error_kinds: HashSet<ToolErrorKind>,
    /// Backoff applied between attempts.
    pub backoff: BackoffStrategy,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(2, BackoffStrategy::default())
    }
}

impl RetryPolicy {
    /// Creates a policy retrying timeouts and backend failures up to
    /// `max_retries` times with the given backoff.
    #[must_use]
    pub fn new(max_retries: usize, backoff: BackoffStrategy) -> Self {
        Self {
            max_retries,
            transient_error_kinds: HashSet::from([ToolErrorKind::Timeout, ToolErrorKind::Backend]),
            backoff,
        }
    }

    /// Overrides the transient error kinds.
    #[must_use]
    pub fn with_transient(mut self, kinds: HashSet<ToolErrorKind>) -> Self {
        self.transient_error_kinds = kinds;
        self
    }

    /// Whether a failed result is transient and may be retried.
    #[must_use]
    pub fn is_transient(&self, result: &ToolResult) -> bool {
        !result.success
            && result.error_kind.is_some_and(|kind| self.transient_error_kinds.contains(&kind))
    }
}

/// Executes tool intents with classified retries under a small budget.
///
/// Every attempt goes through the full executor pipeline (policy gate,
/// budget reservation, update lifecycle), so policy denials and invalid
/// arguments fail once and are never retried, while transient failures
/// re-run until the retry budget is exhausted.
#[derive(Clone)]
pub struct ToolRetrier {
    /// The retry policy in force.
    pub policy: RetryPolicy,
    executor: ToolExecutor,
}

impl ToolRetrier {
    /// Creates a retrier wrapping an executor.
    #[must_use]
    pub fn new(policy: RetryPolicy, executor: ToolExecutor) -> Self {
        Self { policy, executor }
    }

    /// Executes one intent, retrying transient failures until success, a
    /// permanent failure, or the retry budget is exhausted.
    pub async fn execute(
        &self,
        intent: &ToolIntent,
        sink: &UpdateSink,
        client: &ClientBridge,
        cancel: watch::Receiver<bool>,
        task: &TaskNode,
        transcript: &[ModelMessage],
    ) -> Result<ToolResult, OrchestratorError> {
        let mut attempt = 0usize;
        loop {
            let result = self
                .executor
                .execute(intent, sink, client, cancel.clone(), task, transcript)
                .await?;
            if result.success
                || !self.policy.is_transient(&result)
                || attempt >= self.policy.max_retries
            {
                return Ok(result);
            }
            self.policy.backoff.sleep_for(attempt).await;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_acp_agent_server::{ClientBridge, UpdateSink};
    use ee_agent_protocol::SessionId;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::budget::BudgetTracker;
    use crate::config::OrchestratorConfig;
    use crate::events::EventRecorder;
    use crate::policy::PolicyEngine;
    use crate::tasks::TaskId;
    use crate::tools::{
        ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolFuture, ToolRegistry,
    };

    fn plumbing() -> (UpdateSink, ClientBridge, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UpdateSink::new_for_test(SessionId::new("s-1"), tx.clone()),
            ClientBridge::new_for_test(Duration::from_secs(5), tx),
            rx,
        )
    }

    fn task_fixture() -> TaskNode {
        TaskNode::new(TaskId::new("task-1"), "t", "d")
    }

    /// A read-class tool failing with `failure_kind` for the first
    /// `failures` calls, then succeeding.
    #[derive(Clone)]
    struct FlakyTool {
        definition: ToolDefinition,
        failures_left: Arc<Mutex<usize>>,
        failure_kind: ToolErrorKind,
        calls: Arc<Mutex<usize>>,
    }

    impl FlakyTool {
        fn new(failures: usize, failure_kind: ToolErrorKind) -> Self {
            Self::with_definition(
                ToolDefinition::new("flaky", "fails a bounded number of times")
                    .side_effect_class(SideEffectClass::Read),
                failures,
                failure_kind,
            )
        }

        fn with_definition(
            definition: ToolDefinition,
            failures: usize,
            failure_kind: ToolErrorKind,
        ) -> Self {
            Self {
                definition,
                failures_left: Arc::new(Mutex::new(failures)),
                failure_kind,
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            *self.calls.lock().expect("calls poisoned")
        }
    }

    impl ServerTool for FlakyTool {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn execute(
            &self,
            _arguments: serde_json::Value,
            _client: ClientBridge,
            _cancel: watch::Receiver<bool>,
            _context: ToolCallContext,
        ) -> ToolFuture<ToolResult> {
            let failures_left = self.failures_left.clone();
            let failure_kind = self.failure_kind;
            let calls = self.calls.clone();
            Box::pin(async move {
                *calls.lock().expect("calls poisoned") += 1;
                let mut remaining = failures_left.lock().expect("failures poisoned");
                if *remaining > 0 {
                    *remaining -= 1;
                    return ToolResult::failure(failure_kind, "transient boom");
                }
                ToolResult::success("recovered")
            })
        }
    }

    fn retrier_with(tool: Arc<FlakyTool>, policy: RetryPolicy) -> (ToolRetrier, Arc<FlakyTool>) {
        let config = OrchestratorConfig::default();
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let registered: Arc<dyn ServerTool> = tool.clone();
        tools.lock().expect("registry").register(registered).expect("registers tool");
        let executor = ToolExecutor::new(
            config,
            tools,
            Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default()))),
            PolicyEngine::default(),
            0,
            EventRecorder::new(),
        );
        (ToolRetrier::new(policy, executor), tool)
    }

    #[test]
    fn classification_maps_transient_and_permanent_kinds() {
        assert_eq!(classify_tool_error(ToolErrorKind::Timeout), RetryErrorClass::Transient);
        assert_eq!(classify_tool_error(ToolErrorKind::Backend), RetryErrorClass::Transient);
        assert_eq!(
            classify_tool_error(ToolErrorKind::InvalidArguments),
            RetryErrorClass::Permanent
        );
        assert_eq!(
            classify_tool_error(ToolErrorKind::PermissionDenied),
            RetryErrorClass::Permanent
        );
        assert_eq!(classify_tool_error(ToolErrorKind::Cancelled), RetryErrorClass::Permanent);
        assert_eq!(RetryErrorClass::Transient.as_str(), "transient");
        assert_eq!(RetryErrorClass::Permanent.as_str(), "permanent");
    }

    #[test]
    fn backoff_delays_grow_exponentially_and_cap() {
        let strategy =
            BackoffStrategy::new(Duration::from_millis(10), Duration::from_millis(100), 2);
        assert_eq!(strategy.delay_for(0), Duration::from_millis(10));
        assert_eq!(strategy.delay_for(1), Duration::from_millis(20));
        assert_eq!(strategy.delay_for(2), Duration::from_millis(40));
        assert_eq!(strategy.delay_for(3), Duration::from_millis(80));
        assert_eq!(strategy.delay_for(4), Duration::from_millis(100), "capped");
        assert_eq!(
            strategy.delay_for(100),
            Duration::from_millis(100),
            "capped for large attempts"
        );
    }

    #[test]
    fn backoff_defaults_are_sane() {
        let strategy = BackoffStrategy::default();
        assert_eq!(strategy.base_delay, Duration::from_millis(100));
        assert_eq!(strategy.max_delay, Duration::from_secs(2));
        assert_eq!(strategy.multiplier, 2);
        assert_eq!(RetryPolicy::default().max_retries, 2);
    }

    #[test]
    fn policy_marks_only_configured_kinds_transient() {
        let policy = RetryPolicy::default();
        assert!(policy.is_transient(&ToolResult::failure(ToolErrorKind::Timeout, "t")));
        assert!(policy.is_transient(&ToolResult::failure(ToolErrorKind::Backend, "b")));
        assert!(!policy.is_transient(&ToolResult::failure(ToolErrorKind::PermissionDenied, "p")));
        assert!(!policy.is_transient(&ToolResult::failure(ToolErrorKind::InvalidArguments, "i")));
        assert!(!policy.is_transient(&ToolResult::success("ok")), "success is never retried");
    }

    async fn run_once(
        retrier: &ToolRetrier,
        sink: &UpdateSink,
        client: &ClientBridge,
    ) -> Result<ToolResult, OrchestratorError> {
        let intent = ToolIntent::new("tc-1", "flaky", json!({}));
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        retrier.execute(&intent, sink, client, cancel_rx, &task_fixture(), &[]).await
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failure_is_retried_then_succeeds() {
        let tool = Arc::new(FlakyTool::new(1, ToolErrorKind::Timeout));
        let (retrier, tool) = retrier_with(tool, RetryPolicy::default());
        let (sink, client, _rx) = plumbing();

        let started = tokio::time::Instant::now();
        let result = run_once(&retrier, &sink, &client).await.expect("recovers");
        let elapsed = started.elapsed();

        assert!(result.success);
        assert_eq!(tool.call_count(), 2, "one failure then one success");
        assert!(
            elapsed >= Duration::from_millis(100),
            "waits the backoff before the retry, elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn permanent_failure_is_not_retried() {
        let tool = Arc::new(FlakyTool::new(5, ToolErrorKind::PermissionDenied));
        let (retrier, tool) = retrier_with(tool, RetryPolicy::default());
        let (sink, client, _rx) = plumbing();

        let result = run_once(&retrier, &sink, &client).await.expect("returns the failure");
        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert_eq!(tool.call_count(), 1, "permission denials are never retried");
    }

    #[tokio::test]
    async fn invalid_arguments_are_not_retried() {
        let tool = Arc::new(FlakyTool::new(5, ToolErrorKind::InvalidArguments));
        let (retrier, tool) = retrier_with(tool, RetryPolicy::default());
        let (sink, client, _rx) = plumbing();

        let result = run_once(&retrier, &sink, &client).await.expect("returns the failure");
        assert_eq!(result.error_kind, Some(ToolErrorKind::InvalidArguments));
        assert_eq!(tool.call_count(), 1, "invalid arguments are never retried");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_exhaustion_returns_last_failure() {
        let tool = Arc::new(FlakyTool::new(10, ToolErrorKind::Backend));
        let (retrier, tool) = retrier_with(tool, RetryPolicy::default());
        let (sink, client, _rx) = plumbing();

        let result = run_once(&retrier, &sink, &client).await.expect("returns the last failure");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));
        assert_eq!(tool.call_count(), 3, "first attempt plus two retries");
    }

    #[tokio::test]
    async fn zero_retries_disables_retrying() {
        let tool = Arc::new(FlakyTool::new(1, ToolErrorKind::Timeout));
        let policy = RetryPolicy::new(0, BackoffStrategy::default());
        let (retrier, tool) = retrier_with(tool, policy);
        let (sink, client, _rx) = plumbing();

        let result = run_once(&retrier, &sink, &client).await.expect("returns the failure");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Timeout));
        assert_eq!(tool.call_count(), 1);
    }

    #[tokio::test]
    async fn policy_denials_are_never_retried() {
        // A write-class tool under the default policy: the executor denies
        // it before the tool body ever runs, and the retrier must not retry.
        let tool = Arc::new(FlakyTool::with_definition(
            ToolDefinition::new("flaky", "write-class tool")
                .side_effect_class(SideEffectClass::Write),
            5,
            ToolErrorKind::Backend,
        ));
        let tools = Arc::new(Mutex::new(ToolRegistry::new()));
        let registered: Arc<dyn ServerTool> = tool.clone();
        tools.lock().expect("registry").register(registered).expect("registers write tool");
        let executor = ToolExecutor::new(
            OrchestratorConfig::default(),
            tools,
            Arc::new(Mutex::new(BudgetTracker::new(&OrchestratorConfig::default()))),
            PolicyEngine::default(),
            0,
            EventRecorder::new(),
        );
        let retrier = ToolRetrier::new(RetryPolicy::default(), executor);
        let (sink, client, _rx) = plumbing();

        let result = run_once(&retrier, &sink, &client).await.expect("returns the denial");
        assert_eq!(result.error_kind, Some(ToolErrorKind::PermissionDenied));
        assert_eq!(tool.call_count(), 0, "policy-denied tool bodies never run, never retried");
    }
}
