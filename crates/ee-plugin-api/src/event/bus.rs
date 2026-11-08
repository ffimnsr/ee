//! Synchronous event bus used on the editor hot path.
//!
//! Dispatch never holds the registry lock while running a listener, so a
//! listener may register or dispose listeners re-entrantly. Nested dispatch is
//! capped, listener panics are isolated, and listener counts are bounded.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::disposer::Disposer;
use crate::event::{Event, EventError, MAX_DISPATCH_DEPTH, MAX_LISTENERS};

type EmitListener = Arc<dyn Fn(&dyn Any) + Send + Sync + 'static>;
type WaterfallListener = Arc<dyn Fn(&dyn Any, &mut bool) + Send + Sync + 'static>;
type SerialListener = Arc<dyn Fn(&dyn Any, &mut dyn Any) + Send + Sync + 'static>;
type BailListener = Arc<dyn Fn(&dyn Any) -> Option<Box<dyn Any>> + Send + Sync + 'static>;

struct Entry<L> {
    id: u64,
    listener: L,
}

#[derive(Default)]
struct BusState {
    emit: HashMap<TypeId, Vec<Entry<EmitListener>>>,
    waterfall: HashMap<TypeId, Vec<Entry<WaterfallListener>>>,
    serial: HashMap<TypeId, Vec<Entry<SerialListener>>>,
    bail: HashMap<TypeId, Vec<Entry<BailListener>>>,
    next_id: u64,
}

impl BusState {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// State passed to a waterfall listener.
///
/// A listener must call [`Waterfall::proceed`] to continue dispatch. Not calling
/// it short-circuits the waterfall.
pub struct Waterfall<'a, E> {
    event: &'a E,
    proceed: bool,
}

impl<'a, E> Waterfall<'a, E> {
    fn new(event: &'a E) -> Self {
        Self { event, proceed: false }
    }

    /// The event being dispatched.
    pub fn event(&self) -> &E {
        self.event
    }

    /// Continue dispatch to the next listener.
    pub fn proceed(&mut self) {
        self.proceed = true;
    }

    /// Stop dispatch explicitly.
    pub fn stop(&mut self) {
        self.proceed = false;
    }

    /// Whether the listener asked to continue.
    pub fn is_proceeding(&self) -> bool {
        self.proceed
    }
}

struct DepthGuard<'a> {
    depth: &'a AtomicUsize,
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.depth.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Cloneable handle to a synchronous event bus.
#[derive(Clone, Default)]
pub struct EventBus {
    state: Arc<Mutex<BusState>>,
    depth: Arc<AtomicUsize>,
    panics: Arc<AtomicUsize>,
}

impl EventBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BusState> {
        self.state.lock().expect("event bus lock poisoned")
    }

    fn enter_dispatch(&self) -> Result<DepthGuard<'_>, EventError> {
        let previous = self.depth.fetch_add(1, Ordering::SeqCst);
        if previous >= MAX_DISPATCH_DEPTH {
            self.depth.fetch_sub(1, Ordering::SeqCst);
            return Err(EventError::DispatchDepth { max: MAX_DISPATCH_DEPTH });
        }
        Ok(DepthGuard { depth: &self.depth })
    }

    /// Register an observe-only listener in registration order.
    pub fn on<E: Event>(
        &self,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_emit::<E>(listener, false)
    }

    /// Register an observe-only listener before existing listeners.
    pub fn on_prepend<E: Event>(
        &self,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_emit::<E>(listener, true)
    }

    fn register_emit<E: Event>(
        &self,
        listener: impl Fn(&E) + Send + Sync + 'static,
        prepend: bool,
    ) -> Result<Disposer, EventError> {
        let erased: EmitListener = Arc::new(move |any: &dyn Any| {
            if let Some(event) = any.downcast_ref::<E>() {
                listener(event);
            }
        });

        let type_id = TypeId::of::<E>();
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.emit.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name: E::NAME, max: MAX_LISTENERS });
            }
            if prepend {
                entries.insert(0, Entry { id, listener: erased });
            } else {
                entries.push(Entry { id, listener: erased });
            }
            id
        };

        let state = Arc::clone(&self.state);
        Ok(Disposer::new(move || {
            if let Ok(mut state) = state.lock()
                && let Some(entries) = state.emit.get_mut(&type_id)
            {
                entries.retain(|entry| entry.id != id);
            }
        }))
    }

    /// Register a waterfall listener.
    ///
    /// Each listener receives a [`Waterfall`] state and must call `proceed()` to
    /// continue dispatch.
    pub fn on_waterfall<E: Event>(
        &self,
        listener: impl Fn(&mut Waterfall<'_, E>) + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_waterfall::<E>(listener, false)
    }

    /// Register a waterfall listener before existing listeners.
    pub fn on_waterfall_prepend<E: Event>(
        &self,
        listener: impl Fn(&mut Waterfall<'_, E>) + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_waterfall::<E>(listener, true)
    }

    fn register_waterfall<E: Event>(
        &self,
        listener: impl Fn(&mut Waterfall<'_, E>) + Send + Sync + 'static,
        prepend: bool,
    ) -> Result<Disposer, EventError> {
        let erased: WaterfallListener = Arc::new(move |any: &dyn Any, proceed: &mut bool| {
            if let Some(event) = any.downcast_ref::<E>() {
                let mut state = Waterfall::new(event);
                listener(&mut state);
                *proceed = state.is_proceeding();
            }
        });

        let type_id = TypeId::of::<E>();
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.waterfall.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name: E::NAME, max: MAX_LISTENERS });
            }
            if prepend {
                entries.insert(0, Entry { id, listener: erased });
            } else {
                entries.push(Entry { id, listener: erased });
            }
            id
        };

        let state = Arc::clone(&self.state);
        Ok(Disposer::new(move || {
            if let Ok(mut state) = state.lock()
                && let Some(entries) = state.waterfall.get_mut(&type_id)
            {
                entries.retain(|entry| entry.id != id);
            }
        }))
    }

    /// Register a serial listener that folds an accumulator.
    pub fn on_serial<E: Event, R: 'static>(
        &self,
        listener: impl Fn(&E, &mut R) + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        let erased: SerialListener = Arc::new(move |any: &dyn Any, acc: &mut dyn Any| {
            if let (Some(event), Some(acc)) = (any.downcast_ref::<E>(), acc.downcast_mut::<R>()) {
                listener(event, acc);
            }
        });

        let type_id = TypeId::of::<E>();
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.serial.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name: E::NAME, max: MAX_LISTENERS });
            }
            entries.push(Entry { id, listener: erased });
            id
        };

        let state = Arc::clone(&self.state);
        Ok(Disposer::new(move || {
            if let Ok(mut state) = state.lock()
                && let Some(entries) = state.serial.get_mut(&type_id)
            {
                entries.retain(|entry| entry.id != id);
            }
        }))
    }

    /// Register a bail listener.
    ///
    /// Returning `Some(value)` stops dispatch and wins. `None` continues.
    pub fn on_bail<E: Event, R: 'static>(
        &self,
        listener: impl Fn(&E) -> Option<R> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        let erased: BailListener = Arc::new(move |any: &dyn Any| {
            any.downcast_ref::<E>().and_then(&listener).map(|value| Box::new(value) as Box<dyn Any>)
        });

        let type_id = TypeId::of::<E>();
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.bail.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name: E::NAME, max: MAX_LISTENERS });
            }
            entries.push(Entry { id, listener: erased });
            id
        };

        let state = Arc::clone(&self.state);
        Ok(Disposer::new(move || {
            if let Ok(mut state) = state.lock()
                && let Some(entries) = state.bail.get_mut(&type_id)
            {
                entries.retain(|entry| entry.id != id);
            }
        }))
    }

    /// Observe-only dispatch in registration order.
    pub fn emit<E: Event>(&self, event: &E) -> Result<(), EventError> {
        let _guard = self.enter_dispatch()?;
        let listeners = self.snapshot_emit::<E>();
        for listener in listeners {
            if catch_unwind(AssertUnwindSafe(|| listener(event))).is_err() {
                self.panics.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Waterfall dispatch in registration order.
    ///
    /// Dispatch stops at the first listener that does not call `proceed()`.
    pub fn waterfall<E: Event>(&self, event: &E) -> Result<(), EventError> {
        let _guard = self.enter_dispatch()?;
        let listeners = self.snapshot_waterfall::<E>();
        for listener in listeners {
            let mut proceed = false;
            if catch_unwind(AssertUnwindSafe(|| listener(event, &mut proceed))).is_err() {
                self.panics.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if !proceed {
                break;
            }
        }
        Ok(())
    }

    /// Serial dispatch; returns the final accumulator value.
    pub fn serial<E: Event, R: 'static>(&self, event: &E, initial: R) -> Result<R, EventError> {
        let _guard = self.enter_dispatch()?;
        let listeners = self.snapshot_serial::<E>();
        let mut accumulator = initial;
        for listener in listeners {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                listener(event, &mut accumulator as &mut dyn Any)
            }));
        }
        Ok(accumulator)
    }

    /// Bail dispatch; the first `Some` result wins.
    pub fn bail<E: Event, R: 'static>(&self, event: &E) -> Result<Option<R>, EventError> {
        let _guard = self.enter_dispatch()?;
        let listeners = self.snapshot_bail::<E>();
        for listener in listeners {
            let Ok(result) = catch_unwind(AssertUnwindSafe(|| listener(event))) else {
                self.panics.fetch_add(1, Ordering::SeqCst);
                continue;
            };
            if let Some(value) = result {
                let value = value
                    .downcast::<R>()
                    .map_err(|_| EventError::BailTypeMismatch { name: E::NAME })?;
                return Ok(Some(*value));
            }
        }
        Ok(None)
    }

    /// Number of listeners registered for an event type across all modes.
    pub fn listener_count<E: Event>(&self) -> usize {
        let type_id = TypeId::of::<E>();
        let state = self.lock();
        state.emit.get(&type_id).map_or(0, Vec::len)
            + state.waterfall.get(&type_id).map_or(0, Vec::len)
            + state.serial.get(&type_id).map_or(0, Vec::len)
            + state.bail.get(&type_id).map_or(0, Vec::len)
    }

    /// Number of listener panics isolated by this bus.
    pub fn panic_count(&self) -> usize {
        self.panics.load(Ordering::SeqCst)
    }

    fn snapshot_emit<E: Event>(&self) -> Vec<EmitListener> {
        let state = self.lock();
        state
            .emit
            .get(&TypeId::of::<E>())
            .map(|entries| entries.iter().map(|entry| Arc::clone(&entry.listener)).collect())
            .unwrap_or_default()
    }

    fn snapshot_waterfall<E: Event>(&self) -> Vec<WaterfallListener> {
        let state = self.lock();
        state
            .waterfall
            .get(&TypeId::of::<E>())
            .map(|entries| entries.iter().map(|entry| Arc::clone(&entry.listener)).collect())
            .unwrap_or_default()
    }

    fn snapshot_serial<E: Event>(&self) -> Vec<SerialListener> {
        let state = self.lock();
        state
            .serial
            .get(&TypeId::of::<E>())
            .map(|entries| entries.iter().map(|entry| Arc::clone(&entry.listener)).collect())
            .unwrap_or_default()
    }

    fn snapshot_bail<E: Event>(&self) -> Vec<BailListener> {
        let state = self.lock();
        state
            .bail
            .get(&TypeId::of::<E>())
            .map(|entries| entries.iter().map(|entry| Arc::clone(&entry.listener)).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]

    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Debug)]
    struct Observed(u32);
    impl Event for Observed {
        const NAME: &'static str = "test.observed";
    }

    #[derive(Debug)]
    struct Chained(u32);
    impl Event for Chained {
        const NAME: &'static str = "test.chained";
    }

    #[derive(Debug)]
    struct Accumulated;
    impl Event for Accumulated {
        const NAME: &'static str = "test.accumulated";
    }

    #[derive(Debug)]
    struct Gated;
    impl Event for Gated {
        const NAME: &'static str = "test.gated";
    }

    #[test]
    fn emit_preserves_registration_order_and_supports_prepend() {
        let bus = EventBus::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        let first = Arc::clone(&order);
        bus.on(move |event: &Observed| first.lock().unwrap().push(event.0)).unwrap();
        let second = Arc::clone(&order);
        bus.on(move |event: &Observed| second.lock().unwrap().push(event.0 + 10)).unwrap();
        let third = Arc::clone(&order);
        bus.on_prepend(move |event: &Observed| third.lock().unwrap().push(event.0 + 100)).unwrap();

        bus.emit(&Observed(1)).unwrap();
        assert_eq!(*order.lock().unwrap(), vec![101, 1, 11]);
    }

    #[test]
    fn emit_isolates_listener_panics() {
        let bus = EventBus::new();
        let ran = Arc::new(AtomicUsize::new(0));
        bus.on(|_: &Observed| panic!("listener failed")).unwrap();
        let ran_clone = Arc::clone(&ran);
        bus.on(move |_: &Observed| {
            ran_clone.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

        bus.emit(&Observed(1)).unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(bus.panic_count(), 1);
    }

    #[test]
    fn waterfall_short_circuits() {
        let bus = EventBus::new();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let first = Arc::clone(&seen);
        bus.on_waterfall(move |state: &mut Waterfall<'_, Chained>| {
            first.lock().unwrap().push(state.event().0);
            // Do not proceed: dispatch stops here.
        })
        .unwrap();
        let second = Arc::clone(&seen);
        bus.on_waterfall(move |state: &mut Waterfall<'_, Chained>| {
            second.lock().unwrap().push(state.event().0 + 1);
            state.proceed();
        })
        .unwrap();

        bus.waterfall(&Chained(1)).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![1]);
    }

    #[test]
    fn serial_returns_last_value() {
        let bus = EventBus::new();
        bus.on_serial(|_: &Accumulated, acc: &mut u32| *acc += 1).unwrap();
        bus.on_serial(|_: &Accumulated, acc: &mut u32| *acc *= 10).unwrap();

        assert_eq!(bus.serial::<Accumulated, u32>(&Accumulated, 0).unwrap(), 10);
    }

    #[test]
    fn bail_first_value_wins() {
        let bus = EventBus::new();
        let later = Arc::new(AtomicUsize::new(0));

        bus.on_bail(|_: &Gated| Some("first")).unwrap();
        let later_clone = Arc::clone(&later);
        bus.on_bail(move |_: &Gated| {
            later_clone.fetch_add(1, Ordering::SeqCst);
            Some("second")
        })
        .unwrap();

        assert_eq!(bus.bail::<Gated, &'static str>(&Gated).unwrap(), Some("first"));
        assert_eq!(later.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn listener_limit_is_enforced() {
        let bus = EventBus::new();
        for _ in 0..MAX_LISTENERS {
            bus.on(|_: &Observed| {}).unwrap();
        }
        assert_eq!(
            bus.on(|_: &Observed| {}).unwrap_err(),
            EventError::ListenerLimit { name: Observed::NAME, max: MAX_LISTENERS }
        );
    }

    #[test]
    fn dispatch_depth_is_capped() {
        let bus = EventBus::new();
        let deepest_error = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let nested_bus = bus.clone();
        let error = Arc::clone(&deepest_error);
        bus.on(move |_: &Observed| {
            if nested_bus.emit(&Observed(0)).is_err() {
                error.store(true, Ordering::SeqCst);
            }
        })
        .unwrap();

        // Recursion stops at the depth cap and reports an error.
        let _ = bus.emit(&Observed(1));
        assert!(deepest_error.load(Ordering::SeqCst));
    }

    #[test]
    fn disposer_removes_listener() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let disposer = bus
            .on(move |_: &Observed| {
                count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();

        bus.emit(&Observed(1)).unwrap();
        disposer.dispose();
        bus.emit(&Observed(1)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
