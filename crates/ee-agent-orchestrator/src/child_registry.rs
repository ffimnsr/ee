//! Transient registry for supervised in-process subagents.
//!
//! Registry stores lifecycle metadata and cancellation handles only. Prompts,
//! transcripts, tool arguments, outputs, and workspace content never enter it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::subagents::SubagentId;
use crate::tasks::TaskId;

/// Maximum child entries retained for inspection.
pub const DEFAULT_CHILD_SNAPSHOT_LIMIT: usize = 64;
/// Maximum role metadata retained or exposed for one child.
pub const MAX_CHILD_ROLE_CHARS: usize = 128;

/// Observable child lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ChildState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Stalled,
}

impl ChildState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Stalled)
    }
}

/// Latest privacy-safe child activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ChildProgress {
    Registered,
    TaskRunning,
    ModelRequested,
    ModelResponded,
    ToolStarted,
    ToolFinished,
    TaskFinished,
}

/// Result of requesting targeted cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ChildCancelResult {
    Requested,
    AlreadyTerminal,
    NotFound,
}

/// One metadata-only child inspection record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChildSnapshotEntry {
    pub subagent_id: SubagentId,
    pub task_id: TaskId,
    pub parent_task_id: TaskId,
    pub role: String,
    pub state: ChildState,
    pub latest_progress: ChildProgress,
    pub started_at_unix_millis: u64,
    pub deadline_unix_millis: u64,
    pub last_activity_unix_millis: u64,
    pub elapsed_millis: u64,
    pub idle_millis: u64,
}

/// Bounded deterministic registry snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChildSnapshot {
    pub children: Vec<ChildSnapshotEntry>,
    pub total: usize,
    pub truncated: bool,
}

pub(crate) struct ChildRegistration {
    pub(crate) cancel: watch::Receiver<bool>,
    pub(crate) deadline: Instant,
}

#[derive(Debug)]
struct ChildEntry {
    subagent_id: SubagentId,
    task_id: TaskId,
    parent_task_id: TaskId,
    role: String,
    state: ChildState,
    latest_progress: ChildProgress,
    started_at: Instant,
    deadline: Instant,
    last_activity: Instant,
    finished_at: Option<Instant>,
    started_at_system: SystemTime,
    last_activity_system: SystemTime,
    cancel_tx: watch::Sender<bool>,
}

/// Backend-owned child registry. Terminal entries remain as bounded recent
/// history; active cancellation senders are never exposed by public snapshots.
#[derive(Debug)]
pub(crate) struct ChildRegistry {
    entries: Mutex<BTreeMap<SubagentId, ChildEntry>>,
    retention_limit: usize,
}

impl Default for ChildRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_CHILD_SNAPSHOT_LIMIT)
    }
}

impl ChildRegistry {
    pub(crate) fn new(retention_limit: usize) -> Self {
        Self { entries: Mutex::new(BTreeMap::new()), retention_limit: retention_limit.max(1) }
    }

    pub(crate) fn register(
        &self,
        subagent_id: SubagentId,
        task_id: TaskId,
        parent_task_id: TaskId,
        role: String,
        timeout: Duration,
    ) -> ChildRegistration {
        let now = Instant::now();
        let now_system = SystemTime::now();
        let deadline = now + timeout;
        let (cancel_tx, cancel) = watch::channel(false);
        let entry = ChildEntry {
            subagent_id: subagent_id.clone(),
            task_id,
            parent_task_id,
            role: role.chars().take(MAX_CHILD_ROLE_CHARS).collect(),
            state: ChildState::Pending,
            latest_progress: ChildProgress::Registered,
            started_at: now,
            deadline,
            last_activity: now,
            finished_at: None,
            started_at_system: now_system,
            last_activity_system: now_system,
            cancel_tx,
        };
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(subagent_id, entry);
        Self::prune(&mut entries, self.retention_limit);
        ChildRegistration { cancel, deadline }
    }

    pub(crate) fn mark_running(&self, id: &SubagentId) {
        self.update(id, ChildState::Running, ChildProgress::TaskRunning);
    }

    pub(crate) fn heartbeat(&self, id: &SubagentId, progress: ChildProgress) {
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = entries.get_mut(id) else { return };
        if entry.state.is_terminal() {
            return;
        }
        entry.latest_progress = progress;
        entry.last_activity = Instant::now();
        entry.last_activity_system = SystemTime::now();
    }

    pub(crate) fn finish(&self, id: &SubagentId, state: ChildState) -> bool {
        debug_assert!(state.is_terminal());
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = {
            let Some(entry) = entries.get_mut(id) else { return false };
            if entry.state.is_terminal() {
                return false;
            }
            let now = Instant::now();
            entry.state = state;
            entry.latest_progress = ChildProgress::TaskFinished;
            entry.last_activity = now;
            entry.finished_at = Some(now);
            entry.last_activity_system = SystemTime::now();
            true
        };
        Self::prune(&mut entries, self.retention_limit);
        changed
    }

    pub(crate) fn cancel(&self, id: &SubagentId) -> ChildCancelResult {
        let entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = entries.get(id) else { return ChildCancelResult::NotFound };
        if entry.state.is_terminal() {
            return ChildCancelResult::AlreadyTerminal;
        }
        let _ = entry.cancel_tx.send(true);
        ChildCancelResult::Requested
    }

    pub(crate) fn cancel_all(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .values()
            .filter(|entry| !entry.state.is_terminal())
            .map(|entry| usize::from(entry.cancel_tx.send(true).is_ok()))
            .sum()
    }

    pub(crate) async fn wait_for_stall(&self, id: &SubagentId, stall_timeout: Duration) -> bool {
        loop {
            let wake_at = {
                let entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(entry) = entries.get(id) else { return false };
                if entry.state.is_terminal() {
                    return false;
                }
                if entry.state == ChildState::Pending {
                    Instant::now() + stall_timeout
                } else {
                    entry.last_activity + stall_timeout
                }
            };
            tokio::time::sleep_until(wake_at).await;
            let entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(entry) = entries.get(id) else { return false };
            if entry.state == ChildState::Running
                && !entry.state.is_terminal()
                && Instant::now().duration_since(entry.last_activity) >= stall_timeout
            {
                return true;
            }
        }
    }

    pub(crate) fn snapshot(&self, requested_limit: usize) -> ChildSnapshot {
        let entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let total = entries.len();
        let limit = requested_limit.min(self.retention_limit);
        let now = Instant::now();
        let children = entries
            .values()
            .rev()
            .take(limit)
            .map(|entry| ChildSnapshotEntry {
                subagent_id: entry.subagent_id.clone(),
                task_id: entry.task_id.clone(),
                parent_task_id: entry.parent_task_id.clone(),
                role: entry.role.clone(),
                state: entry.state,
                latest_progress: entry.latest_progress,
                started_at_unix_millis: unix_millis(entry.started_at_system),
                deadline_unix_millis: unix_millis(
                    entry.started_at_system + entry.deadline.duration_since(entry.started_at),
                ),
                last_activity_unix_millis: unix_millis(entry.last_activity_system),
                elapsed_millis: millis(
                    entry.finished_at.unwrap_or(now).duration_since(entry.started_at),
                ),
                idle_millis: millis(now.duration_since(entry.last_activity)),
            })
            .collect();
        ChildSnapshot { children, total, truncated: total > limit }
    }

    fn update(&self, id: &SubagentId, state: ChildState, progress: ChildProgress) {
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = entries.get_mut(id) else { return };
        if entry.state.is_terminal() {
            return;
        }
        entry.state = state;
        entry.latest_progress = progress;
        entry.last_activity = Instant::now();
        entry.last_activity_system = SystemTime::now();
    }

    fn prune(entries: &mut BTreeMap<SubagentId, ChildEntry>, limit: usize) {
        while entries.len() > limit {
            let terminal = entries
                .iter()
                .filter(|(_, entry)| entry.state.is_terminal())
                .min_by_key(|(_, entry)| entry.started_at)
                .map(|(id, _)| id.clone());
            let Some(id) = terminal else { break };
            entries.remove(&id);
        }
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(registry: &ChildRegistry, id: &str) -> ChildRegistration {
        registry.register(
            SubagentId::new(id),
            TaskId::new(id),
            TaskId::new("parent"),
            "researcher".into(),
            Duration::from_secs(30),
        )
    }

    #[test]
    fn targeted_cancel_does_not_cancel_sibling() {
        let registry = ChildRegistry::default();
        let first = register(&registry, "first");
        let second = register(&registry, "second");
        assert_eq!(registry.cancel(&SubagentId::new("first")), ChildCancelResult::Requested);
        assert!(*first.cancel.borrow());
        assert!(!*second.cancel.borrow());
    }

    #[test]
    fn snapshot_is_bounded_and_metadata_only() {
        let registry = ChildRegistry::new(3);
        for id in ["one", "two", "three"] {
            register(&registry, id);
        }
        let snapshot = registry.snapshot(2);
        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.children.len(), 2);
        assert!(snapshot.truncated);
        let json = serde_json::to_string(&snapshot).expect("serializes");
        assert!(!json.contains("prompt"));
        assert!(!json.contains("transcript"));
    }

    #[test]
    fn snapshot_caps_role_metadata_without_leaking_suffix() {
        let registry = ChildRegistry::default();
        let sensitive_suffix = "DO_NOT_EXPOSE_SECRET";
        let role = format!("{}{}", "r".repeat(MAX_CHILD_ROLE_CHARS), sensitive_suffix);
        registry.register(
            SubagentId::new("child"),
            TaskId::new("child"),
            TaskId::new("parent"),
            role,
            Duration::from_secs(30),
        );

        let snapshot = registry.snapshot(1);
        assert_eq!(snapshot.children[0].role.chars().count(), MAX_CHILD_ROLE_CHARS);
        assert!(!serde_json::to_string(&snapshot).expect("serializes").contains(sensitive_suffix));
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_elapsed_time_stops_at_finish() {
        let registry = ChildRegistry::default();
        register(&registry, "child");
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(registry.finish(&SubagentId::new("child"), ChildState::Completed));
        let finished_elapsed = registry.snapshot(1).children[0].elapsed_millis;

        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(registry.snapshot(1).children[0].elapsed_millis, finished_elapsed);
        assert_eq!(finished_elapsed, 5_000);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_completion_prunes_oldest_terminal_entry() {
        let registry = ChildRegistry::new(2);
        register(&registry, "z-oldest");
        tokio::time::advance(Duration::from_secs(1)).await;
        register(&registry, "a-newer");
        tokio::time::advance(Duration::from_secs(1)).await;
        register(&registry, "active");

        assert!(registry.finish(&SubagentId::new("z-oldest"), ChildState::Completed));
        assert!(registry.finish(&SubagentId::new("a-newer"), ChildState::Completed));

        let snapshot = registry.snapshot(3);
        assert_eq!(snapshot.total, 2);
        assert!(
            !snapshot.children.iter().any(|child| child.subagent_id == SubagentId::new("z-oldest"))
        );
        assert!(
            snapshot.children.iter().any(|child| child.subagent_id == SubagentId::new("a-newer"))
        );
        assert!(
            snapshot.children.iter().any(|child| child.subagent_id == SubagentId::new("active"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn activity_refreshes_stall_deadline() {
        let registry = std::sync::Arc::new(ChildRegistry::default());
        register(&registry, "child");
        let id = SubagentId::new("child");
        registry.mark_running(&id);
        let waiter = tokio::spawn({
            let registry = registry.clone();
            let id = id.clone();
            async move { registry.wait_for_stall(&id, Duration::from_secs(10)).await }
        });
        tokio::time::advance(Duration::from_secs(9)).await;
        registry.heartbeat(&id, ChildProgress::ModelResponded);
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!waiter.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(waiter.await.expect("waiter"));
    }
}
