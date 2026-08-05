//! Milestone summaries: bounded context snapshots under memory pressure.
//!
//! The [`MilestoneTracker`] accumulates raw observations and completed-task
//! counts; once the configured event or completed-task threshold is reached,
//! [`MilestoneTracker::finish_milestone`] consumes the observations into one
//! bounded [`MilestoneSummary`], stores it in the memory store with
//! provenance (key `milestone:<n>`, attributed to the source task), and
//! drops low-value raw observations when the store is under memory pressure.
//! The observation buffer itself is bounded, so the tracker never grows
//! without limit.

use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::memory::{MemoryItem, MemoryStore};
use crate::tasks::{TaskId, truncate};
use crate::trust::TrustLevel;

/// Default events between milestone summaries.
pub const DEFAULT_MILESTONE_MAX_EVENTS: usize = 64;
/// Default completed tasks between milestone summaries.
pub const DEFAULT_MILESTONE_MAX_COMPLETED_TASKS: usize = 8;
/// Default used-bytes fraction that counts as memory pressure (80%).
pub const DEFAULT_COMPACTION_PRESSURE: f64 = 0.8;
/// Default key prefix of low-value raw observations.
pub const DEFAULT_LOW_VALUE_PREFIX: &str = "obs:";
/// Cap on one milestone summary's text.
pub const MILESTONE_SUMMARY_MAX_CHARS: usize = 2_000;
/// Cap on retained raw observations; oldest are dropped first.
const MAX_OBSERVATIONS: usize = 256;

/// Milestone trigger and compaction knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneConfig {
    /// Milestone fires after this many observed events.
    pub max_events: usize,
    /// Milestone fires after this many completed tasks.
    pub max_completed_tasks: usize,
    /// Used-bytes fraction (0.0–1.0) above which low-value observations are
    /// dropped after a summary; 1.0 disables compaction, 0.0 always compacts.
    pub compaction_pressure: f64,
    /// Key prefix of low-value raw observations to drop under pressure.
    pub low_value_prefix: String,
}

impl Default for MilestoneConfig {
    fn default() -> Self {
        Self {
            max_events: DEFAULT_MILESTONE_MAX_EVENTS,
            max_completed_tasks: DEFAULT_MILESTONE_MAX_COMPLETED_TASKS,
            compaction_pressure: DEFAULT_COMPACTION_PRESSURE,
            low_value_prefix: DEFAULT_LOW_VALUE_PREFIX.to_string(),
        }
    }
}

/// One raw observation feeding the next milestone summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MilestoneObservation {
    /// Bounded observation text.
    pub text: String,
    /// Low-value observations (raw tool noise) are dropped under pressure
    /// once a summary replaced them.
    pub low_value: bool,
    /// The task that produced the observation, when known.
    pub source_task: Option<TaskId>,
}

impl MilestoneObservation {
    /// Creates an observation.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), low_value: false, source_task: None }
    }

    /// Marks the observation as low-value.
    #[must_use]
    pub fn low_value(mut self) -> Self {
        self.low_value = true;
        self
    }

    /// Attributes the observation to a task.
    #[must_use]
    pub fn from_task(mut self, source: TaskId) -> Self {
        self.source_task = Some(source);
        self
    }
}

/// A bounded summary produced at a milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MilestoneSummary {
    /// 1-based milestone number.
    pub milestone_number: u64,
    /// Bounded summary text (observations joined, capped).
    pub text: String,
    /// Observations consumed into this summary.
    pub event_count: usize,
    /// Completed tasks counted at this milestone.
    pub completed_tasks: usize,
    /// The task the summary is attributed to.
    pub source_task: Option<TaskId>,
}

/// Accumulates observations and completed-task counts until a milestone is
/// due, then summarizes into memory with provenance and compaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneTracker {
    config: MilestoneConfig,
    events_seen: usize,
    completed_tasks_seen: usize,
    milestone_number: u64,
    observations: Vec<MilestoneObservation>,
}

impl MilestoneTracker {
    /// Creates a tracker with the given config.
    #[must_use]
    pub fn new(config: MilestoneConfig) -> Self {
        Self {
            config,
            events_seen: 0,
            completed_tasks_seen: 0,
            milestone_number: 0,
            observations: Vec::new(),
        }
    }

    /// Observes one event, returning whether a milestone is now due.
    /// The observation buffer is bounded; the oldest observation is dropped
    /// when the cap is exceeded.
    pub fn observe(&mut self, observation: MilestoneObservation) -> bool {
        self.observations.push(observation);
        if self.observations.len() > MAX_OBSERVATIONS {
            self.observations.remove(0);
        }
        self.events_seen += 1;
        self.is_due()
    }

    /// Observes one completed task (recording its summary as a high-value
    /// observation), returning whether a milestone is now due.
    pub fn observe_completed_task(&mut self, summary: impl Into<String>) -> bool {
        self.observations.push(MilestoneObservation::new(summary));
        if self.observations.len() > MAX_OBSERVATIONS {
            self.observations.remove(0);
        }
        self.completed_tasks_seen += 1;
        self.is_due()
    }

    /// Whether the event or completed-task threshold was reached.
    #[must_use]
    pub fn is_due(&self) -> bool {
        self.events_seen >= self.config.max_events
            || self.completed_tasks_seen >= self.config.max_completed_tasks
    }

    /// Retained observations in arrival order.
    #[must_use]
    pub fn observations(&self) -> &[MilestoneObservation] {
        &self.observations
    }

    /// The last (or zero) milestone number.
    #[must_use]
    pub fn milestone_number(&self) -> u64 {
        self.milestone_number
    }

    /// Consumes the current milestone: when due, builds a bounded summary
    /// from the retained observations, stores it in `memory` under key
    /// `milestone:<n>` with provenance (`source_task`), drops low-value raw
    /// observations when the store is under pressure, and resets the
    /// counters.  Returns the summary, or `None` when no milestone is due.
    pub fn finish_milestone(
        &mut self,
        memory: &mut MemoryStore,
        source_task: Option<TaskId>,
    ) -> Option<MilestoneSummary> {
        if !self.is_due() {
            return None;
        }
        self.milestone_number += 1;
        let event_count = self.events_seen;
        let completed_tasks = self.completed_tasks_seen;
        let observations = std::mem::take(&mut self.observations);
        self.events_seen = 0;
        self.completed_tasks_seen = 0;
        let text = summarize(self.milestone_number, event_count, completed_tasks, &observations);
        let summary = MilestoneSummary {
            milestone_number: self.milestone_number,
            text: text.clone(),
            event_count,
            completed_tasks,
            source_task: source_task.clone(),
        };
        let key = format!("milestone:{}", self.milestone_number);
        let item = match source_task {
            Some(task) => {
                MemoryItem::from_task(key, text, task).with_trust(TrustLevel::SystemPolicy)
            }
            None => MemoryItem::new(key, text).with_trust(TrustLevel::SystemPolicy),
        };
        if memory.insert(item).is_ok() {
            self.compact_low_value(memory);
        }
        Some(summary)
    }

    /// Drops low-value raw observations (keyed by the configured prefix)
    /// when the memory store is under pressure.  Returns the number of
    /// dropped items.
    pub fn compact_low_value(&self, memory: &mut MemoryStore) -> usize {
        if self.pressure(memory) < self.config.compaction_pressure {
            return 0;
        }
        memory.remove_prefix(&self.config.low_value_prefix)
    }

    /// Used-bytes fraction of the configured limit; 1.0 when the limit is
    /// zero (always under pressure).
    fn pressure(&self, memory: &MemoryStore) -> f64 {
        if memory.limit_bytes() == 0 {
            return 1.0;
        }
        memory.total_bytes() as f64 / memory.limit_bytes() as f64
    }
}

/// Bounded milestone text: a header line plus one `- ` line per observation,
/// truncated to [`MILESTONE_SUMMARY_MAX_CHARS`].
fn summarize(
    milestone_number: u64,
    event_count: usize,
    completed_tasks: usize,
    observations: &[MilestoneObservation],
) -> String {
    let mut lines = vec![format!(
        "milestone {milestone_number}: {event_count} events, {completed_tasks} completed tasks"
    )];
    for observation in observations {
        lines.push(format!("- {}", observation.text));
    }
    truncate(&lines.join("\n"), MILESTONE_SUMMARY_MAX_CHARS)
}

/// Stores one raw observation as a low-value item (prefix from `config`) or
/// a plain observation item.  Returns a store error when the item does not
/// fit.  Convenience for callers that keep raw observations in memory until
/// the next milestone compacts them.
pub fn store_observation(
    memory: &mut MemoryStore,
    config: &MilestoneConfig,
    observation: &MilestoneObservation,
) -> Result<usize, OrchestratorError> {
    let key = if observation.low_value {
        format!("{}{}", config.low_value_prefix, observation.text)
    } else {
        format!("event:{}", observation.text)
    };
    let item = match &observation.source_task {
        Some(task) => MemoryItem::from_task(key, observation.text.clone(), task.clone()),
        None => MemoryItem::new(key, observation.text.clone()),
    };
    memory.insert(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_with(events: usize, completed: usize) -> MilestoneTracker {
        MilestoneTracker::new(MilestoneConfig {
            max_events: events,
            max_completed_tasks: completed,
            compaction_pressure: DEFAULT_COMPACTION_PRESSURE,
            low_value_prefix: DEFAULT_LOW_VALUE_PREFIX.to_string(),
        })
    }

    #[test]
    fn milestone_fires_after_configured_events() {
        let mut tracker = tracker_with(3, 8);
        assert!(!tracker.observe(MilestoneObservation::new("one")));
        assert!(!tracker.observe(MilestoneObservation::new("two")));
        assert!(tracker.observe(MilestoneObservation::new("three")), "third event fires");

        let mut memory = MemoryStore::new(4096);
        let task = TaskId::new("task-1");
        let summary = tracker.finish_milestone(&mut memory, Some(task.clone())).expect("summary");
        assert_eq!(summary.milestone_number, 1);
        assert_eq!(summary.event_count, 3);
        assert_eq!(summary.source_task, Some(task.clone()));
        assert!(summary.text.contains("one"));
        assert!(summary.text.contains("three"));
        assert!(summary.text.contains("3 events"));

        let item = memory.query("milestone:1").expect("stored with provenance");
        assert_eq!(item.source_task, Some(task));
        assert_eq!(item.value, summary.text);
        assert!(!tracker.is_due(), "counters reset after the milestone");
        assert!(tracker.observations().is_empty());
    }

    #[test]
    fn milestone_fires_after_configured_completed_tasks() {
        let mut tracker = tracker_with(64, 2);
        assert!(!tracker.observe_completed_task("task A done"));
        assert!(tracker.observe_completed_task("task B done"), "task threshold fires");
        let mut memory = MemoryStore::new(4096);
        let summary = tracker.finish_milestone(&mut memory, None).expect("summary");
        assert_eq!(summary.completed_tasks, 2);
        assert!(summary.text.contains("2 completed tasks"));
        assert!(memory.query("milestone:1").is_some());
    }

    #[test]
    fn no_summary_when_milestone_is_not_due() {
        let mut tracker = tracker_with(4, 2);
        tracker.observe(MilestoneObservation::new("one"));
        let mut memory = MemoryStore::new(4096);
        assert_eq!(tracker.finish_milestone(&mut memory, None), None);
        assert!(memory.is_empty(), "nothing stored before the milestone fires");
        assert_eq!(tracker.milestone_number(), 0);
    }

    #[test]
    fn milestone_numbers_increment_across_milestones() {
        let mut tracker = tracker_with(1, 8);
        let mut memory = MemoryStore::new(4096);
        tracker.observe(MilestoneObservation::new("a"));
        let first = tracker.finish_milestone(&mut memory, None).expect("first");
        tracker.observe(MilestoneObservation::new("b"));
        let second = tracker.finish_milestone(&mut memory, None).expect("second");
        assert_eq!(first.milestone_number, 1);
        assert_eq!(second.milestone_number, 2);
        assert!(memory.query("milestone:1").is_some());
        assert!(memory.query("milestone:2").is_some());
    }

    #[test]
    fn summaries_are_bounded() {
        let mut tracker = tracker_with(1, 8);
        let long = "x".repeat(MILESTONE_SUMMARY_MAX_CHARS * 2);
        tracker.observe(MilestoneObservation::new(long.clone()));
        let mut memory = MemoryStore::new(4096);
        let summary = tracker.finish_milestone(&mut memory, None).expect("summary");
        assert!(summary.text.chars().count() <= MILESTONE_SUMMARY_MAX_CHARS + 1);
        assert!(!summary.text.contains(&long), "truncated, not the raw text");
    }

    #[test]
    fn observation_buffer_is_bounded_and_drops_oldest() {
        let mut tracker = tracker_with(usize::MAX, usize::MAX);
        for index in 0..(MAX_OBSERVATIONS + 20) {
            tracker.observe(MilestoneObservation::new(format!("obs {index}")));
        }
        assert_eq!(tracker.observations().len(), MAX_OBSERVATIONS);
        assert!(
            tracker.observations()[0].text == format!("obs {}", 20),
            "oldest observations dropped first"
        );
    }

    #[test]
    fn compaction_drops_low_value_observations_after_summary() {
        // Threshold 0.0 always treats the store as under pressure, so the
        // low-value raw observations are dropped once the summary replaced
        // them; the store is large enough that nothing is evicted instead.
        let mut memory = MemoryStore::new(128);
        let config = MilestoneConfig {
            max_events: 2,
            max_completed_tasks: 8,
            compaction_pressure: 0.0,
            low_value_prefix: DEFAULT_LOW_VALUE_PREFIX.to_string(),
        };
        let mut tracker = MilestoneTracker::new(config.clone());
        let first = MilestoneObservation::new("a").low_value();
        let second = MilestoneObservation::new("b").low_value();
        store_observation(&mut memory, &config, &first).expect("stores");
        store_observation(&mut memory, &config, &second).expect("stores");
        assert!(memory.query("obs:a").is_some(), "raw observation stored");
        tracker.observe(first);
        tracker.observe(second);

        let summary = tracker.finish_milestone(&mut memory, None).expect("summary");
        let milestone = memory.query("milestone:1").expect("kept");
        assert_eq!(milestone.value, summary.text);
        assert_eq!(memory.query("obs:a"), None, "low-value observations dropped");
        assert_eq!(memory.query("obs:b"), None, "low-value observations dropped");
        assert_eq!(memory.total_bytes(), milestone.byte_size(), "only the summary remains");
    }

    #[test]
    fn no_compaction_without_memory_pressure() {
        let mut memory = MemoryStore::new(4096);
        let config = MilestoneConfig {
            max_events: 1,
            max_completed_tasks: 8,
            compaction_pressure: 1.0,
            low_value_prefix: DEFAULT_LOW_VALUE_PREFIX.to_string(),
        };
        store_observation(&mut memory, &config, &MilestoneObservation::new("keep me").low_value())
            .expect("stores");
        let mut tracker = MilestoneTracker::new(config.clone());
        tracker.observe(MilestoneObservation::new("event").low_value());
        tracker.finish_milestone(&mut memory, None).expect("summary");
        assert_eq!(
            memory.query("obs:keep me").map(|item| item.value).as_deref(),
            Some("keep me"),
            "no pressure, no compaction"
        );
        assert!(memory.query("milestone:1").is_some());
    }

    #[test]
    fn store_observation_marks_low_value_with_prefix() {
        let mut memory = MemoryStore::new(4096);
        let config = MilestoneConfig::default();
        let low = MilestoneObservation::new("raw").low_value().from_task(TaskId::new("task-1"));
        store_observation(&mut memory, &config, &low).expect("stores");
        let stored = memory.query("obs:raw").expect("prefixed");
        assert_eq!(stored.source_task, Some(TaskId::new("task-1")));
        store_observation(&mut memory, &config, &MilestoneObservation::new("kept"))
            .expect("stores");
        assert!(memory.query("event:kept").is_some(), "high-value events use the event prefix");
    }

    #[test]
    fn milestone_types_roundtrip_through_json() {
        let config = MilestoneConfig {
            max_events: 2,
            max_completed_tasks: 1,
            compaction_pressure: 0.5,
            low_value_prefix: "obs:".to_string(),
        };
        let json = serde_json::to_string(&config).expect("serializes");
        let restored: MilestoneConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, config);

        let summary = MilestoneSummary {
            milestone_number: 3,
            text: "milestone 3: done".into(),
            event_count: 12,
            completed_tasks: 1,
            source_task: Some(TaskId::new("task-1")),
        };
        let json = serde_json::to_string(&summary).expect("serializes");
        let restored: MilestoneSummary = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, summary);
    }
}
