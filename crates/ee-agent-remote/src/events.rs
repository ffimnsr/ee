//! Ordered, resumable notification fan-out.
//!
//! The agent-event pump records every session-scoped notification in one
//! bounded ring buffer per session and fans it out to connected clients. A
//! reconnecting client replays from its last cursor through `history/tail`.
//!
//! Cursor rules:
//!
//! - the first recorded notification for a session has cursor 1;
//! - cursors are monotonic per session and never reused;
//! - when the ring overflows, oldest entries are dropped, which clients detect
//!   as a `gap` in `history/tail`;
//! - live-only notifications (connection status with no session) carry cursor
//!   `0` and are never replayed.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::protocol::{BufferedNotification, HistoryTailResult, StatusChangedNotification};
use crate::redact::Redactor;

/// Cursor value reserved for live-only notifications.
pub const LIVE_ONLY_CURSOR: u64 = 0;

#[derive(Debug, Clone)]
struct BufferedEntry {
    cursor: u64,
    method: String,
    params: Value,
}

#[derive(Debug)]
struct SessionStream {
    cursor: u64,
    capacity: usize,
    entries: VecDeque<BufferedEntry>,
}

impl SessionStream {
    fn new(capacity: usize) -> Self {
        Self { cursor: 0, capacity, entries: VecDeque::new() }
    }

    fn push_with(&mut self, method: &str, make_params: impl FnOnce(u64) -> Value) -> (u64, Value) {
        self.cursor += 1;
        let cursor = self.cursor;
        let params = make_params(cursor);
        self.entries.push_back(BufferedEntry {
            cursor,
            method: method.to_string(),
            params: params.clone(),
        });
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
        (cursor, params)
    }

    fn tail(&self, session_id: &str, from_cursor: u64, limit: usize) -> HistoryTailResult {
        let oldest = self.entries.front().map(|entry| entry.cursor);
        let gap = matches!(oldest, Some(oldest) if from_cursor.saturating_add(1) < oldest);
        let notifications = self
            .entries
            .iter()
            .filter(|entry| entry.cursor > from_cursor)
            .take(limit)
            .map(|entry| BufferedNotification {
                cursor: entry.cursor,
                method: entry.method.clone(),
                params: entry.params.clone(),
            })
            .collect();
        HistoryTailResult {
            session_id: session_id.to_string(),
            cursor: self.cursor,
            notifications,
            gap,
        }
    }
}

#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<Value>,
    overloaded: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct BusState {
    streams: BTreeMap<String, SessionStream>,
    subscribers: BTreeMap<u64, Subscriber>,
    capacity: usize,
    queue_capacity: usize,
}

/// One connected client's notification stream.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
    /// Bounded outbound notification channel.
    pub receiver: mpsc::Receiver<Value>,
    /// Set when a notification had to be dropped because this client's queue
    /// was full. Requests on this connection must answer `overload`.
    pub overloaded: Arc<AtomicBool>,
    bus: Arc<Mutex<BusState>>,
}

impl Subscription {
    /// The subscription id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the outbound queue overflowed.
    #[must_use]
    pub fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.lock().expect("event bus poisoned").subscribers.remove(&self.id);
    }
}

/// Shared notification bus.
#[derive(Debug, Clone)]
pub struct EventBus {
    state: Arc<Mutex<BusState>>,
    next_subscriber: Arc<AtomicU64>,
    redactor: Redactor,
}

impl EventBus {
    /// Creates a bus with a per-session ring capacity and per-client queue
    /// capacity and no configured-value seeds.
    #[must_use]
    pub fn new(ring_capacity: usize, queue_capacity: usize) -> Self {
        Self::with_redactor(ring_capacity, queue_capacity, Redactor::default())
    }

    /// Creates a bus whose buffered and live payloads use the host redactor.
    #[must_use]
    pub fn with_redactor(ring_capacity: usize, queue_capacity: usize, redactor: Redactor) -> Self {
        Self {
            state: Arc::new(Mutex::new(BusState {
                capacity: ring_capacity.max(1),
                queue_capacity: queue_capacity.max(1),
                ..BusState::default()
            })),
            next_subscriber: Arc::new(AtomicU64::new(1)),
            redactor,
        }
    }

    /// Registers one client notification stream.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(self.queue_capacity());
        let overloaded = Arc::new(AtomicBool::new(false));
        self.state
            .lock()
            .expect("event bus poisoned")
            .subscribers
            .insert(id, Subscriber { sender, overloaded: Arc::clone(&overloaded) });
        Subscription { id, receiver, overloaded, bus: Arc::clone(&self.state) }
    }

    fn queue_capacity(&self) -> usize {
        self.state.lock().expect("event bus poisoned").queue_capacity
    }

    /// Makes sure a stream exists for `session_id` (idempotent).
    pub fn ensure_stream(&self, session_id: &str) {
        let mut state = self.state.lock().expect("event bus poisoned");
        let capacity = state.capacity;
        state.streams.entry(session_id.to_string()).or_insert_with(|| SessionStream::new(capacity));
    }

    /// Records and broadcasts a session-scoped notification. Returns its
    /// cursor and the number of clients whose queue overflowed.
    pub fn publish(&self, session_id: &str, method: &str, params: Value) -> (u64, usize) {
        self.publish_with(session_id, method, move |_| params)
    }

    /// Records and broadcasts a session-scoped notification whose payload
    /// embeds its assigned cursor.
    pub fn publish_with(
        &self,
        session_id: &str,
        method: &str,
        make_params: impl FnOnce(u64) -> Value,
    ) -> (u64, usize) {
        let mut state = self.state.lock().expect("event bus poisoned");
        let capacity = state.capacity;
        let stream = state
            .streams
            .entry(session_id.to_string())
            .or_insert_with(|| SessionStream::new(capacity));
        let (cursor, params) =
            stream.push_with(method, |cursor| self.redactor.value(&make_params(cursor)));
        let message = crate::protocol::notification(method, params);
        let dropped = fan_out(&mut state.subscribers, &message);
        (cursor, dropped)
    }

    /// Broadcasts a live-only notification (cursor 0, never replayed).
    /// Returns the number of clients whose queue overflowed.
    pub fn publish_live(&self, method: &str, params: Value) -> usize {
        let message = crate::protocol::notification(method, self.redactor.value(&params));
        let mut state = self.state.lock().expect("event bus poisoned");
        fan_out(&mut state.subscribers, &message)
    }

    /// Broadcasts an arbitrary raw frame (a notification or a server→client
    /// request) to every connected client.
    ///
    /// Approval requests use this path so the transport writes the frame
    /// as-is and routes the client's JSON-RPC response back to the approval
    /// registry. Returns the number of clients whose queue overflowed.
    pub fn broadcast_frame(&self, frame: Value) -> usize {
        let frame = self.redactor.value(&frame);
        let mut state = self.state.lock().expect("event bus poisoned");
        fan_out(&mut state.subscribers, &frame)
    }

    /// Broadcasts a status change. Session-scoped statuses are buffered;
    /// connection-scoped statuses are live-only.
    pub fn publish_status(&self, status: StatusChangedNotification) -> Option<u64> {
        match status.session_id.clone() {
            Some(session_id) => {
                let (cursor, _) =
                    self.publish_with(&session_id, crate::protocol::STATUS_CHANGED, |cursor| {
                        let mut status = status.clone();
                        status.cursor = cursor;
                        serde_json::to_value(&status).unwrap_or(Value::Null)
                    });
                Some(cursor)
            }
            None => {
                let params = serde_json::to_value(&status).ok()?;
                self.publish_live(crate::protocol::STATUS_CHANGED, params);
                None
            }
        }
    }

    /// Replays buffered notifications after `from_cursor`.
    #[must_use]
    pub fn tail(&self, session_id: &str, from_cursor: u64, limit: usize) -> HistoryTailResult {
        let state = self.state.lock().expect("event bus poisoned");
        match state.streams.get(session_id) {
            Some(stream) => stream.tail(session_id, from_cursor, limit.max(1)),
            None => HistoryTailResult {
                session_id: session_id.to_string(),
                cursor: 0,
                notifications: Vec::new(),
                gap: false,
            },
        }
    }

    /// Highest allocated cursor for a session, when the stream exists.
    #[must_use]
    pub fn cursor(&self, session_id: &str) -> Option<u64> {
        self.state
            .lock()
            .expect("event bus poisoned")
            .streams
            .get(session_id)
            .map(|stream| stream.cursor)
    }

    /// Number of connected subscribers (tests and status).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.state.lock().expect("event bus poisoned").subscribers.len()
    }
}

fn fan_out(subscribers: &mut BTreeMap<u64, Subscriber>, message: &Value) -> usize {
    let mut dead = Vec::new();
    let mut dropped = 0;
    for (id, subscriber) in subscribers.iter() {
        match subscriber.sender.try_send(message.clone()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                subscriber.overloaded.store(true, Ordering::Release);
                dropped += 1;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => dead.push(*id),
        }
    }
    for id in dead {
        subscribers.remove(&id);
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(session_id: Option<&str>) -> StatusChangedNotification {
        StatusChangedNotification {
            cursor: 0,
            agent_id: Some(String::from("fake")),
            session_id: session_id.map(str::to_string),
            state: crate::protocol::StatusState::Ready,
            detail: None,
        }
    }

    #[test]
    fn cursors_start_at_one_and_increase() {
        let bus = EventBus::new(8, 8);
        assert_eq!(bus.publish("s", "session/update", serde_json::json!({})).0, 1);
        assert_eq!(bus.publish("s", "session/update", serde_json::json!({})).0, 2);
        assert_eq!(bus.cursor("s"), Some(2));
        assert_eq!(bus.cursor("other"), None);
    }

    #[test]
    fn tail_replays_after_the_cursor_and_reports_gaps() {
        let bus = EventBus::new(2, 8);
        for _ in 0..4 {
            bus.publish("s", "session/update", serde_json::json!({}));
        }
        let tail = bus.tail("s", 0, 10);
        assert!(tail.gap, "cursor 0 is older than the retained ring");
        assert_eq!(tail.notifications.len(), 2);
        assert_eq!(tail.cursor, 4);

        let tail = bus.tail("s", 2, 10);
        assert!(!tail.gap);
        assert_eq!(tail.notifications.iter().map(|n| n.cursor).collect::<Vec<_>>(), vec![3, 4]);

        let tail = bus.tail("s", 4, 10);
        assert!(tail.notifications.is_empty());
        assert!(!tail.gap);
    }

    #[test]
    fn session_status_is_buffered_and_connection_status_is_live_only() {
        let bus = EventBus::new(8, 8);
        bus.publish_status(status(Some("s")));
        bus.publish_status(status(None));
        let tail = bus.tail("s", 0, 10);
        assert_eq!(tail.cursor, 1);
        assert_eq!(tail.notifications[0].method, crate::protocol::STATUS_CHANGED);
    }

    #[tokio::test]
    async fn slow_subscribers_are_marked_overloaded_without_blocking_the_pump() {
        let bus = EventBus::new(8, 1);
        let mut subscription = bus.subscribe();
        assert_eq!(bus.publish("s", "session/update", serde_json::json!({"n":1})).1, 0);
        assert_eq!(bus.publish("s", "session/update", serde_json::json!({"n":2})).1, 1);
        assert!(subscription.is_overloaded());

        let first = subscription.receiver.recv().await.expect("first");
        assert_eq!(first["params"]["n"], 1);
    }

    #[test]
    fn dropped_subscriptions_are_pruned() {
        let bus = EventBus::new(8, 8);
        {
            let _subscription = bus.subscribe();
            assert_eq!(bus.subscriber_count(), 1);
        }
        assert_eq!(bus.subscriber_count(), 0);
    }
}
