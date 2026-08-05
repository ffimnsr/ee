//! Provider-level rate limiting.
//!
//! [`RateLimiter`] enforces a concurrency cap (tokio semaphore) and a
//! requests-per-window cap (rolling window over [`RateLimitClock`]) shared by
//! every caller that holds a clone — so subagents talking to the same
//! provider share one limit.  Calls queue while waiting for a slot, but only
//! within their deadline: when the queue wait would exceed the turn deadline
//! the call fails fast with a timeout error instead of blocking the turn.
//!
//! The clock is injectable ([`RateLimitClock`]); the default [`TokioClock`]
//! uses `tokio::time`, whose paused-time mode makes window and deadline tests
//! deterministic without wall-clock sleeps.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::error::OrchestratorError;

/// Queue-wait polling step when the semaphore is contended.
const SEMAPHORE_POLL: Duration = Duration::from_millis(10);

/// Boxed sleep returned by [`RateLimitClock::sleep`].
pub type RateLimitSleep = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Injectable time source for window and deadline arithmetic.
///
/// The default [`TokioClock`] works with `tokio::time` (including paused-time
/// tests); tests may inject a manual clock for deadline scenarios without a
/// runtime.
pub trait RateLimitClock: Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> Instant;
    /// Sleeps for `duration` (or returns immediately for manual clocks).
    fn sleep(&self, duration: Duration) -> RateLimitSleep;
}

/// Default clock backed by `tokio::time`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

impl RateLimitClock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> RateLimitSleep {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Rate limiter knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum in-flight model calls sharing this limiter.
    pub max_concurrent: usize,
    /// Maximum requests admitted per `window`.
    pub max_requests_per_window: usize,
    /// Rolling window length.
    pub window: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self { max_concurrent: 4, max_requests_per_window: 60, window: Duration::from_secs(60) }
    }
}

/// Shared limiter state; clones share the same budgets.
struct RateLimiterInner {
    config: RateLimitConfig,
    clock: Box<dyn RateLimitClock>,
    semaphore: Arc<Semaphore>,
    window_times: Mutex<VecDeque<Instant>>,
}

impl std::fmt::Debug for RateLimiterInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiterInner")
            .field("config", &self.config)
            .field("window_times", &self.window_times)
            .finish_non_exhaustive()
    }
}

/// Provider-level rate limiter; cheap to clone and share across subagents.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    inner: Arc<RateLimiterInner>,
}

impl RateLimiter {
    /// Creates a limiter with the tokio clock.
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        Self::with_clock(config, Box::new(TokioClock))
    }

    /// Creates a limiter with an injected clock.
    #[must_use]
    pub fn with_clock(config: RateLimitConfig, clock: Box<dyn RateLimitClock>) -> Self {
        Self {
            inner: Arc::new(RateLimiterInner {
                config,
                clock,
                semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
                window_times: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// The configured knobs.
    #[must_use]
    pub fn config(&self) -> RateLimitConfig {
        self.inner.config
    }

    /// Acquires a concurrency permit and a window slot before a model call.
    ///
    /// Waits for capacity when the provider is busy, but only while `deadline`
    /// (usually the turn deadline) allows: if the queue wait would exceed it,
    /// fails fast with a timeout error.  The returned permit releases the
    /// concurrency slot when dropped; callers must hold it for the call.
    pub async fn acquire(
        &self,
        deadline: Option<Instant>,
    ) -> Result<RateLimitPermit, OrchestratorError> {
        let inner = &self.inner;
        let now = inner.clock.now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Err(OrchestratorError::Timeout(
                "rate limit queue wait would exceed the turn deadline".into(),
            ));
        }

        // Concurrency permit, deadline-bounded.
        let permit = loop {
            match inner.semaphore.clone().try_acquire_owned() {
                Ok(permit) => break permit,
                Err(_) => {
                    if deadline.is_some_and(|deadline| inner.clock.now() >= deadline) {
                        return Err(OrchestratorError::Timeout(
                            "rate limit concurrency wait exceeds the turn deadline".into(),
                        ));
                    }
                    inner.clock.sleep(SEMAPHORE_POLL).await;
                }
            }
        };

        // Window slot, deadline-bounded.
        let slot = self.reserve_window_slot(deadline).await;
        if let Err(error) = slot {
            drop(permit);
            return Err(error);
        }

        Ok(RateLimitPermit { permit })
    }

    async fn reserve_window_slot(
        &self,
        deadline: Option<Instant>,
    ) -> Result<(), OrchestratorError> {
        let inner = &self.inner;
        loop {
            let now = inner.clock.now();
            // The guard never crosses an await: it is scoped to the slot
            // check, so the acquire future stays `Send` for tokio::spawn.
            let wait = {
                let mut window_times =
                    inner.window_times.lock().expect("rate limit window poisoned");
                // Drop stale entries older than the window.
                let cutoff = now.checked_sub(inner.config.window).unwrap_or(now);
                while window_times.front().is_some_and(|time| *time <= cutoff) {
                    window_times.pop_front();
                }
                if window_times.len() < inner.config.max_requests_per_window {
                    window_times.push_back(now);
                    return Ok(());
                }
                let earliest = window_times.front().expect("non-empty window");
                earliest.saturating_duration_since(now) + inner.config.window
            };
            let exceeds_deadline = deadline
                .is_some_and(|deadline| now.checked_add(wait).is_none_or(|end| end > deadline));
            if exceeds_deadline {
                return Err(OrchestratorError::Timeout(
                    "rate limit window wait exceeds the turn deadline".into(),
                ));
            }
            inner.clock.sleep(wait).await;
        }
    }

    /// In-flight request count (concurrency + window occupancy), for
    /// diagnostics and tests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner
            .semaphore
            .available_permits()
            .min(self.inner.config.max_concurrent)
            .abs_diff(self.inner.config.max_concurrent)
    }

    /// Requests admitted in the current window.
    #[must_use]
    pub fn window_requests(&self) -> usize {
        let window_times = self.inner.window_times.lock().expect("rate limit window poisoned");
        window_times.len()
    }
}

/// Concurrency permit; releases its slot on drop.
#[derive(Debug)]
pub struct RateLimitPermit {
    permit: OwnedSemaphorePermit,
}

impl RateLimitPermit {
    /// Releases the permit immediately.
    pub fn release(self) {
        drop(self.permit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn config() -> RateLimitConfig {
        RateLimitConfig {
            max_concurrent: 2,
            max_requests_per_window: 100,
            window: Duration::from_secs(3_600),
        }
    }

    /// Manual clock for deadline tests: time advances only when told to.
    #[derive(Debug, Clone)]
    struct ManualClock {
        now: Arc<StdMutex<Instant>>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self { now: Arc::new(StdMutex::new(Instant::now())) }
        }

        fn advance(&self, duration: Duration) {
            *self.now.lock().expect("manual clock poisoned") += duration;
        }
    }

    impl RateLimitClock for ManualClock {
        fn now(&self) -> Instant {
            *self.now.lock().expect("manual clock poisoned")
        }

        fn sleep(&self, _duration: Duration) -> RateLimitSleep {
            // Yield cooperatively so the test's advance loop can run; the
            // acquire loop re-checks the (manually advanced) clock after
            // every wake-up.
            Box::pin(async { tokio::task::yield_now().await })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_limit_bounds_inflight_calls() {
        let limiter = RateLimiter::new(config());
        let first = limiter.acquire(None).await.expect("first acquires");
        let second = limiter.acquire(None).await.expect("second acquires");
        assert_eq!(limiter.in_flight(), 2);

        let waiting = limiter.clone();
        let task = tokio::spawn(async move { waiting.acquire(None).await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "third call must wait for a slot");
        assert_eq!(limiter.in_flight(), 2);

        drop(first);
        let third =
            tokio::time::timeout(Duration::from_secs(1), async { task.await.expect("task joins") })
                .await
                .expect("completes once a slot frees")
                .expect("acquires");
        assert_eq!(limiter.in_flight(), 2);
        drop(second);
        drop(third);
        assert_eq!(limiter.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn shared_provider_limit_applies_across_subagents() {
        // Two subagent contexts share one limiter: combined concurrency is
        // capped by the provider-level limit, not per subagent.
        let limiter = RateLimiter::new(RateLimitConfig {
            max_concurrent: 1,
            max_requests_per_window: 100,
            window: Duration::from_secs(3_600),
        });
        let subagent_a = limiter.clone();
        let subagent_b = limiter.clone();

        let a = tokio::spawn(async move {
            let _permit = subagent_a.acquire(None).await.expect("a acquires");
            tokio::time::sleep(Duration::from_secs(3_600)).await;
        });
        // Let a acquire before b starts.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let b = tokio::spawn(async move {
            let permit = subagent_b
                .acquire(Some(Instant::now() + Duration::from_secs(3_600)))
                .await
                .expect("b acquires once a releases");
            drop(permit);
        });
        tokio::task::yield_now().await;
        assert!(!b.is_finished(), "subagent b blocked by subagent a's in-flight call");

        // Release a's permit by cancelling its task; b then completes.
        a.abort();
        tokio::time::timeout(Duration::from_secs(1), async { b.await.expect("b joins") })
            .await
            .expect("b completes after a releases");
    }

    #[tokio::test(start_paused = true)]
    async fn per_window_limit_blocks_and_releases_after_window() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_concurrent: 10,
            max_requests_per_window: 2,
            window: Duration::from_secs(3_600),
        });
        let first = limiter.acquire(None).await.expect("first");
        let second = limiter.acquire(None).await.expect("second");
        assert_eq!(limiter.window_requests(), 2);

        let waiting = limiter.clone();
        let task = tokio::spawn(async move { waiting.acquire(None).await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "third request waits for the window");

        tokio::time::advance(Duration::from_secs(3_600)).await;
        let third =
            tokio::time::timeout(Duration::from_secs(1), async { task.await.expect("task joins") })
                .await
                .expect("completes after the window passes")
                .expect("acquires");
        drop(first);
        drop(second);
        drop(third);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_fail_fast_when_window_wait_exceeds_deadline() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_concurrent: 10,
            max_requests_per_window: 1,
            window: Duration::from_secs(3_600),
        });
        let _held = limiter.acquire(None).await.expect("fills the window");

        let waiting = limiter.clone();
        let task = tokio::spawn(async move {
            // Turn deadline is 60s; the window opens in 3600s → fail fast.
            waiting.acquire(Some(Instant::now() + Duration::from_secs(60))).await
        });
        tokio::task::yield_now().await;
        let error = task.await.expect("task joins").expect_err("timeout error");
        assert!(matches!(error, OrchestratorError::Timeout(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_waits_when_window_fits_budget() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_concurrent: 10,
            max_requests_per_window: 1,
            window: Duration::from_secs(3_600),
        });
        let _held = limiter.acquire(None).await.expect("fills the window");

        let waiting = limiter.clone();
        let task = tokio::spawn(async move {
            // Deadline 2h > 1h window wait → allowed to queue.
            waiting.acquire(Some(Instant::now() + Duration::from_secs(7_200))).await
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "queued while the deadline allows");

        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::time::timeout(Duration::from_secs(1), async { task.await.expect("task joins") })
            .await
            .expect("completes within budget")
            .expect("acquires");
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_wait_fails_fast_past_deadline() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_concurrent: 1,
            max_requests_per_window: 100,
            window: Duration::from_secs(3_600),
        });
        let _held = limiter.acquire(None).await.expect("fills concurrency");

        let waiting = limiter.clone();
        let task = tokio::spawn(async move {
            waiting.acquire(Some(Instant::now() + Duration::from_secs(30))).await
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "queued within deadline");
        tokio::time::advance(Duration::from_secs(31)).await;
        let error = task.await.expect("task joins").expect_err("timeout error");
        assert!(matches!(error, OrchestratorError::Timeout(_)));
    }

    #[tokio::test]
    async fn manual_clock_drives_deadline_checks_without_tokio_pause() {
        let clock = ManualClock::new();
        let limiter = RateLimiter::with_clock(
            RateLimitConfig {
                max_concurrent: 1,
                max_requests_per_window: 100,
                window: Duration::from_secs(3_600),
            },
            Box::new(clock.clone()),
        );
        let _held = limiter.acquire(None).await.expect("fills concurrency");

        let waiting = limiter.clone();
        let task = tokio::spawn(async move {
            waiting.acquire(Some(Instant::now() + Duration::from_secs(60))).await
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if task.is_finished() {
                break;
            }
            clock.advance(Duration::from_secs(10));
        }
        let error = task.await.expect("resolves").expect_err("timeout after deadline");
        assert!(matches!(error, OrchestratorError::Timeout(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_fails_before_waiting() {
        let limiter = RateLimiter::new(config());
        let error = limiter
            .acquire(Some(Instant::now() - Duration::from_secs(1)))
            .await
            .expect_err("deadline already passed");
        assert!(matches!(error, OrchestratorError::Timeout(_)));
        assert_eq!(limiter.window_requests(), 0, "no slot reserved");
    }

    #[test]
    fn config_defaults_and_roundtrip() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.max_requests_per_window, 60);
        let json = serde_json::to_string(&config).expect("serializes");
        let restored: RateLimitConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, config);
    }

    #[tokio::test]
    async fn in_flight_counts_permits_held() {
        let limiter = RateLimiter::new(config());
        assert_eq!(limiter.in_flight(), 0);
        let held = limiter.acquire(None).await.expect("acquires");
        assert_eq!(limiter.in_flight(), 1);
        held.release();
        assert_eq!(limiter.in_flight(), 0);
    }

    #[tokio::test]
    async fn concurrent_requests_never_exceed_the_limit() {
        let limiter = RateLimiter::new(config());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let limiter = limiter.clone();
            let active = active.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let _permit = limiter.acquire(None).await.expect("acquires");
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.await.expect("completes");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak concurrency {} > 2",
            peak.load(Ordering::SeqCst)
        );
    }
}
