//! The shared retry boundary for buffered and streaming attempts.

use std::future::Future;
use std::time::Instant;

use ee_agent_acp_server::ProviderError;

use crate::profile::RetryPolicy;
use crate::wire::{
    StreamDelta, is_retryable, is_timeout, is_unreachable, retry_after_of, retry_delay,
    unreachable_delay,
};

use super::{DeltaSink, StreamAttempt};

/// Runs a buffered attempt under a retry policy.
///
/// Only rate-limited and transient failures are retried, and only while the
/// attempt budget lasts; every other failure returns immediately.
///
/// # Errors
///
/// Returns the last failure when the budget is exhausted.
pub async fn with_buffered_retry<T, F, Fut>(
    policy: &RetryPolicy,
    mut attempt: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut retries = 0u32;
    let mut timeouts = 0u32;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        match attempt().await {
            Err(error) if is_retryable(&error) => {
                let unreachable = is_unreachable(&error);
                if unreachable {
                    // Unreachable failures do not spend the general allowance;
                    // an outage usually outlasts it and every attempt fails
                    // before reaching the provider.
                    let since = *unreachable_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= policy.unreachable_window {
                        return Err(error);
                    }
                } else {
                    if retries >= policy.max_attempts {
                        return Err(error);
                    }
                    if is_timeout(&error) {
                        timeouts += 1;
                        if timeouts > policy.max_timeout_retries {
                            return Err(error);
                        }
                    }
                    retries += 1;
                }
                let delay = if unreachable {
                    unreachable_delay(policy, retries)
                } else {
                    retry_delay(policy, retries.saturating_sub(1), retry_after_of(&error))
                };
                tokio::time::sleep(delay).await;
            }
            other => return other,
        }
    }
}

/// Runs a streaming attempt under a retry policy.
///
/// A retry happens only when no delta reached `sink` yet: once streamed output
/// exists, repeating the request would duplicate it, so the failure surfaces
/// instead. This is the "no retry after first output" rule.
///
/// # Errors
///
/// Returns the last failure when the budget is exhausted.
pub async fn with_stream_retry<T>(
    policy: &RetryPolicy,
    sink: DeltaSink<'_>,
    attempt: &mut dyn StreamAttempt<T>,
) -> Result<T, ProviderError> {
    let mut retries = 0u32;
    let mut timeouts = 0u32;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        let mut started = false;
        let result = {
            let mut forward = |delta: StreamDelta| {
                started = true;
                (*sink)(delta)
            };
            attempt.run(&mut forward).await
        };
        // Once streamed output exists, repeating the request would duplicate it,
        // so the failure surfaces instead.
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) if started => return Err(error),
            Err(error) => error,
        };
        let unreachable = is_unreachable(&error);
        if !is_retryable(&error) {
            return Err(error);
        }
        if unreachable {
            let since = *unreachable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= policy.unreachable_window {
                return Err(error);
            }
        } else {
            if retries >= policy.max_attempts {
                return Err(error);
            }
            if is_timeout(&error) {
                timeouts += 1;
                if timeouts > policy.max_timeout_retries {
                    return Err(error);
                }
            }
            retries += 1;
        }
        let delay = if unreachable {
            unreachable_delay(policy, retries)
        } else {
            retry_delay(policy, retries.saturating_sub(1), retry_after_of(&error))
        };
        tokio::time::sleep(delay).await;
    }
}
