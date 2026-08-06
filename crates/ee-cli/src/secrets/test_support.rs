//! Shared test doubles for the secrets module (`cfg(test)` only).
//!
//! All fakes are per-test owned state: no globals, no shared locks, no real
//! keychain, host-identity source, or filesystem access.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use zeroize::Zeroizing;

use super::{HostBinding, Keychain, SecretStoreError};

/// Keychain double that counts every call and always behaves as "no entry".
/// Proves platform interaction is exactly zero.
pub(crate) struct CountingKeychain {
    loads: Arc<AtomicUsize>,
    stores: Arc<AtomicUsize>,
}

impl CountingKeychain {
    pub(crate) fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let stores = Arc::new(AtomicUsize::new(0));
        (Self { loads: loads.clone(), stores: stores.clone() }, loads, stores)
    }
}

impl Keychain for CountingKeychain {
    fn load(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretStoreError> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    fn store(&self, _service: &str, _account: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
        self.stores.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Result of one keychain load or store call, as seen by [`Keychain`]
/// implementors.
pub(crate) type LoadResult = Result<Option<Zeroizing<Vec<u8>>>, SecretStoreError>;

/// Result of one keychain store call.
pub(crate) type StoreResult = Result<(), SecretStoreError>;

/// Keychain double with scripted per-call results, consumed FIFO.
///
/// Exhausted load scripts fail with [`SecretStoreError::KeychainUnavailable`];
/// exhausted store scripts succeed. Used for read/write failure paths.
pub(crate) struct ScriptedKeychain {
    load_results: Mutex<Vec<LoadResult>>,
    store_results: Mutex<Vec<StoreResult>>,
    loads: AtomicUsize,
    stores: AtomicUsize,
}

impl ScriptedKeychain {
    pub(crate) fn new(load_results: Vec<LoadResult>, store_results: Vec<StoreResult>) -> Self {
        Self {
            load_results: Mutex::new(load_results),
            store_results: Mutex::new(store_results),
            loads: AtomicUsize::new(0),
            stores: AtomicUsize::new(0),
        }
    }

    pub(crate) fn load_calls(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    pub(crate) fn store_calls(&self) -> usize {
        self.stores.load(Ordering::Relaxed)
    }
}

impl Keychain for ScriptedKeychain {
    fn load(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretStoreError> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        let mut results = self.load_results.lock().expect("scripted keychain lock");
        if results.is_empty() {
            return Err(SecretStoreError::KeychainUnavailable);
        }
        results.remove(0)
    }

    fn store(&self, _service: &str, _account: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
        self.stores.fetch_add(1, Ordering::Relaxed);
        let mut results = self.store_results.lock().expect("scripted keychain lock");
        if results.is_empty() {
            return Ok(());
        }
        results.remove(0)
    }
}

/// In-memory keychain double: real load/store semantics without any platform
/// backend. Clones share the same entries and counters.
#[derive(Clone)]
pub(crate) struct StoredKeychain {
    inner: Arc<StoredKeychainInner>,
}

struct StoredKeychainInner {
    entries: Mutex<HashMap<(String, String), Vec<u8>>>,
    loads: AtomicUsize,
    stores: AtomicUsize,
}

impl StoredKeychain {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(StoredKeychainInner {
                entries: Mutex::new(HashMap::new()),
                loads: AtomicUsize::new(0),
                stores: AtomicUsize::new(0),
            }),
        }
    }

    /// Pre-populates an entry (simulating an existing platform credential).
    pub(crate) fn seed(&self, service: &str, account: &str, value: &[u8]) {
        self.inner
            .entries
            .lock()
            .expect("stored keychain lock")
            .insert((service.to_owned(), account.to_owned()), value.to_vec());
    }

    pub(crate) fn stored(&self, service: &str, account: &str) -> Option<Vec<u8>> {
        self.inner
            .entries
            .lock()
            .expect("stored keychain lock")
            .get(&(service.to_owned(), account.to_owned()))
            .cloned()
    }

    pub(crate) fn load_calls(&self) -> usize {
        self.inner.loads.load(Ordering::Relaxed)
    }

    pub(crate) fn store_calls(&self) -> usize {
        self.inner.stores.load(Ordering::Relaxed)
    }
}

impl Keychain for StoredKeychain {
    fn load(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretStoreError> {
        self.inner.loads.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .inner
            .entries
            .lock()
            .expect("stored keychain lock")
            .get(&(service.to_owned(), account.to_owned()))
            .cloned()
            .map(Zeroizing::new))
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        self.inner.stores.fetch_add(1, Ordering::Relaxed);
        self.inner
            .entries
            .lock()
            .expect("stored keychain lock")
            .insert((service.to_owned(), account.to_owned()), value.to_vec());
        Ok(())
    }
}

/// Fixed test binding used by store-level tests.
pub(crate) fn test_binding() -> HostBinding {
    HostBinding::from_digest([0x42; 32])
}
