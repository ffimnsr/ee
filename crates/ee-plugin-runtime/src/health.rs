//! Plugin health policy: crash counters, restart backoff, and stable uptime.
//!
//! Controllers are pure: callers pass the observed uptime so tests never depend
//! on the wall clock.

use serde::{Deserialize, Serialize};

/// Stable health states exposed to the host and frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Loaded,
    Blocked,
    Degraded,
    Crashed,
    Disabled,
    Reloaded,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Blocked => "blocked",
            Self::Degraded => "degraded",
            Self::Crashed => "crashed",
            Self::Disabled => "disabled",
            Self::Reloaded => "reloaded",
        }
    }
}

/// Typed reason attached to a health state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    Startup,
    ApplyFailed,
    DependencyMissing,
    ContributionConflict,
    TransportTimeout,
    TransportClosed,
    ResourceLimit,
    ConfigInvalid,
    Crashed,
    Stopped,
    Reloaded,
    Other(String),
}

impl HealthReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Startup => "startup",
            Self::ApplyFailed => "apply_failed",
            Self::DependencyMissing => "dependency_missing",
            Self::ContributionConflict => "contribution_conflict",
            Self::TransportTimeout => "transport_timeout",
            Self::TransportClosed => "transport_closed",
            Self::ResourceLimit => "resource_limit",
            Self::ConfigInvalid => "config_invalid",
            Self::Crashed => "crashed",
            Self::Stopped => "stopped",
            Self::Reloaded => "reloaded",
            Self::Other(value) => value,
        }
    }
}

/// Why a plugin instance stopped.
///
/// Runtime transports record this instead of collapsing every stop into a
/// generic crash, so hosts can distinguish clean exits from resource breaches
/// and timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// The plugin exited on its own or acknowledged shutdown.
    CleanExit,
    /// The process exited without a shutdown handshake or trapped.
    Crashed,
    /// Resident set size crossed the manifest memory cap.
    RssLimit,
    /// Accumulated CPU time crossed the manifest CPU cap.
    CpuLimit,
    /// An RPC or initialize handshake exceeded `rpc_timeout_ms`.
    RpcTimeout,
    /// Retained stderr crossed the runtime cap.
    StderrLimit,
    /// The plugin ignored shutdown and was killed after the grace period.
    ShutdownTimeout,
    /// Wasm fuel was exhausted.
    FuelExhausted,
    /// A Wasm memory growth request was denied.
    MemoryLimit,
    /// A Wasm call trapped.
    Trap,
    /// The host shut the plugin down.
    HostShutdown,
}

impl TerminationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CleanExit => "clean_exit",
            Self::Crashed => "crashed",
            Self::RssLimit => "rss_limit",
            Self::CpuLimit => "cpu_limit",
            Self::RpcTimeout => "rpc_timeout",
            Self::StderrLimit => "stderr_limit",
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::FuelExhausted => "fuel_exhausted",
            Self::MemoryLimit => "memory_limit",
            Self::Trap => "trap",
            Self::HostShutdown => "host_shutdown",
        }
    }

    /// Health reason reported for a termination.
    pub fn health_reason(self) -> HealthReason {
        match self {
            Self::CleanExit | Self::HostShutdown => HealthReason::Stopped,
            Self::Crashed | Self::Trap | Self::StderrLimit => HealthReason::Crashed,
            Self::RssLimit | Self::CpuLimit | Self::MemoryLimit => HealthReason::ResourceLimit,
            Self::RpcTimeout => HealthReason::TransportTimeout,
            Self::FuelExhausted | Self::ShutdownTimeout => HealthReason::ResourceLimit,
        }
    }
}

/// Restart policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub max_crashes: u32,
    /// Uptime after which crash counters and backoff reset.
    pub stable_uptime_ms: u64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            base_backoff_ms: 250,
            max_backoff_ms: 8_000,
            max_crashes: 8,
            stable_uptime_ms: 30_000,
        }
    }
}

/// Immutable health view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub state: HealthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
    pub crashes: u32,
    pub consecutive_failures: u32,
    pub restart_delay_ms: u64,
}

/// Mutable per-plugin health controller.
#[derive(Debug, Clone)]
pub struct HealthTracker {
    policy: HealthPolicy,
    state: HealthState,
    reason: Option<HealthReason>,
    crashes: u32,
    consecutive_failures: u32,
    restart_delay_ms: u64,
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new(HealthPolicy::default())
    }
}

impl HealthTracker {
    pub fn new(policy: HealthPolicy) -> Self {
        Self {
            policy,
            state: HealthState::Loaded,
            reason: None,
            crashes: 0,
            consecutive_failures: 0,
            restart_delay_ms: 0,
        }
    }

    pub fn policy(&self) -> HealthPolicy {
        self.policy
    }

    pub fn mark_loaded(&mut self) {
        self.state = HealthState::Loaded;
        self.reason = Some(HealthReason::Startup);
    }

    pub fn mark_blocked(&mut self, reason: HealthReason) {
        self.state = HealthState::Blocked;
        self.reason = Some(reason);
    }

    pub fn mark_degraded(&mut self, reason: HealthReason) {
        self.state = HealthState::Degraded;
        self.reason = Some(reason);
    }

    pub fn mark_disabled(&mut self, reason: HealthReason) {
        self.state = HealthState::Disabled;
        self.reason = Some(reason);
    }

    pub fn mark_reloaded(&mut self) {
        self.state = HealthState::Reloaded;
        self.reason = Some(HealthReason::Reloaded);
    }

    /// Reset failure counters after a stable run.
    pub fn record_stable(&mut self, uptime_ms: u64) {
        if uptime_ms >= self.policy.stable_uptime_ms {
            self.crashes = 0;
            self.consecutive_failures = 0;
            self.restart_delay_ms = 0;
        }
    }

    /// Record a crash and return the delay before the next restart.
    pub fn record_crash(&mut self, uptime_ms: u64) -> u64 {
        self.record_stable(uptime_ms);
        self.record_failure(HealthReason::Crashed)
    }

    /// Record a resource breach and return the restart delay.
    pub fn record_resource_breach(&mut self) -> u64 {
        self.record_failure(HealthReason::ResourceLimit)
    }

    fn record_failure(&mut self, reason: HealthReason) -> u64 {
        self.crashes = self.crashes.saturating_add(1).min(self.policy.max_crashes);
        self.consecutive_failures =
            self.consecutive_failures.saturating_add(1).min(self.policy.max_crashes);
        self.restart_delay_ms = if self.restart_delay_ms == 0 {
            self.policy.base_backoff_ms
        } else {
            self.restart_delay_ms.saturating_mul(2).min(self.policy.max_backoff_ms)
        };
        self.state = if self.crashes >= self.policy.max_crashes {
            HealthState::Crashed
        } else {
            HealthState::Degraded
        };
        self.reason = Some(reason);
        self.restart_delay_ms
    }

    /// Whether the policy still allows another restart.
    pub fn restart_allowed(&self) -> bool {
        self.crashes < self.policy.max_crashes
    }

    pub fn restart_delay_ms(&self) -> u64 {
        self.restart_delay_ms
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            state: self.state,
            reason: self.reason.clone(),
            crashes: self.crashes,
            consecutive_failures: self.consecutive_failures,
            restart_delay_ms: self.restart_delay_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HealthPolicy {
        HealthPolicy {
            base_backoff_ms: 100,
            max_backoff_ms: 400,
            max_crashes: 4,
            stable_uptime_ms: 1_000,
        }
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let mut tracker = HealthTracker::new(policy());
        assert_eq!(tracker.record_crash(0), 100);
        assert_eq!(tracker.record_crash(0), 200);
        assert_eq!(tracker.record_crash(0), 400);
        assert_eq!(tracker.record_crash(0), 400);
        assert_eq!(tracker.snapshot().crashes, 4);
        assert_eq!(tracker.snapshot().state, HealthState::Crashed);
        assert!(!tracker.restart_allowed());
    }

    #[test]
    fn stable_uptime_resets_counters() {
        let mut tracker = HealthTracker::new(policy());
        tracker.record_crash(0);
        tracker.record_crash(0);
        assert_eq!(tracker.restart_delay_ms(), 200);

        tracker.record_stable(1_000);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.crashes, 0);
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.restart_delay_ms, 0);

        assert_eq!(tracker.record_crash(0), 100);
    }

    #[test]
    fn resource_breach_transitions_to_degraded() {
        let mut tracker = HealthTracker::new(policy());
        let delay = tracker.record_resource_breach();
        assert_eq!(delay, 100);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.state, HealthState::Degraded);
        assert_eq!(snapshot.reason, Some(HealthReason::ResourceLimit));
        assert!(tracker.restart_allowed());
    }

    #[test]
    fn state_transitions_snapshot_deterministically() {
        let mut tracker = HealthTracker::new(policy());
        tracker.mark_loaded();
        assert_eq!(tracker.snapshot().state, HealthState::Loaded);
        tracker.mark_blocked(HealthReason::DependencyMissing);
        assert_eq!(tracker.snapshot().state, HealthState::Blocked);
        tracker.mark_degraded(HealthReason::TransportTimeout);
        assert_eq!(tracker.snapshot().state, HealthState::Degraded);
        tracker.mark_reloaded();
        assert_eq!(tracker.snapshot().state, HealthState::Reloaded);
        tracker.mark_disabled(HealthReason::Stopped);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.state, HealthState::Disabled);
        assert_eq!(snapshot.reason, Some(HealthReason::Stopped));
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["state"], "disabled");
    }
}
