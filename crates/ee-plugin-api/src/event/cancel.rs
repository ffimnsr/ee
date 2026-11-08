//! Minimal cancellation primitive for async event dispatch.
//!
//! This avoids pulling a runtime-specific dependency into `ee-plugin-api`.
//! Hosts may bridge this token to their own cancellation type.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    wakers: Vec<Waker>,
}

/// Cloneable cancellation signal.
#[derive(Clone, Default)]
pub struct CancelToken {
    state: Arc<Mutex<CancelState>>,
}

impl CancelToken {
    /// Create an uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel the token and wake every waiter.
    pub fn cancel(&self) {
        let wakers = {
            let mut state = self.state.lock().expect("cancel token lock poisoned");
            if state.cancelled {
                return;
            }
            state.cancelled = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.state.lock().expect("cancel token lock poisoned").cancelled
    }

    /// Future that resolves when the token is cancelled.
    pub fn cancelled(&self) -> CancelFuture {
        CancelFuture { token: self.clone() }
    }
}

/// Future returned by [`CancelToken::cancelled`].
pub struct CancelFuture {
    token: CancelToken,
}

impl Future for CancelFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.token.state.lock().expect("cancel token lock poisoned");
        if state.cancelled {
            return Poll::Ready(());
        }
        if !state.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::executor::block_on;
    use futures::future::{join, pending};

    use super::*;

    #[test]
    fn cancel_resolves_cancelled_future() {
        let token = CancelToken::new();
        let future = token.cancelled();
        token.cancel();
        block_on(future);
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_observed_by_race() {
        let token = CancelToken::new();
        let token_clone = token.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancelled);

        let waiter = async move {
            token_clone.cancelled().await;
            observed.store(true, Ordering::SeqCst);
        };
        let canceller = async {
            // Cancellation must be observable even when the other branch is pending.
            futures::future::poll_fn(|_| {
                token.cancel();
                Poll::Ready(())
            })
            .await;
        };
        block_on(join(waiter, canceller));
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn pending_waiter_does_not_hang_on_cancel() {
        let token = CancelToken::new();
        let token_clone = token.clone();
        let waiter = async move {
            futures::future::select(Box::pin(pending::<()>()), Box::pin(token_clone.cancelled()))
                .await;
        };
        token.cancel();
        block_on(waiter);
    }
}
