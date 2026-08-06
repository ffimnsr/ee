//! Injectable policy clock (Phase 6 lifecycle).
//!
//! The evaluator itself never reads the clock — time is injected per
//! evaluation — but grant creation, store-load validation, and lifecycle
//! surfaces need a clock, and tests must never depend on wall-clock sleeps.
//! Production uses [`PolicyClock::System`]; tests use a deterministic
//! [`PolicyClock::Fake`] whose time advances only on demand.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Injectable clock for trust-policy time reads.
#[derive(Debug, Clone, Default)]
pub(crate) enum PolicyClock {
    /// Real wall clock (production).
    #[default]
    System,
    /// Deterministic fake clock for tests; advances only via [`Self::advance`].
    Fake(Arc<Mutex<SystemTime>>),
}

impl PolicyClock {
    /// Fake clock starting at `start` (tests).
    pub(crate) fn fake_at(start: SystemTime) -> Self {
        PolicyClock::Fake(Arc::new(Mutex::new(start)))
    }

    /// Current policy time.
    pub(crate) fn now(&self) -> SystemTime {
        match self {
            PolicyClock::System => SystemTime::now(),
            PolicyClock::Fake(inner) => *inner.lock().expect("fake clock poisoned"),
        }
    }

    /// Advances a fake clock by `duration`; a no-op for the system clock.
    pub(crate) fn advance(&self, duration: Duration) {
        if let PolicyClock::Fake(inner) = self {
            *inner.lock().expect("fake clock poisoned") += duration;
        }
    }
}
