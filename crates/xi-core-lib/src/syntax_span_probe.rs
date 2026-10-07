//! Test-only wall-clock stage timing for syntax span production.
//!
//! The manual `syntax_span_render_perf_probe` arms the recorder around one real
//! render-path call and reads per-stage wall times back afterwards. Stages come
//! from two layers: the render path (`view/render.rs`) records context-line
//! collection, chunk slicing, the whole `chunk_syntax_spans` call, and assembly;
//! the top-level span walk (`tree_sitter_support.rs`) records parse, highlight
//! scan, fallback walk, injection handling, and compaction. Nested injection
//! parses roll into their parent's `Injection` stage, so every stage is
//! attributed exactly once per measured call.
//!
//! Compiled out without `cfg(test)`; when disarmed each hook is one
//! thread-local branch.

use std::cell::RefCell;
use std::time::Duration;

/// One named stage of span production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanStage {
    /// Collecting the context `VisualLine`s for the window.
    ContextLines,
    /// Building the chunk text and its per-line segments.
    ChunkSlice,
    /// The whole `chunk_syntax_spans` call for the window.
    ChunkSpans,
    /// Skipping context rows and padding the result to `line_count`.
    Assembly,
    /// Top-level tree-sitter parse of the chunk.
    Parse,
    /// Highlight-query scan over the parsed tree.
    HighlightWalk,
    /// Fallback node walk when the highlight query produced no spans.
    FallbackWalk,
    /// Injection query scan plus nested injection parses.
    Injection,
    /// Span compaction after the walks.
    Compact,
}

const STAGE_COUNT: usize = 9;

impl SpanStage {
    /// Every stage, in probe-report order.
    pub(crate) const ALL: [SpanStage; STAGE_COUNT] = [
        SpanStage::ContextLines,
        SpanStage::ChunkSlice,
        SpanStage::ChunkSpans,
        SpanStage::Assembly,
        SpanStage::Parse,
        SpanStage::HighlightWalk,
        SpanStage::FallbackWalk,
        SpanStage::Injection,
        SpanStage::Compact,
    ];

    /// Stable lowercase name for probe output.
    pub(crate) fn name(self) -> &'static str {
        match self {
            SpanStage::ContextLines => "context_lines",
            SpanStage::ChunkSlice => "chunk_slice",
            SpanStage::ChunkSpans => "chunk_spans",
            SpanStage::Assembly => "assembly",
            SpanStage::Parse => "parse",
            SpanStage::HighlightWalk => "highlight_walk",
            SpanStage::FallbackWalk => "fallback_walk",
            SpanStage::Injection => "injection",
            SpanStage::Compact => "compact",
        }
    }

    fn index(self) -> usize {
        SpanStage::ALL.iter().position(|stage| *stage == self).expect("stage listed in ALL")
    }
}

/// Accumulated wall time per stage for one armed measurement.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SpanStageTimings {
    durations: [Duration; STAGE_COUNT],
}

impl SpanStageTimings {
    /// Recorded time for `stage`, in microseconds.
    pub(crate) fn micros(&self, stage: SpanStage) -> u128 {
        self.durations[stage.index()].as_micros()
    }

    /// `name=micros` pairs for every stage, for probe output.
    pub(crate) fn summary(&self) -> String {
        SpanStage::ALL
            .iter()
            .map(|stage| format!("{}={}", stage.name(), self.micros(*stage)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn add(&mut self, stage: SpanStage, elapsed: Duration) {
        self.durations[stage.index()] += elapsed;
    }
}

thread_local! {
    static RECORDER: RefCell<Option<SpanStageTimings>> = const { RefCell::new(None) };
}

/// Starts recording for the current thread, discarding any previous snapshot.
pub(crate) fn arm() {
    RECORDER.with(|slot| *slot.borrow_mut() = Some(SpanStageTimings::default()));
}

/// Takes the recorded snapshot, disarming the recorder for this thread.
pub(crate) fn take() -> Option<SpanStageTimings> {
    RECORDER.with(|slot| slot.borrow_mut().take())
}

/// Records one stage's elapsed time when the recorder is armed.
pub(crate) fn record(stage: SpanStage, elapsed: Duration) {
    RECORDER.with(|slot| {
        if let Some(timings) = slot.borrow_mut().as_mut() {
            timings.add(stage, elapsed);
        }
    });
}
