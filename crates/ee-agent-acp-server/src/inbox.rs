//! Bounded per-session prompt inbox.
//!
//! Sits between ACP `session/prompt` and the provider/orchestrator.  While a
//! turn runs, an arriving prompt is either steered into that turn (default)
//! or queued and promoted when the turn settles.  The inbox is the only
//! place that knows about pending items, so cancellation, size caps, and
//! promotion stay deterministic.
//!
//! Prompts are bounded twice: at most [`DEFAULT_MAX_PENDING_PROMPTS`] items
//! per session and [`DEFAULT_MAX_PROMPT_BYTES`] serialized bytes per buffered
//! prompt.  The caps are configurable through [`AcpAgentServerConfig`].
//!
//! [`AcpAgentServerConfig`]: crate::config::AcpAgentServerConfig

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use ee_agent_protocol::{ContentBlock, Meta, PromptDelivery, RequestId, SessionId};
use tokio::sync::{Notify, mpsc};

/// Default maximum number of buffered prompts per session.
pub const DEFAULT_MAX_PENDING_PROMPTS: usize = 16;

/// Default maximum serialized size of one buffered prompt.
pub const DEFAULT_MAX_PROMPT_BYTES: usize = 256 * 1024;

/// One steer message: the prompt blocks plus the request that delivered them.
#[derive(Debug, Clone)]
pub(crate) struct SteerMessage {
    pub request_id: RequestId,
    pub blocks: Vec<ContentBlock>,
}

/// Receiver half of one running turn's steer channel.
///
/// The provider drains this at each step boundary.  Cloning shares the same
/// channel, so the inbox registry can drain leftovers after the turn ends.
#[derive(Clone)]
pub struct SteerInbox {
    inner: Arc<SteerInboxInner>,
}

struct SteerInboxInner {
    rx: Mutex<mpsc::Receiver<SteerMessage>>,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for SteerInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteerInbox").finish_non_exhaustive()
    }
}

impl SteerInbox {
    /// Creates a steer channel with the given capacity.
    ///
    /// Providers receive the inbox through [`crate::PromptContext`]; tests
    /// may construct one directly to seed steers before a turn starts.
    #[must_use]
    pub fn channel(capacity: usize) -> (SteerSender, Self) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let notify = Arc::new(Notify::new());
        let inbox = Self {
            inner: Arc::new(SteerInboxInner { rx: Mutex::new(rx), notify: notify.clone() }),
        };
        (SteerSender { tx, notify }, inbox)
    }

    /// Drains every steer message currently buffered, in send order.
    #[must_use]
    pub fn drain(&self) -> Vec<Vec<ContentBlock>> {
        self.drain_messages().into_iter().map(|message| message.blocks).collect()
    }

    /// Waits for one steer message.
    ///
    /// Returns `None` only when the sender side is gone and the channel is
    /// empty.
    pub async fn recv(&self) -> Option<Vec<ContentBlock>> {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(message) = self.try_recv() {
                return Some(message.blocks);
            }
            notified.await;
        }
    }

    fn try_recv(&self) -> Option<SteerMessage> {
        self.inner.rx.lock().expect("steer inbox poisoned").try_recv().ok()
    }

    fn drain_messages(&self) -> Vec<SteerMessage> {
        let mut rx = self.inner.rx.lock().expect("steer inbox poisoned");
        let mut messages = Vec::new();
        while let Ok(message) = rx.try_recv() {
            messages.push(message);
        }
        messages
    }
}

/// Sender half of one running turn's steer channel.
#[derive(Clone)]
pub struct SteerSender {
    tx: mpsc::Sender<SteerMessage>,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for SteerSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteerSender").finish_non_exhaustive()
    }
}

impl SteerSender {
    /// Attempts to enqueue one steer without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`SteerSendError::Full`] when the channel is at capacity and
    /// [`SteerSendError::Closed`] when the turn already settled.
    pub fn try_send(
        &self,
        request_id: RequestId,
        blocks: Vec<ContentBlock>,
    ) -> Result<(), SteerSendError> {
        self.tx.try_send(SteerMessage { request_id, blocks }).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SteerSendError::Full,
            mpsc::error::TrySendError::Closed(_) => SteerSendError::Closed,
        })?;
        self.notify.notify_waiters();
        Ok(())
    }
}

/// Why a non-blocking steer send failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerSendError {
    /// The running turn's steer buffer is full.
    Full,
    /// The running turn already settled.
    Closed,
}

/// One buffered prompt waiting for the running turn to settle.
#[derive(Debug, Clone)]
pub(crate) struct QueuedPrompt {
    pub request_id: RequestId,
    pub prompt: Vec<ContentBlock>,
    pub metadata: Option<Meta>,
}

/// Result of accepting a prompt into the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// Injected into the running turn; the request joins that turn's result.
    Steered,
    /// Buffered; `position` is 1-based.
    Queued { position: usize },
    /// No turn is running, so the caller starts the prompt normally.
    Idle,
    /// Inbox disabled by configuration.
    Disabled,
    /// Inbox capacity or per-prompt size cap exceeded.
    Full,
}

/// What the registry keeps per session.
#[derive(Default)]
struct SessionInbox {
    steer: Option<SteerSender>,
    steer_inbox: Option<SteerInbox>,
    waiters: Vec<RequestId>,
    pending: VecDeque<QueuedPrompt>,
}

/// Items released when a turn settles.
pub(crate) struct FinishedInbox {
    /// Steer requests that joined the finished turn.
    pub waiters: Vec<RequestId>,
    /// The next queued prompt and its fresh steer inbox.
    pub next: Option<(QueuedPrompt, SteerInbox)>,
}

/// Bounded per-session prompt inbox.
pub(crate) struct PromptInbox {
    enabled: bool,
    max_pending: usize,
    max_prompt_bytes: usize,
    sessions: Mutex<HashMap<SessionId, SessionInbox>>,
}

impl PromptInbox {
    pub(crate) fn new(enabled: bool, max_pending: usize, max_prompt_bytes: usize) -> Self {
        Self {
            enabled,
            max_pending: max_pending.max(1),
            max_prompt_bytes,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Marks a turn active and returns the steer inbox the provider drains.
    pub(crate) fn begin(&self, session_id: &SessionId) -> SteerInbox {
        let (sender, inbox) = SteerInbox::channel(self.max_pending);
        let mut sessions = self.sessions.lock().expect("prompt inbox poisoned");
        let state = sessions.entry(session_id.clone()).or_default();
        state.steer = Some(sender);
        state.steer_inbox = Some(inbox.clone());
        inbox
    }

    /// Accepts one prompt while a turn is running.
    pub(crate) fn deliver(
        &self,
        session_id: &SessionId,
        request_id: RequestId,
        delivery: PromptDelivery,
        prompt: Vec<ContentBlock>,
        metadata: Option<Meta>,
    ) -> DeliveryOutcome {
        let mut sessions = self.sessions.lock().expect("prompt inbox poisoned");
        let Some(state) = sessions.get_mut(session_id) else {
            return DeliveryOutcome::Idle;
        };
        let Some(sender) = state.steer.clone() else {
            return DeliveryOutcome::Idle;
        };
        if !self.enabled {
            return DeliveryOutcome::Disabled;
        }
        if prompt_bytes(&prompt) > self.max_prompt_bytes {
            return DeliveryOutcome::Full;
        }
        if delivery == PromptDelivery::Steer {
            match sender.try_send(request_id.clone(), prompt.clone()) {
                Ok(()) => {
                    state.waiters.push(request_id);
                    return DeliveryOutcome::Steered;
                }
                Err(SteerSendError::Full | SteerSendError::Closed) => {
                    // A saturated or closing steer buffer falls back to the
                    // queue so the prompt is never lost to a timing race.
                }
            }
        }
        if state.pending.len() >= self.max_pending {
            return DeliveryOutcome::Full;
        }
        state.pending.push_back(QueuedPrompt { request_id, prompt, metadata });
        DeliveryOutcome::Queued { position: state.pending.len() }
    }

    /// Settles the running turn.
    ///
    /// Returns the steer waiters that must receive the turn's result and the
    /// next queued prompt, if any.  Steer messages the provider never drained
    /// are promoted ahead of older queued prompts so they still run.
    pub(crate) fn finish(&self, session_id: &SessionId) -> FinishedInbox {
        let mut sessions = self.sessions.lock().expect("prompt inbox poisoned");
        let Some(mut state) = sessions.remove(session_id) else {
            return FinishedInbox { waiters: Vec::new(), next: None };
        };
        let undelivered =
            state.steer_inbox.take().map(|inbox| inbox.drain_messages()).unwrap_or_default();
        let undelivered_ids: Vec<RequestId> =
            undelivered.iter().map(|message| message.request_id.clone()).collect();
        state.waiters.retain(|request_id| !undelivered_ids.contains(request_id));
        for message in undelivered.into_iter().rev() {
            state.pending.push_front(QueuedPrompt {
                request_id: message.request_id,
                prompt: message.blocks,
                metadata: None,
            });
        }
        let waiters = state.waiters;
        let Some(next) = state.pending.pop_front() else {
            return FinishedInbox { waiters, next: None };
        };
        let (sender, inbox) = SteerInbox::channel(self.max_pending);
        state.steer = Some(sender);
        state.steer_inbox = Some(inbox.clone());
        state.waiters = Vec::new();
        sessions.insert(session_id.clone(), state);
        FinishedInbox { waiters, next: Some((next, inbox)) }
    }

    /// Cancels every buffered item for a session: queued prompts, steer
    /// waiters, and steers the provider never drained.  The whole session
    /// state is removed so the settling task cannot promote anything.
    pub(crate) fn cancel_session(&self, session_id: &SessionId) -> Vec<RequestId> {
        let Some(mut state) =
            self.sessions.lock().expect("prompt inbox poisoned").remove(session_id)
        else {
            return Vec::new();
        };
        let mut request_ids = std::mem::take(&mut state.waiters);
        for queued in state.pending.drain(..) {
            if !request_ids.contains(&queued.request_id) {
                request_ids.push(queued.request_id);
            }
        }
        if let Some(inbox) = state.steer_inbox {
            for message in inbox.drain_messages() {
                // A delivered steer is both a waiter and a buffered message
                // until the provider drains it; answer it exactly once.
                if !request_ids.contains(&message.request_id) {
                    request_ids.push(message.request_id);
                }
            }
        }
        request_ids
    }

    /// Removes all inbox state for a closed session.
    pub(crate) fn close(&self, session_id: &SessionId) -> (Vec<QueuedPrompt>, Vec<RequestId>) {
        let Some(state) = self.sessions.lock().expect("prompt inbox poisoned").remove(session_id)
        else {
            return (Vec::new(), Vec::new());
        };
        (state.pending.into_iter().collect(), state.waiters)
    }

    /// Number of queued items, for diagnostics and tests.
    #[cfg(test)]
    pub(crate) fn pending_len(&self, session_id: &SessionId) -> usize {
        self.sessions
            .lock()
            .expect("prompt inbox poisoned")
            .get(session_id)
            .map_or(0, |state| state.pending.len())
    }
}

fn prompt_bytes(prompt: &[ContentBlock]) -> usize {
    serde_json::to_vec(prompt).map_or(usize::MAX, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::TextContent;

    fn block(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextContent::new(text))]
    }

    #[test]
    fn idle_delivery_reports_idle_without_buffering() {
        let inbox = PromptInbox::new(true, 4, 1024);
        let outcome = inbox.deliver(
            &SessionId::new("s"),
            RequestId::Number(1),
            PromptDelivery::Steer,
            block("hi"),
            None,
        );
        assert_eq!(outcome, DeliveryOutcome::Idle);
    }

    #[test]
    fn disabled_inbox_reports_disabled() {
        let inbox = PromptInbox::new(false, 4, 1024);
        inbox.begin(&SessionId::new("s"));
        let outcome = inbox.deliver(
            &SessionId::new("s"),
            RequestId::Number(1),
            PromptDelivery::Steer,
            block("hi"),
            None,
        );
        assert_eq!(outcome, DeliveryOutcome::Disabled);
    }

    #[test]
    fn steers_are_drained_in_order_and_become_waiters() {
        let inbox = PromptInbox::new(true, 4, 1024);
        let steer = inbox.begin(&SessionId::new("s"));
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(1),
                PromptDelivery::Steer,
                block("one"),
                None,
            ),
            DeliveryOutcome::Steered
        );
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(2),
                PromptDelivery::Steer,
                block("two"),
                None,
            ),
            DeliveryOutcome::Steered
        );
        let drained = steer.drain();
        assert_eq!(drained.len(), 2);
        let finished = inbox.finish(&SessionId::new("s"));
        assert_eq!(finished.waiters, vec![RequestId::Number(1), RequestId::Number(2)]);
        assert!(finished.next.is_none());
    }

    #[test]
    fn queue_promotes_in_fifo_order() {
        let inbox = PromptInbox::new(true, 4, 1024);
        let _steer = inbox.begin(&SessionId::new("s"));
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(1),
                PromptDelivery::Queue,
                block("one"),
                None,
            ),
            DeliveryOutcome::Queued { position: 1 }
        );
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(2),
                PromptDelivery::Queue,
                block("two"),
                None,
            ),
            DeliveryOutcome::Queued { position: 2 }
        );
        let finished = inbox.finish(&SessionId::new("s"));
        let (first, _inbox) = finished.next.expect("first queued prompt promotes");
        assert_eq!(first.request_id, RequestId::Number(1));
        let finished = inbox.finish(&SessionId::new("s"));
        let (second, _inbox) = finished.next.expect("second queued prompt promotes");
        assert_eq!(second.request_id, RequestId::Number(2));
    }

    #[test]
    fn queue_capacity_is_bounded() {
        let inbox = PromptInbox::new(true, 1, 1024);
        let _steer = inbox.begin(&SessionId::new("s"));
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(1),
                PromptDelivery::Queue,
                block("one"),
                None,
            ),
            DeliveryOutcome::Queued { position: 1 }
        );
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(2),
                PromptDelivery::Queue,
                block("two"),
                None,
            ),
            DeliveryOutcome::Full
        );
    }

    #[test]
    fn oversized_prompt_is_rejected() {
        let inbox = PromptInbox::new(true, 4, 8);
        let _steer = inbox.begin(&SessionId::new("s"));
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(1),
                PromptDelivery::Queue,
                block("this is much longer than eight bytes"),
                None,
            ),
            DeliveryOutcome::Full
        );
    }

    #[test]
    fn undrained_steer_promotes_as_the_next_turn() {
        let inbox = PromptInbox::new(true, 4, 1024);
        let _steer = inbox.begin(&SessionId::new("s"));
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(7),
                PromptDelivery::Steer,
                block("late"),
                None,
            ),
            DeliveryOutcome::Steered
        );
        let finished = inbox.finish(&SessionId::new("s"));
        assert!(finished.waiters.is_empty(), "undrained steer is not a waiter of the old turn");
        let (next, _inbox) = finished.next.expect("undrained steer promotes");
        assert_eq!(next.request_id, RequestId::Number(7));
    }

    #[test]
    fn cancel_session_answers_queued_and_steer_waiters() {
        let inbox = PromptInbox::new(true, 4, 1024);
        let steer = inbox.begin(&SessionId::new("s"));
        let _ = inbox.deliver(
            &SessionId::new("s"),
            RequestId::Number(1),
            PromptDelivery::Queue,
            block("queued"),
            None,
        );
        assert_eq!(
            inbox.deliver(
                &SessionId::new("s"),
                RequestId::Number(2),
                PromptDelivery::Steer,
                block("steer"),
                None,
            ),
            DeliveryOutcome::Steered
        );
        let pending_ids = inbox.cancel_session(&SessionId::new("s"));
        assert!(pending_ids.contains(&RequestId::Number(1)));
        assert!(pending_ids.contains(&RequestId::Number(2)));
        assert!(steer.drain().is_empty(), "cancel removed the undrained steer");
        assert_eq!(inbox.pending_len(&SessionId::new("s")), 0);
        assert!(inbox.finish(&SessionId::new("s")).waiters.is_empty());
    }
}
