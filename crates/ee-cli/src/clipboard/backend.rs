//! Platform clipboard worker built on `arboard`.
//!
//! X11 and Wayland serve clipboard reads from the process that owns the
//! selection, so the `arboard` handle must outlive each request; Windows also
//! expects clipboard operations to come from a single thread. A dedicated
//! worker owns the handle and serves requests over a channel, which keeps
//! blocking reads off the UI thread.
//!
//! The worker also applies the transport policy: it tries the planned
//! transports in order, retries a transient `ClipboardOccupied`, and records at
//! most one status message per failure streak. Keeping this on the worker means
//! the UI thread only ever sends a message, never waits for a display server.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(all(unix, not(target_os = "macos")))]
use std::time::Instant;

use super::ErrorSlot;
use super::osc52::{self, Osc52Error};
use super::policy::{
    ClipboardSelection, FailureAction, TerminalWrap, Transport, TransportPlan, classify_read_error,
    classify_write_error,
};

/// Bounded wait for a read when a cached value can cover a timeout.
pub(super) const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Bounded wait for a read when nothing is cached. The first read also pays for
/// spawning the worker and connecting to the display server, and falling back
/// to an empty value there would silently paste nothing.
pub(super) const COLD_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Delay before retrying a transport that reported `ClipboardOccupied`.
const OCCUPIED_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Upper bound on the shutdown wait that keeps the last yank alive when no
/// clipboard manager is running.
#[cfg(all(unix, not(target_os = "macos")))]
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

pub(super) enum Request {
    Write {
        selection: ClipboardSelection,
        text: String,
        plan: TransportPlan,
        wrap: TerminalWrap,
        max_bytes: usize,
    },
    Read {
        selection: ClipboardSelection,
        transport: Option<Transport>,
        reply: SyncSender<Option<String>>,
    },
    /// Stop the worker; `wait` holds the selection briefly before dropping it.
    Shutdown { wait: bool },
}

/// Request channel plus the worker's join handle.
pub(super) struct Handle {
    pub(super) sender: Sender<Request>,
    pub(super) join: Mutex<Option<JoinHandle<()>>>,
}

/// Spawn the worker thread and return its handle.
///
/// `None` means the thread could not be started; the caller then reports the
/// write as unaccepted.
pub(super) fn spawn(errors: Arc<ErrorSlot>) -> Option<Handle> {
    let (sender, receiver) = mpsc::channel();
    let join = thread::Builder::new()
        .name(String::from("ee-clipboard"))
        .spawn(move || run(receiver, errors))
        .ok()?;
    Some(Handle { sender, join: Mutex::new(Some(join)) })
}

fn run(receiver: Receiver<Request>, errors: Arc<ErrorSlot>) {
    let mut backend = Backend { clipboard: None, last_arboard: None };
    while let Ok(request) = receiver.recv() {
        match request {
            Request::Write { selection, text, plan, wrap, max_bytes } => {
                backend.write(selection, &text, plan, wrap, max_bytes, &errors);
            }
            Request::Read { selection, transport, reply } => {
                let value = backend.read(selection, transport, &errors);
                let _ = reply.send(value);
            }
            Request::Shutdown { wait } => {
                backend.shutdown(wait);
                break;
            }
        }
    }
}

/// Outcome of one transport attempt.
#[derive(Debug)]
enum Attempt {
    /// The transport accepted the write.
    Done,
    /// The transport declined; try the fallback when one exists.
    TryFallback(String),
    /// The transport failed with nothing left to try; surface `message`.
    Fail(String),
}

/// Outcome of an arboard read.
enum ReadOutcome {
    /// Clipboard text.
    Value(String),
    /// Authoritative empty clipboard.
    Empty,
    /// Transport failure; the caller may substitute its cached value.
    Failed,
}

struct Backend {
    clipboard: Option<arboard::Clipboard>,
    /// Last value this process successfully placed on the platform clipboard,
    /// re-asserted on shutdown so a clipboard manager can take it over.
    last_arboard: Option<(ClipboardSelection, String)>,
}

impl Backend {
    /// Try the planned transports in order, surfacing one message on failure.
    fn write(
        &mut self,
        selection: ClipboardSelection,
        text: &str,
        plan: TransportPlan,
        wrap: TerminalWrap,
        max_bytes: usize,
        errors: &ErrorSlot,
    ) {
        let mut last_failure = None;
        for transport in [Some(plan.primary), plan.fallback].into_iter().flatten() {
            let outcome = match transport {
                Transport::Arboard => self.attempt_arboard_write(selection, text),
                Transport::Osc52 => attempt_osc52_write(text, max_bytes, wrap),
            };
            match outcome {
                Attempt::Done => {
                    errors.clear();
                    return;
                }
                Attempt::TryFallback(message) => last_failure = Some(message),
                Attempt::Fail(message) => {
                    errors.record(message);
                    return;
                }
            }
        }
        errors.record(last_failure.unwrap_or_else(|| String::from("system clipboard unavailable")));
    }

    fn attempt_arboard_write(&mut self, selection: ClipboardSelection, text: &str) -> Attempt {
        // Track a real platform write separately from `Done`, which also covers
        // the `Ignore` case where there was nothing to report.
        let mut wrote = false;
        let outcome = {
            let Some(clipboard) = self.handle() else {
                return Attempt::TryFallback(String::from("system clipboard unavailable"));
            };
            match set(clipboard, selection, text) {
                Ok(()) => {
                    wrote = true;
                    Attempt::Done
                }
                Err(error) => match classify_write_error(&error) {
                    FailureAction::Retry => {
                        thread::sleep(OCCUPIED_RETRY_DELAY);
                        match set(clipboard, selection, text) {
                            Ok(()) => {
                                wrote = true;
                                Attempt::Done
                            }
                            Err(error) => {
                                Attempt::TryFallback(format!("clipboard write failed: {error}"))
                            }
                        }
                    }
                    FailureAction::Fallback => {
                        Attempt::TryFallback(format!("clipboard write failed: {error}"))
                    }
                    FailureAction::Ignore => Attempt::Done,
                    FailureAction::Surface => {
                        Attempt::Fail(format!("clipboard write failed: {error}"))
                    }
                },
            }
        };
        if wrote {
            self.last_arboard = Some((selection, text.to_owned()));
        }
        outcome
    }

    fn read(
        &mut self,
        selection: ClipboardSelection,
        transport: Option<Transport>,
        errors: &ErrorSlot,
    ) -> Option<String> {
        if transport != Some(Transport::Arboard) {
            return None;
        }
        let outcome = {
            let Some(clipboard) = self.handle() else {
                errors.record(String::from("system clipboard unavailable"));
                return None;
            };
            match get(clipboard, selection) {
                Ok(text) => ReadOutcome::Value(text),
                Err(error) => match classify_read_error(&error) {
                    // An empty clipboard is not a transport failure: report it
                    // as authoritative so the caller does not substitute a
                    // stale cached value.
                    FailureAction::Ignore => ReadOutcome::Empty,
                    FailureAction::Retry => {
                        thread::sleep(OCCUPIED_RETRY_DELAY);
                        match get(clipboard, selection) {
                            Ok(text) => ReadOutcome::Value(text),
                            Err(_) => ReadOutcome::Failed,
                        }
                    }
                    FailureAction::Fallback | FailureAction::Surface => {
                        errors.record(format!("clipboard read failed: {error}"));
                        ReadOutcome::Failed
                    }
                },
            }
        };
        match outcome {
            ReadOutcome::Value(text) => {
                errors.clear();
                Some(text)
            }
            ReadOutcome::Empty => {
                errors.clear();
                Some(String::new())
            }
            ReadOutcome::Failed => None,
        }
    }

    /// Re-assert the last platform-clipboard value for a bounded window so a
    /// clipboard manager can take ownership before the process exits.
    fn shutdown(&mut self, wait: bool) {
        let last = self.last_arboard.take();
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if wait
                && let Some((selection, text)) = last
                && let Some(clipboard) = self.clipboard.as_mut()
            {
                use arboard::SetExtLinux as _;
                let deadline = Instant::now() + SHUTDOWN_WAIT;
                let _ = clipboard
                    .set()
                    .clipboard(selection_kind(selection))
                    .wait_until(deadline)
                    .text(text);
            }
        }
        // macOS and Windows keep the clipboard after the process exits.
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            let _ = (wait, last);
        }
    }

    /// Lazily create the platform clipboard.
    ///
    /// Creation fails in headless sessions; a failed attempt is retried on the
    /// next request rather than latching the failure.
    fn handle(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }
}

fn attempt_osc52_write(text: &str, max_bytes: usize, wrap: TerminalWrap) -> Attempt {
    match osc52::write(text, max_bytes, wrap) {
        Ok(()) => Attempt::Done,
        // Over the cap: skip and report rather than fall back, because the
        // fallback would target the wrong clipboard over SSH.
        Err(error @ Osc52Error::TooLarge { .. }) => Attempt::Fail(error.message()),
        Err(error) => Attempt::TryFallback(error.message()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn selection_kind(selection: ClipboardSelection) -> arboard::LinuxClipboardKind {
    match selection {
        ClipboardSelection::Clipboard => arboard::LinuxClipboardKind::Clipboard,
        ClipboardSelection::Primary => arboard::LinuxClipboardKind::Primary,
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set(
    clipboard: &mut arboard::Clipboard,
    selection: ClipboardSelection,
    text: &str,
) -> Result<(), arboard::Error> {
    use arboard::SetExtLinux as _;
    clipboard.set().clipboard(selection_kind(selection)).text(text.to_owned())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn get(
    clipboard: &mut arboard::Clipboard,
    selection: ClipboardSelection,
) -> Result<String, arboard::Error> {
    use arboard::GetExtLinux as _;
    clipboard.get().clipboard(selection_kind(selection)).text()
}

// macOS and Windows have no primary selection, so `*` targets the same
// clipboard as `+`.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn set(
    clipboard: &mut arboard::Clipboard,
    selection: ClipboardSelection,
    text: &str,
) -> Result<(), arboard::Error> {
    let _ = selection;
    clipboard.set_text(text.to_owned())
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn get(
    clipboard: &mut arboard::Clipboard,
    selection: ClipboardSelection,
) -> Result<String, arboard::Error> {
    let _ = selection;
    clipboard.get_text()
}
