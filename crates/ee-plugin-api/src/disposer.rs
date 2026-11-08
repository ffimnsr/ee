//! Deterministic disposal primitives.
//!
//! Every registration a plugin performs returns a [`Disposer`]. Disposers are
//! idempotent and panic-isolated so a failing cleanup cannot unwind the host or
//! prevent the rest of a scope from shutting down.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

type DisposeFn = Box<dyn FnOnce() + Send + 'static>;

/// A one-shot cleanup handle.
///
/// Clones share the same underlying closure. The first [`Disposer::dispose`]
/// call runs it; later calls are no-ops.
#[must_use = "dropping a Disposer does not run its cleanup; call dispose()"]
#[derive(Clone)]
pub struct Disposer {
    inner: Arc<Mutex<Option<DisposeFn>>>,
}

impl Disposer {
    /// Wrap a cleanup closure.
    pub fn new<F>(dispose: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self { inner: Arc::new(Mutex::new(Some(Box::new(dispose)))) }
    }

    /// Run the cleanup at most once.
    ///
    /// A panicking cleanup is caught and treated as disposed. Panics are
    /// reported by the host at the plugin scope boundary, not rethrown here.
    pub fn dispose(&self) {
        let Some(dispose) = self.inner.lock().expect("disposer lock poisoned").take() else {
            return;
        };
        let _ = catch_unwind(AssertUnwindSafe(dispose));
    }

    /// Whether the cleanup has already run or been taken.
    pub fn is_disposed(&self) -> bool {
        self.inner.lock().expect("disposer lock poisoned").is_none()
    }
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disposer").field("disposed", &self.is_disposed()).finish()
    }
}

/// LIFO stack of disposers.
#[derive(Default)]
pub struct DisposerStack {
    entries: Vec<Disposer>,
}

impl DisposerStack {
    /// Create an empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a disposer.
    pub fn push(&mut self, disposer: Disposer) {
        self.entries.push(disposer);
    }

    /// Number of registered disposers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Dispose every entry in reverse registration order.
    ///
    /// Panics from individual disposers are already isolated by
    /// [`Disposer::dispose`], so this always drains the stack.
    pub fn dispose_all(&mut self) {
        while let Some(disposer) = self.entries.pop() {
            disposer.dispose();
        }
    }
}

impl Drop for DisposerStack {
    fn drop(&mut self) {
        self.dispose_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn dispose_runs_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let disposer = Disposer::new(move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });

        assert!(!disposer.is_disposed());
        disposer.dispose();
        disposer.dispose();
        assert!(disposer.is_disposed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn clones_share_disposal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let disposer = Disposer::new(move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });
        let clone = disposer.clone();

        clone.dispose();
        disposer.dispose();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stack_disposes_lifo() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut stack = DisposerStack::new();
        for value in [1, 2, 3] {
            let order = Arc::clone(&order);
            stack.push(Disposer::new(move || {
                order.lock().unwrap().push(value);
            }));
        }
        stack.dispose_all();
        assert_eq!(*order.lock().unwrap(), vec![3, 2, 1]);
    }

    #[test]
    fn stack_isolates_panics() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut stack = DisposerStack::new();

        let first = Arc::clone(&order);
        stack.push(Disposer::new(move || {
            first.lock().unwrap().push(1);
        }));
        stack.push(Disposer::new(|| panic!("cleanup failed")));
        let third = Arc::clone(&order);
        stack.push(Disposer::new(move || {
            third.lock().unwrap().push(3);
        }));

        stack.dispose_all();
        assert_eq!(*order.lock().unwrap(), vec![3, 1]);
    }

    #[test]
    fn stack_drop_disposes() {
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let mut stack = DisposerStack::new();
            let calls_clone = Arc::clone(&calls);
            stack.push(Disposer::new(move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
