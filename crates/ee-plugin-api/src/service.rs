//! Typed service registry.
//!
//! A service is any `Send + Sync + 'static` value registered under a stable,
//! versioned [`ServiceId`]. Typed handles fail closed: a lookup through a
//! [`ServiceKey<T>`] never downcasts a value registered for a different `T`.

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::ids::{IdError, ServiceId};

/// A host or plugin capability that can be resolved by other plugins.
pub trait Service: Send + Sync + 'static {
    /// Stable, versioned identifier, for example `editor.document@1`.
    const ID: &'static str;
}

/// Typed handle for a service registered under a specific [`ServiceId`].
#[derive(Debug)]
pub struct ServiceKey<T: ?Sized> {
    id: ServiceId,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static> ServiceKey<T> {
    /// Build a key from `T::ID`.
    ///
    /// Panics when the service declares an invalid identifier. That is a
    /// programmer error in a `Service` implementation, not user input.
    pub fn new() -> Self
    where
        T: Service,
    {
        let id = ServiceId::from_static(T::ID);
        Self { id, _marker: PhantomData }
    }

    /// Build a key from an explicit identifier.
    pub fn from_id(id: ServiceId) -> Self {
        Self { id, _marker: PhantomData }
    }

    /// Build a key, validating an identifier string.
    pub fn from_str_id(id: &str) -> Result<Self, IdError> {
        Ok(Self { id: id.parse()?, _marker: PhantomData })
    }

    /// Registered identifier.
    pub fn id(&self) -> &ServiceId {
        &self.id
    }
}

impl<T: Service> Default for ServiceKey<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> Clone for ServiceKey<T> {
    fn clone(&self) -> Self {
        Self { id: self.id.clone(), _marker: PhantomData }
    }
}

/// Errors produced by registration and lookup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("service `{id}` is already registered")]
    Duplicate { id: ServiceId },
    #[error("service `{id}` is registered with a different concrete type")]
    TypeMismatch { id: ServiceId },
    #[error("service `{id}` is not registered")]
    Missing { id: ServiceId },
}

type ErasedService = Arc<dyn Any + Send + Sync>;

/// Registry of services visible to one plugin host.
#[derive(Default)]
pub struct ServiceRegistry {
    services: HashMap<ServiceId, ErasedService>,
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `service` under `T::ID`.
    ///
    /// Duplicate registration is rejected; replacement requires a new id or a
    /// future explicit host policy.
    pub fn register<T: Service>(&mut self, service: Arc<T>) -> Result<(), ServiceError> {
        let id = ServiceId::from_static(T::ID);
        self.register_with_id(id, service)
    }

    /// Register `service` under an explicit identifier.
    ///
    /// Used by builtin providers whose runtime identity is data-driven. The
    /// lookup still checks the concrete `TypeId`.
    pub fn register_with_id<T: 'static + Send + Sync>(
        &mut self,
        id: ServiceId,
        service: Arc<T>,
    ) -> Result<(), ServiceError> {
        if self.services.contains_key(&id) {
            return Err(ServiceError::Duplicate { id });
        }
        self.services.insert(id, service);
        Ok(())
    }

    /// Re-register an already erased service.
    ///
    /// Used by reload rollback to restore providers that were temporarily
    /// removed for a candidate generation.
    pub fn register_erased(
        &mut self,
        id: ServiceId,
        service: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), ServiceError> {
        if self.services.contains_key(&id) {
            return Err(ServiceError::Duplicate { id });
        }
        self.services.insert(id, service);
        Ok(())
    }

    /// Look up a typed service through a key.
    pub fn get<T: 'static + Send + Sync>(&self, key: &ServiceKey<T>) -> Option<Arc<T>> {
        let erased = self.services.get(key.id())?;
        Arc::clone(erased).downcast::<T>().ok()
    }

    /// Look up a typed service or return a typed error.
    pub fn require<T: 'static + Send + Sync>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Arc<T>, ServiceError> {
        let erased = self
            .services
            .get(key.id())
            .ok_or_else(|| ServiceError::Missing { id: key.id().clone() })?;
        Arc::clone(erased)
            .downcast::<T>()
            .map_err(|_| ServiceError::TypeMismatch { id: key.id().clone() })
    }

    /// Remove a service only when it is still the same registered value.
    ///
    /// Reload uses this to prevent a disposed old scope from removing a
    /// service that a newer generation already re-registered under the same
    /// id.
    pub fn remove_if_same(&mut self, id: &ServiceId, service: &Arc<dyn Any + Send + Sync>) -> bool {
        let Some(current) = self.services.get(id) else {
            return false;
        };
        if !Arc::ptr_eq(current, service) {
            return false;
        }
        self.services.remove(id);
        true
    }

    /// Remove a service by identifier.
    pub fn remove(&mut self, id: &ServiceId) -> Option<ErasedService> {
        self.services.remove(id)
    }

    /// Whether an identifier is registered.
    pub fn contains(&self, id: &ServiceId) -> bool {
        self.services.contains_key(id)
    }

    /// Registered identifiers, sorted for deterministic diagnostics.
    pub fn ids(&self) -> Vec<ServiceId> {
        let mut ids: Vec<_> = self.services.keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug)]
    struct CounterService {
        calls: AtomicUsize,
    }

    impl CounterService {
        fn new() -> Self {
            Self { calls: AtomicUsize::new(0) }
        }

        fn bump(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Service for CounterService {
        const ID: &'static str = "test.counter@1";
    }

    #[derive(Debug)]
    struct OtherService;

    impl Service for OtherService {
        const ID: &'static str = "test.other@1";
    }

    #[test]
    fn registers_and_resolves() {
        let mut registry = ServiceRegistry::new();
        let service = Arc::new(CounterService::new());
        registry.register(Arc::clone(&service)).unwrap();

        let resolved = registry.get(&ServiceKey::<CounterService>::new()).unwrap();
        resolved.bump();
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
        assert!(registry.contains(&ServiceId::from_static("test.counter@1")));
    }

    #[test]
    fn rejects_duplicate_registration() {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(CounterService::new())).unwrap();
        let err = registry.register(Arc::new(CounterService::new())).unwrap_err();
        assert_eq!(err, ServiceError::Duplicate { id: ServiceId::from_static("test.counter@1") });
    }

    #[test]
    fn rejects_type_mismatch() {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(CounterService::new())).unwrap();

        // Same id, wrong expected type: fail closed instead of downcasting.
        let wrong_key =
            ServiceKey::<OtherService>::from_id(ServiceId::from_static("test.counter@1"));
        assert!(registry.get(&wrong_key).is_none());
        assert_eq!(
            registry.require(&wrong_key).unwrap_err(),
            ServiceError::TypeMismatch { id: ServiceId::from_static("test.counter@1") }
        );
    }

    #[test]
    fn missing_service_is_reported() {
        let registry = ServiceRegistry::new();
        assert_eq!(
            registry.require(&ServiceKey::<OtherService>::new()).unwrap_err(),
            ServiceError::Missing { id: ServiceId::from_static("test.other@1") }
        );
    }

    #[test]
    fn remove_returns_registered_value() {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(OtherService)).unwrap();
        let id = ServiceId::from_static("test.other@1");
        assert!(registry.remove(&id).is_some());
        assert!(!registry.contains(&id));
    }

    #[test]
    fn remove_if_same_requires_identity() {
        let mut registry = ServiceRegistry::new();
        let id = ServiceId::from_static("test.counter@1");
        let first = Arc::new(CounterService::new());
        registry.register(Arc::clone(&first)).unwrap();
        registry.remove(&id);

        let second = Arc::new(CounterService::new());
        registry.register(Arc::clone(&second)).unwrap();

        let first_erased: Arc<dyn Any + Send + Sync> = first;
        assert!(!registry.remove_if_same(&id, &first_erased));
        assert!(registry.contains(&id));

        let second_erased: Arc<dyn Any + Send + Sync> = second;
        assert!(registry.remove_if_same(&id, &second_erased));
        assert!(!registry.contains(&id));
    }

    #[test]
    fn ids_are_sorted() {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(OtherService)).unwrap();
        registry.register(Arc::new(CounterService::new())).unwrap();
        let ids = registry.ids();
        assert_eq!(ids[0], ServiceId::from_static("test.counter@1"));
        assert_eq!(ids[1], ServiceId::from_static("test.other@1"));
    }
}
