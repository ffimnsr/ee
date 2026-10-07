//! Test-only wall-clock stage timing for the file-open pipeline.
//!
//! The manual `open_path_perf_probe` test (`tabs/tests/open_perf.rs`) arms the
//! recorder around one real open request and reads per-stage wall times back
//! afterwards. Stages follow the real open path:
//!
//! - `FileManager::open_with_override`: `Stat` (existence check, metadata,
//!   line-count hint sample, open-policy decision) and `VlfOpen` (VLF store
//!   open plus background index kickoff).
//! - `try_load_file` (rope path): `ReadDecode` (file open, full read, advisory
//!   lock), `Analysis` (encoding guess plus bounded formatting probe),
//!   `RopeBuild` (`Rope::from`).
//! - `CoreState::do_new_view`: `EditorCreate` (CRDT engine plus buffer) and
//!   `ViewInit` (view creation, buffer config, language detection, plugin
//!   binding, `view_init`).
//! - `CoreState::finalize_new_views`: `Finalize` (whitespace detection plus
//!   `finish_init`); the initial render pass runs inside this stage and is
//!   additionally attributed to `Render`, so the two overlap by design.
//!
//! Compiled out without `cfg(test)`; when disarmed each hook is one
//! thread-local branch.

use std::cell::RefCell;
use std::time::Duration;

/// One named stage of the open pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenStage {
    /// Existence check, metadata stat, line-count hint sample, policy decision.
    Stat,
    /// File open, full read, and advisory lock acquisition.
    ReadDecode,
    /// Encoding guess and bounded formatting analysis.
    Analysis,
    /// Rope construction from the decoded text.
    RopeBuild,
    /// VLF store open and background index kickoff.
    VlfOpen,
    /// Editor construction (CRDT engine plus head buffer).
    EditorCreate,
    /// View creation, buffer config, language detection, and `view_init`.
    ViewInit,
    /// Deferred `finalize_new_views` work, including the initial render pass.
    Finalize,
    /// One render pass producing or updating the frontend payload.
    Render,
}

const STAGE_COUNT: usize = 9;

impl OpenStage {
    /// Every stage, in probe-report order.
    pub(crate) const ALL: [OpenStage; STAGE_COUNT] = [
        OpenStage::Stat,
        OpenStage::ReadDecode,
        OpenStage::Analysis,
        OpenStage::RopeBuild,
        OpenStage::VlfOpen,
        OpenStage::EditorCreate,
        OpenStage::ViewInit,
        OpenStage::Finalize,
        OpenStage::Render,
    ];

    /// Stable lowercase name for probe output and JSON artifacts.
    pub(crate) fn name(self) -> &'static str {
        match self {
            OpenStage::Stat => "stat",
            OpenStage::ReadDecode => "read_decode",
            OpenStage::Analysis => "analysis",
            OpenStage::RopeBuild => "rope_build",
            OpenStage::VlfOpen => "vlf_open",
            OpenStage::EditorCreate => "editor_create",
            OpenStage::ViewInit => "view_init",
            OpenStage::Finalize => "finalize",
            OpenStage::Render => "render",
        }
    }

    fn index(self) -> usize {
        OpenStage::ALL.iter().position(|stage| *stage == self).expect("stage listed in ALL")
    }
}

/// Accumulated wall time per stage for one armed measurement.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OpenStageTimings {
    durations: [Duration; STAGE_COUNT],
}

impl OpenStageTimings {
    /// Recorded time for `stage`, in microseconds.
    pub(crate) fn micros(&self, stage: OpenStage) -> u128 {
        self.durations[stage.index()].as_micros()
    }

    /// Sum of every stage in the pipeline, in microseconds.
    ///
    /// `Finalize` and `Render` overlap (see module docs), so this total
    /// double-counts the initial render pass by design.
    pub(crate) fn total_micros(&self) -> u128 {
        self.durations.iter().map(Duration::as_micros).sum()
    }

    /// `name=micros` pairs for every stage, for probe output.
    pub(crate) fn summary(&self) -> String {
        OpenStage::ALL
            .iter()
            .map(|stage| format!("{}={}", stage.name(), self.micros(*stage)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn add(&mut self, stage: OpenStage, elapsed: Duration) {
        self.durations[stage.index()] += elapsed;
    }
}

thread_local! {
    static RECORDER: RefCell<Option<OpenStageTimings>> = const { RefCell::new(None) };
}

/// Starts recording for the current thread, discarding any previous snapshot.
pub(crate) fn arm() {
    RECORDER.with(|slot| *slot.borrow_mut() = Some(OpenStageTimings::default()));
}

/// Takes the recorded snapshot, disarming the recorder for this thread.
pub(crate) fn take() -> Option<OpenStageTimings> {
    RECORDER.with(|slot| slot.borrow_mut().take())
}

/// Records one stage's elapsed time when the recorder is armed.
pub(crate) fn record(stage: OpenStage, elapsed: Duration) {
    RECORDER.with(|slot| {
        if let Some(timings) = slot.borrow_mut().as_mut() {
            timings.add(stage, elapsed);
        }
    });
}
