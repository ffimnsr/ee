//! System clipboard transport.
//!
//! Registers live in [`crate::registers`]; this module owns only the OS
//! transport. Writes go to the terminal clipboard through OSC 52 (which works
//! over SSH with no display server) and to the platform clipboard through
//! `arboard`. The `arboard` handle is owned by a worker thread because X11 and
//! Wayland serve clipboard reads from the process that owns the selection, so
//! the handle must outlive each request, and because a clipboard read can block
//! on the owning application.
//!
//! The UI thread never waits on a display server: writes and reads are channel
//! messages to the worker, which applies the transport policy from
//! [`policy`]. Failures come back through [`ErrorSlot`] and are drained once per
//! failure streak by the status bar.

#[cfg(not(test))]
mod backend;
mod osc52;
mod policy;
#[cfg(test)]
mod test_backend;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

#[cfg(not(test))]
use std::sync::OnceLock;
#[cfg(not(test))]
use std::sync::mpsc;

pub(crate) use policy::{
    ClipboardBackendKind, ClipboardConfig, ClipboardEnvironment, ClipboardRouting,
    ClipboardSelection, DEFAULT_OSC52_MAX_BYTES,
};

/// Seed the fake clipboard for the current test.
#[cfg(test)]
pub(crate) fn set_test_clipboard(selection: ClipboardSelection, text: impl Into<String>) {
    test_backend::set_test_clipboard(selection, text);
}

/// Last clipboard failure, reported to the status bar once per failure streak.
///
/// A streak is a run of identical failures: the first is queued, exact repeats
/// are dropped, and any success clears the streak so the next failure is queued
/// again. This keeps a hung or unsupported clipboard from flooding the status
/// line.
#[derive(Default)]
pub(crate) struct ErrorSlot {
    state: Mutex<ErrorState>,
}

#[derive(Default)]
struct ErrorState {
    /// Message waiting to be shown.
    pending: Option<String>,
    /// Message already queued for the current failure streak.
    streak: Option<String>,
}

impl ErrorSlot {
    /// Queue a failure message unless it repeats the current streak.
    fn record(&self, message: String) {
        if let Ok(mut state) = self.state.lock()
            && state.streak.as_deref() != Some(message.as_str())
        {
            state.streak = Some(message.clone());
            state.pending = Some(message);
        }
    }

    /// Clear the streak after a success so the next failure is queued again.
    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.streak = None;
        }
    }

    /// Take the pending status message, if any.
    pub(crate) fn take(&self) -> Option<String> {
        self.state.lock().ok().and_then(|mut state| state.pending.take())
    }
}

/// Cheap, cloneable handle to the process-wide clipboard transport.
///
/// The worker thread starts on first use, so handles created in unit tests
/// never spawn one.
#[derive(Clone)]
pub(crate) struct ClipboardHandle {
    inner: Arc<Inner>,
}

struct Inner {
    #[cfg(not(test))]
    worker: OnceLock<Option<backend::Handle>>,
    /// Transport selection and OSC 52 limits, updated live by `:set`.
    config: Mutex<ClipboardConfig>,
    /// SSH/multiplexer facts, detected once at construction. Read only by the
    /// non-test worker path.
    #[cfg_attr(test, allow(dead_code))]
    environment: ClipboardEnvironment,
    /// Last value this process wrote or read, used when a read cannot reach the
    /// platform clipboard within its bounded wait.
    cache: Mutex<HashMap<ClipboardSelection, String>>,
    /// Shared with the worker so it can report failures back to the UI thread.
    errors: Arc<ErrorSlot>,
}

impl fmt::Debug for ClipboardHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardHandle").finish_non_exhaustive()
    }
}

impl Default for ClipboardHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardHandle {
    pub(crate) fn new() -> Self {
        Self::with_settings(ClipboardConfig::default(), ClipboardEnvironment::detect())
    }

    /// Build a handle from resolved settings and the detected environment.
    pub(crate) fn with_settings(
        config: ClipboardConfig,
        environment: ClipboardEnvironment,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                #[cfg(not(test))]
                worker: OnceLock::new(),
                config: Mutex::new(config),
                environment,
                cache: Mutex::new(HashMap::new()),
                errors: Arc::new(ErrorSlot::default()),
            }),
        }
    }

    /// Update the transport preference (`:set clipboard_backend=...`).
    pub(crate) fn set_backend(&self, backend: ClipboardBackendKind) {
        if let Ok(mut config) = self.inner.config.lock() {
            config.backend = backend;
        }
    }

    /// Update the OSC 52 payload cap (`:set clipboard_osc52_max_bytes=...`).
    pub(crate) fn set_osc52_max_bytes(&self, max_bytes: usize) {
        if let Ok(mut config) = self.inner.config.lock() {
            config.osc52_max_bytes = max_bytes;
        }
    }

    /// Update the exit wait (`:set clipboard_wait_on_exit=...`).
    pub(crate) fn set_wait_on_exit(&self, wait: bool) {
        if let Ok(mut config) = self.inner.config.lock() {
            config.wait_on_exit = wait;
        }
    }

    /// Take the pending clipboard failure message, if any.
    pub(crate) fn take_error(&self) -> Option<String> {
        self.inner.errors.take()
    }

    /// Test-only snapshot of the resolved transport settings.
    ///
    /// The worker is not compiled into test builds, so this is the only way to
    /// assert that `:set` reaches the live configuration.
    #[cfg(test)]
    pub(crate) fn test_config(&self) -> ClipboardConfig {
        *self.inner.config.lock().expect("clipboard config poisoned")
    }

    /// Write `text` to the selected system clipboard.
    ///
    /// Returns `true` when the request was handed to the worker. The worker
    /// applies the transport policy and reports failures through
    /// [`ClipboardHandle::take_error`], so the return value only distinguishes
    /// "queued" from "no worker".
    pub(crate) fn write_text(&self, selection: ClipboardSelection, text: String) -> bool {
        self.store_cache(selection, &text);

        #[cfg(test)]
        {
            test_backend::write(selection, &text);
            true
        }
        #[cfg(not(test))]
        {
            let (plan, wrap, max_bytes) = {
                let config = self.inner.config.lock().map(|config| *config).unwrap_or_default();
                (
                    config.backend.write_plan(self.inner.environment.remote),
                    self.inner.environment.wrap,
                    config.osc52_max_bytes,
                )
            };
            self.sender().is_some_and(|handle| {
                handle
                    .sender
                    .send(backend::Request::Write { selection, text, plan, wrap, max_bytes })
                    .is_ok()
            })
        }
    }

    /// Read the selected system clipboard.
    ///
    /// Waits for the worker with a bounded deadline: [`backend::READ_TIMEOUT`]
    /// when a cached value can cover a timeout, [`backend::COLD_READ_TIMEOUT`]
    /// otherwise. On timeout or transport failure the last value this process
    /// wrote or read is returned. An empty clipboard is authoritative — nothing
    /// is pasted and no cached value is substituted.
    pub(crate) fn read_text(&self, selection: ClipboardSelection) -> Option<String> {
        #[cfg(test)]
        let value = test_backend::read(selection);

        #[cfg(not(test))]
        let value = {
            let transport = self
                .inner
                .config
                .lock()
                .map(|config| config.backend.read_transport())
                .unwrap_or(Some(policy::Transport::Arboard));
            match transport {
                // OSC 52 cannot read; the cache is the only fallback.
                None => None,
                Some(transport) => {
                    let deadline = if self.has_cached(selection) {
                        backend::READ_TIMEOUT
                    } else {
                        backend::COLD_READ_TIMEOUT
                    };
                    let (reply, response) = mpsc::sync_channel(1);
                    match self.sender() {
                        Some(handle)
                            if handle
                                .sender
                                .send(backend::Request::Read {
                                    selection,
                                    transport: Some(transport),
                                    reply,
                                })
                                .is_ok() =>
                        {
                            response.recv_timeout(deadline).ok().flatten()
                        }
                        _ => None,
                    }
                }
            }
        };

        match value {
            Some(text) => {
                if !text.is_empty() {
                    self.store_cache(selection, &text);
                }
                Some(text)
            }
            None => self.cached(selection),
        }
    }

    /// Stop the worker, honouring `clipboard_wait_on_exit`.
    ///
    /// Queued requests are processed first; when the wait is enabled the worker
    /// then re-asserts the last value it placed on the platform clipboard for a
    /// bounded window so a clipboard manager can take ownership before the
    /// process exits. A handle that never spawned a worker returns immediately.
    pub(crate) fn shutdown(&self) {
        #[cfg(not(test))]
        if let Some(Some(handle)) = self.inner.worker.get() {
            let wait = self.inner.config.lock().map(|config| config.wait_on_exit).unwrap_or(true);
            let _ = handle.sender.send(backend::Request::Shutdown { wait });
            if let Ok(mut join) = handle.join.lock()
                && let Some(join) = join.take()
            {
                let _ = join.join();
            }
        }
    }

    #[cfg(not(test))]
    fn sender(&self) -> Option<&backend::Handle> {
        self.inner.worker.get_or_init(|| backend::spawn(Arc::clone(&self.inner.errors))).as_ref()
    }

    fn store_cache(&self, selection: ClipboardSelection, text: &str) {
        if let Ok(mut cache) = self.inner.cache.lock() {
            cache.insert(selection, text.to_owned());
        }
    }

    fn cached(&self, selection: ClipboardSelection) -> Option<String> {
        let cache = self.inner.cache.lock().ok()?;
        match cache.get(&selection) {
            Some(text) if !text.is_empty() => Some(text.clone()),
            _ => None,
        }
    }

    #[cfg(not(test))]
    fn has_cached(&self, selection: ClipboardSelection) -> bool {
        self.inner
            .cache
            .lock()
            .map(|cache| cache.get(&selection).is_some_and(|text| !text.is_empty()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_slot_reports_once_per_streak() {
        let slot = ErrorSlot::default();
        assert_eq!(slot.take(), None);

        slot.record(String::from("boom"));
        assert_eq!(slot.take().as_deref(), Some("boom"));
        // The same failure in the same streak is not queued again.
        slot.record(String::from("boom"));
        assert_eq!(slot.take(), None);

        // A different failure is queued.
        slot.record(String::from("other"));
        assert_eq!(slot.take().as_deref(), Some("other"));

        // A success clears the streak, so the original failure reports again.
        slot.clear();
        slot.record(String::from("boom"));
        assert_eq!(slot.take().as_deref(), Some("boom"));
    }

    #[test]
    fn take_error_is_empty_until_a_failure_is_recorded() {
        let handle = ClipboardHandle::new();
        assert_eq!(handle.take_error(), None);
        handle.inner.errors.record(String::from("clipboard write failed"));
        assert_eq!(handle.take_error().as_deref(), Some("clipboard write failed"));
        assert_eq!(handle.take_error(), None);
    }

    #[test]
    fn live_settings_updates_are_visible_to_the_handle() {
        let handle = ClipboardHandle::new();
        handle.set_backend(ClipboardBackendKind::Osc52);
        handle.set_osc52_max_bytes(4096);
        handle.set_wait_on_exit(false);

        let config = *handle.inner.config.lock().unwrap();
        assert_eq!(config.backend, ClipboardBackendKind::Osc52);
        assert_eq!(config.osc52_max_bytes, 4096);
        assert!(!config.wait_on_exit);
    }

    #[test]
    fn shutdown_without_a_worker_is_a_no_op() {
        // A handle that never performed a request must not spawn a thread just
        // to shut it down.
        ClipboardHandle::new().shutdown();
    }
}
