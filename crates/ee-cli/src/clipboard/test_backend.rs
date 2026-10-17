//! In-process clipboard stand-in for unit tests.
//!
//! Tests must never reach the platform clipboard: it is unavailable in CI, it
//! would spawn the worker thread, and OSC 52 would write escape sequences into
//! the test terminal. The store is thread-local so parallel tests stay isolated.

use std::cell::RefCell;
use std::collections::HashMap;

use super::policy::ClipboardSelection;

thread_local! {
    static TEST_CLIPBOARDS: RefCell<HashMap<ClipboardSelection, String>> =
        RefCell::new(HashMap::new());
}

/// Seed the fake clipboard for the current test.
pub(crate) fn set_test_clipboard(selection: ClipboardSelection, text: impl Into<String>) {
    TEST_CLIPBOARDS.with(|clipboards| {
        clipboards.borrow_mut().insert(selection, text.into());
    });
}

pub(super) fn read(selection: ClipboardSelection) -> Option<String> {
    TEST_CLIPBOARDS.with(|clipboards| clipboards.borrow().get(&selection).cloned())
}

pub(super) fn write(selection: ClipboardSelection, text: &str) {
    TEST_CLIPBOARDS.with(|clipboards| {
        clipboards.borrow_mut().insert(selection, text.to_owned());
    });
}
