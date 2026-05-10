// Copyright 2026 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Asynchronous, cancellable whole-document scan operations.
//!
//! Expensive commands that must process the entire document text (e.g.
//! `reindent`) run on a background thread so they do not block the UI render
//! loop. Each new invocation cancels any prior task by bumping a generation
//! counter. Results are deposited in a shared slot and picked up by an idle
//! callback in `tabs.rs`.
//!
//! # Policy
//!
//! Whole-scan operations are gated behind [`VlfFeatureGates::whole_doc_ops`].
//! - `Normal` mode (`whole_doc_ops: true`): async execution path taken here.
//! - `ConstrainedNormal` and `Vlf` (`whole_doc_ops: false`): caller alerts the
//!   user and returns before reaching this module.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use xi_rope::{Rope, RopeDelta};

use crate::lang_features;

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Result payload from a completed whole-document scan operation.
pub(crate) enum WholeScanResult {
    /// Completed reindent: `Some(delta)` when changes were produced, `None`
    /// when the document was already correctly indented.
    Reindent(Option<RopeDelta>),
}

// ---------------------------------------------------------------------------
// Task tracker
// ---------------------------------------------------------------------------

/// Tracks an in-flight asynchronous whole-document scan task.
///
/// Only one task runs at a time per buffer. Starting a new task while one is
/// already running cancels the old one by bumping the generation counter; the
/// background thread checks the generation before depositing its result and
/// exits early if it has been superseded.
///
/// Stored as a field of [`crate::editor::Editor`].  Access is single-threaded
/// (all method calls come from the main core event loop or the idle callback);
/// the `Arc<Mutex<…>>` is only needed to share the result slot with the
/// background thread.
pub(crate) struct WholeScanTask {
    /// Monotonically increasing generation counter.  Bumped on every
    /// `start_*` call to signal cancellation to any running thread.
    generation: Arc<AtomicU64>,
    /// Result slot deposited by the background thread: `(generation, result)`.
    result: Arc<Mutex<Option<(u64, WholeScanResult)>>>,
    /// Background thread handle.  Dropped (and joined when the OS reclaims it)
    /// when a new task starts or when the `WholeScanTask` is dropped.
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WholeScanTask {
    pub(crate) fn new() -> Self {
        WholeScanTask {
            generation: Arc::new(AtomicU64::new(0)),
            result: Arc::new(Mutex::new(None)),
            handle: None,
        }
    }

    /// Returns `true` when a background task is currently running.
    #[allow(dead_code)]
    pub(crate) fn is_in_progress(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Cancel any running task and start an async reindent operation.
    ///
    /// `text` must be a cheap [`Rope`] clone (copy-on-write, O(1)) of the
    /// current buffer.  The background thread owns this snapshot and does not
    /// touch the live buffer.
    ///
    /// The result is deposited in the shared slot once the thread completes.
    /// Call [`poll`] from the idle callback to retrieve it.
    pub(crate) fn start_reindent(
        &mut self,
        text: Rope,
        line_ranges: Vec<(usize, usize)>,
        lang_name: String,
        indent_str: String,
    ) -> u64 {
        // Bump generation → old thread will see a stale value and bail before
        // depositing its result.
        let task_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        // Drop old handle — thread will exit on its own when it checks the
        // generation; we do not block waiting for it.
        drop(self.handle.take());

        let gen_arc = Arc::clone(&self.generation);
        let result_arc = Arc::clone(&self.result);

        self.handle = Some(
            std::thread::Builder::new()
                .name("xi-whole-scan-reindent".into())
                .spawn(move || {
                    // Early bail if already superseded before the expensive work.
                    if gen_arc.load(Ordering::Acquire) != task_gen {
                        return;
                    }

                    let delta =
                        lang_features::reindent(&text, &line_ranges, &lang_name, &indent_str);

                    // Post-computation bail: do not deposit a stale result.
                    if gen_arc.load(Ordering::Acquire) != task_gen {
                        return;
                    }

                    *result_arc.lock().unwrap() =
                        Some((task_gen, WholeScanResult::Reindent(delta)));
                })
                .expect("failed to spawn whole-scan thread"),
        );

        task_gen
    }

    /// Poll for the most recently completed result.
    ///
    /// Returns `Some(result)` if the current generation has deposited a result,
    /// clearing the slot so the same result is not returned twice.  Returns
    /// `None` when no result is available (task still running, was cancelled,
    /// or was already consumed).
    pub(crate) fn poll(&mut self) -> Option<WholeScanResult> {
        let current_gen = self.generation.load(Ordering::Acquire);
        let mut slot = self.result.lock().unwrap();
        match slot.as_ref() {
            Some((task_gen, _)) if *task_gen == current_gen => slot.take().map(|(_, r)| r),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_poll_returns_none() {
        let mut task = WholeScanTask::new();
        assert!(task.poll().is_none());
        assert!(!task.is_in_progress());
    }

    #[test]
    fn reindent_task_completes_and_poll_returns_result() {
        // "Rust" is known to syntect; reindent may or may not produce a delta
        // depending on the content, but the result slot must be populated.
        let text = Rope::from("fn foo() {\n}\n");
        let mut task = WholeScanTask::new();
        task.start_reindent(text, vec![(0, 2)], "Rust".to_string(), "    ".to_string());
        // Spin-wait up to 2 s for the background thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(r) = task.poll() {
                assert!(matches!(r, WholeScanResult::Reindent(_)));
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("reindent task did not complete within 2 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Second poll must be None (slot cleared).
        assert!(task.poll().is_none());
    }

    #[test]
    fn starting_new_task_supersedes_old_result() {
        let text = Rope::from("fn foo() {\n}\n");
        let mut task = WholeScanTask::new();

        // Start first task and let it complete.
        task.start_reindent(text.clone(), vec![(0, 2)], "Rust".to_string(), "    ".to_string());
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Start a second task before polling — the first result's generation is
        // now stale so poll() must skip it and return the second result.
        task.start_reindent(text, vec![(0, 1)], "Rust".to_string(), "    ".to_string());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(r) = task.poll() {
                assert!(matches!(r, WholeScanResult::Reindent(_)));
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("second reindent task did not complete within 2 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(task.poll().is_none());
    }

    #[test]
    fn unknown_language_reindent_deposits_none_result() {
        // "NoSuchLanguage_xyz" is not in syntect; reindent() returns None.
        let text = Rope::from("some text\n");
        let mut task = WholeScanTask::new();
        task.start_reindent(
            text,
            vec![(0, 1)],
            "NoSuchLanguage_xyz".to_string(),
            "    ".to_string(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(WholeScanResult::Reindent(delta)) = task.poll() {
                // Unknown language → no delta.
                assert!(delta.is_none());
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("task did not complete within 2 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
