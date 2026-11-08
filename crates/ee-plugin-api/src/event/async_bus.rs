//! Asynchronous event bus for agent dispatch.
//!
//! Semantics mirror the sync bus, but listeners return futures and every
//! dispatch point is cancellation-aware.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, Either, join_all, select};

use crate::disposer::Disposer;
use crate::event::{CancelToken, Event, EventError, MAX_LISTENERS};

type ErasedEvent = Arc<dyn Any + Send + Sync>;

type EmitListener = Arc<dyn Fn(ErasedEvent) -> BoxFuture<'static, ()> + Send + Sync + 'static>;
type WaterfallListener =
    Arc<dyn Fn(ErasedEvent, bool) -> BoxFuture<'static, bool> + Send + Sync + 'static>;
type SerialListener = Arc<
    dyn Fn(ErasedEvent, ErasedEvent) -> BoxFuture<'static, ErasedEvent> + Send + Sync + 'static,
>;
type BailListener = Arc<
    dyn Fn(ErasedEvent) -> BoxFuture<'static, Option<Box<dyn Any + Send + Sync>>>
        + Send
        + Sync
        + 'static,
>;

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

/// Cloneable handle to an asynchronous event bus.
#[derive(Clone, Default)]
pub struct AsyncEventBus {
    state: Arc<Mutex<BusState>>,
}

impl AsyncEventBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BusState> {
        self.state.lock().expect("async event bus lock poisoned")
    }

    /// Register an observe-only listener.
    pub fn on<E: Event + Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_emit::<E>(listener, false)
    }

    /// Register an observe-only listener before existing listeners.
    pub fn on_prepend<E: Event + Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        self.register_emit::<E>(listener, true)
    }

    fn register_emit<E: Event + Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>) -> BoxFuture<'static, ()> + Send + Sync + 'static,
        prepend: bool,
    ) -> Result<Disposer, EventError> {
        let erased: EmitListener =
            Arc::new(move |event: ErasedEvent| match event.downcast::<E>() {
                Ok(event) => listener(event),
                Err(_) => Box::pin(async {}),
            });

        self.insert_emit(TypeId::of::<E>(), E::NAME, erased, prepend)
    }

    fn insert_emit(
        &self,
        type_id: TypeId,
        name: &'static str,
        listener: EmitListener,
        prepend: bool,
    ) -> Result<Disposer, EventError> {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.emit.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name, max: MAX_LISTENERS });
            }
            if prepend {
                entries.insert(0, Entry { id, listener });
            } else {
                entries.push(Entry { id, listener });
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
    /// The listener receives the event and whether the previous listener asked
    /// to continue. Returning `true` continues dispatch.
    pub fn on_waterfall<E: Event + Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>, bool) -> BoxFuture<'static, bool> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        let erased: WaterfallListener =
            Arc::new(move |event: ErasedEvent, proceed: bool| match event.downcast::<E>() {
                Ok(event) => listener(event, proceed),
                Err(_) => Box::pin(async move { proceed }),
            });

        self.insert_waterfall(TypeId::of::<E>(), E::NAME, erased)
    }

    fn insert_waterfall(
        &self,
        type_id: TypeId,
        name: &'static str,
        listener: WaterfallListener,
    ) -> Result<Disposer, EventError> {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.waterfall.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name, max: MAX_LISTENERS });
            }
            entries.push(Entry { id, listener });
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
    pub fn on_serial<E: Event + Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>, Arc<R>) -> BoxFuture<'static, R> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        let listener = Arc::new(listener);
        let erased: SerialListener = Arc::new(move |event: ErasedEvent, acc: ErasedEvent| {
            let listener = Arc::clone(&listener);
            let event = match event.downcast::<E>() {
                Ok(event) => event,
                Err(_) => return Box::pin(async { acc }),
            };
            let acc = match acc.downcast::<R>() {
                Ok(acc) => acc,
                Err(_) => {
                    return Box::pin(async { unreachable!("serial accumulator type checked") });
                }
            };
            Box::pin(async move {
                let value = (*listener)(event, acc).await;
                Arc::new(value) as ErasedEvent
            })
        });

        self.insert_serial(TypeId::of::<E>(), E::NAME, erased)
    }

    fn insert_serial(
        &self,
        type_id: TypeId,
        name: &'static str,
        listener: SerialListener,
    ) -> Result<Disposer, EventError> {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.serial.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name, max: MAX_LISTENERS });
            }
            entries.push(Entry { id, listener });
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

    /// Register a bail listener. Returning `Some(value)` wins.
    pub fn on_bail<E: Event + Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        listener: impl Fn(Arc<E>) -> BoxFuture<'static, Option<R>> + Send + Sync + 'static,
    ) -> Result<Disposer, EventError> {
        let erased: BailListener =
            Arc::new(move |event: ErasedEvent| match event.downcast::<E>() {
                Ok(event) => {
                    let future = listener(event);
                    Box::pin(async move {
                        future.await.map(|value| Box::new(value) as Box<dyn Any + Send + Sync>)
                    }) as BoxFuture<'static, Option<Box<dyn Any + Send + Sync>>>
                }
                Err(_) => Box::pin(async { None }),
            });

        self.insert_bail(TypeId::of::<E>(), E::NAME, erased)
    }

    fn insert_bail(
        &self,
        type_id: TypeId,
        name: &'static str,
        listener: BailListener,
    ) -> Result<Disposer, EventError> {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            let entries = state.bail.entry(type_id).or_default();
            if entries.len() >= MAX_LISTENERS {
                return Err(EventError::ListenerLimit { name, max: MAX_LISTENERS });
            }
            entries.push(Entry { id, listener });
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
    pub async fn emit<E: Event + Send + Sync + 'static>(
        &self,
        event: E,
        cancel: &CancelToken,
    ) -> Result<(), EventError> {
        let event: ErasedEvent = Arc::new(event);
        for listener in self.snapshot_emit::<E>() {
            if wait(listener(event.clone()), cancel).await.is_none() {
                return Err(EventError::Cancelled { name: E::NAME });
            }
        }
        Ok(())
    }

    /// Run every listener concurrently.
    pub async fn parallel<E: Event + Send + Sync + 'static>(
        &self,
        event: E,
        cancel: &CancelToken,
    ) -> Result<(), EventError> {
        let event: ErasedEvent = Arc::new(event);
        let futures = self.snapshot_emit::<E>().into_iter().map(|listener| listener(event.clone()));
        let all = join_all(futures);
        if wait(all, cancel).await.is_none() {
            return Err(EventError::Cancelled { name: E::NAME });
        }
        Ok(())
    }

    /// Waterfall dispatch; stops when a listener returns `false`.
    pub async fn waterfall<E: Event + Send + Sync + 'static>(
        &self,
        event: E,
        cancel: &CancelToken,
    ) -> Result<(), EventError> {
        let event: ErasedEvent = Arc::new(event);
        for listener in self.snapshot_waterfall::<E>() {
            let future = listener(event.clone(), false);
            let Some(proceed) = wait(future, cancel).await else {
                return Err(EventError::Cancelled { name: E::NAME });
            };
            if !proceed {
                break;
            }
        }
        Ok(())
    }

    /// Serial dispatch; returns the final accumulator.
    pub async fn serial<E: Event + Send + Sync + 'static, R: Send + Sync + Clone + 'static>(
        &self,
        event: E,
        initial: R,
        cancel: &CancelToken,
    ) -> Result<R, EventError> {
        let event: ErasedEvent = Arc::new(event);
        let mut accumulator: ErasedEvent = Arc::new(initial);
        for listener in self.snapshot_serial::<E>() {
            let future = listener(event.clone(), accumulator);
            let Some(next) = wait(future, cancel).await else {
                return Err(EventError::Cancelled { name: E::NAME });
            };
            accumulator = next;
        }
        accumulator
            .downcast::<R>()
            .map(|value| Arc::unwrap_or_clone(value))
            .map_err(|_| EventError::BailTypeMismatch { name: E::NAME })
    }

    /// Bail dispatch; the first `Some` result wins.
    pub async fn bail<E: Event + Send + Sync + 'static, R: Send + Sync + 'static>(
        &self,
        event: E,
        cancel: &CancelToken,
    ) -> Result<Option<R>, EventError> {
        let event: ErasedEvent = Arc::new(event);
        for listener in self.snapshot_bail::<E>() {
            let future = listener(event.clone());
            let Some(result) = wait(future, cancel).await else {
                return Err(EventError::Cancelled { name: E::NAME });
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

    /// Number of listeners registered for an event type.
    pub fn listener_count<E: Event>(&self) -> usize {
        let type_id = TypeId::of::<E>();
        let state = self.lock();
        state.emit.get(&type_id).map_or(0, Vec::len)
            + state.waterfall.get(&type_id).map_or(0, Vec::len)
            + state.serial.get(&type_id).map_or(0, Vec::len)
            + state.bail.get(&type_id).map_or(0, Vec::len)
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

/// Await a future, returning `None` when the cancellation token wins.
async fn wait<T>(future: impl std::future::Future<Output = T>, cancel: &CancelToken) -> Option<T> {
    let future = std::pin::pin!(future);
    let cancelled = cancel.cancelled();
    match select(future, cancelled).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(((), _)) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_must_use)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::executor::block_on;
    use futures::future::pending;

    use super::*;

    #[derive(Debug)]
    struct AsyncObserved(u32);
    impl Event for AsyncObserved {
        const NAME: &'static str = "test.async.observed";
    }

    #[derive(Debug)]
    struct AsyncChain;
    impl Event for AsyncChain {
        const NAME: &'static str = "test.async.chain";
    }

    #[derive(Debug)]
    struct AsyncFold;
    impl Event for AsyncFold {
        const NAME: &'static str = "test.async.fold";
    }

    #[derive(Debug)]
    struct AsyncGate;
    impl Event for AsyncGate {
        const NAME: &'static str = "test.async.gate";
    }

    #[test]
    fn emit_awaits_every_listener_in_order() {
        let bus = AsyncEventBus::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancelToken::new();

        let first = Arc::clone(&order);
        bus.on(move |event: Arc<AsyncObserved>| {
            let first = Arc::clone(&first);
            Box::pin(async move {
                first.lock().unwrap().push(event.0);
            })
        })
        .unwrap();
        let second = Arc::clone(&order);
        bus.on(move |event: Arc<AsyncObserved>| {
            let second = Arc::clone(&second);
            Box::pin(async move {
                second.lock().unwrap().push(event.0 + 1);
            })
        })
        .unwrap();

        block_on(bus.emit(AsyncObserved(1), &cancel)).unwrap();
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn parallel_runs_every_listener() {
        let bus = AsyncEventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let cancel = CancelToken::new();

        for _ in 0..3 {
            let count = Arc::clone(&count);
            bus.on(move |_event: Arc<AsyncObserved>| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                })
            })
            .unwrap();
        }

        block_on(bus.parallel(AsyncObserved(1), &cancel)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn waterfall_stops_when_listener_returns_false() {
        let bus = AsyncEventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let cancel = CancelToken::new();

        let count_clone = Arc::clone(&count);
        bus.on_waterfall(move |_: Arc<AsyncChain>, _proceed: bool| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { false })
        })
        .unwrap();
        let count_clone = Arc::clone(&count);
        bus.on_waterfall(move |_: Arc<AsyncChain>, _proceed: bool| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { true })
        })
        .unwrap();

        block_on(bus.waterfall(AsyncChain, &cancel)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn serial_returns_last_value() {
        let bus = AsyncEventBus::new();
        let cancel = CancelToken::new();

        bus.on_serial(|_: Arc<AsyncFold>, acc: Arc<u32>| Box::pin(async move { *acc + 1 }))
            .unwrap();
        bus.on_serial(|_: Arc<AsyncFold>, acc: Arc<u32>| Box::pin(async move { *acc * 10 }))
            .unwrap();

        assert_eq!(block_on(bus.serial::<AsyncFold, u32>(AsyncFold, 0, &cancel)).unwrap(), 10);
    }

    #[test]
    fn bail_returns_first_value() {
        let bus = AsyncEventBus::new();
        let later = Arc::new(AtomicUsize::new(0));
        let cancel = CancelToken::new();

        bus.on_bail(|_: Arc<AsyncGate>| Box::pin(async { Some("first") })).unwrap();
        let later_clone = Arc::clone(&later);
        bus.on_bail(move |_: Arc<AsyncGate>| {
            let later = Arc::clone(&later_clone);
            Box::pin(async move {
                later.fetch_add(1, Ordering::SeqCst);
                Some("second")
            })
        })
        .unwrap();

        let winner = block_on(bus.bail::<_, &'static str>(AsyncGate, &cancel)).unwrap();
        assert_eq!(winner, Some("first"));
        assert_eq!(later.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancellation_stops_dispatch() {
        let bus = AsyncEventBus::new();
        let cancel = CancelToken::new();

        bus.on(|_: Arc<AsyncObserved>| Box::pin(pending::<()>())).unwrap();
        cancel.cancel();

        let result = block_on(bus.emit(AsyncObserved(1), &cancel));
        assert_eq!(result, Err(EventError::Cancelled { name: AsyncObserved::NAME }));
    }

    #[test]
    fn disposer_removes_listener() {
        let bus = AsyncEventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let cancel = CancelToken::new();

        let count_clone = Arc::clone(&count);
        let disposer = bus
            .on(move |_event: Arc<AsyncObserved>| {
                let count = Arc::clone(&count_clone);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                })
            })
            .unwrap();

        block_on(bus.emit(AsyncObserved(1), &cancel)).unwrap();
        disposer.dispose();
        block_on(bus.emit(AsyncObserved(1), &cancel)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
