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

//! Stable document API for normal, constrained, and VLF (very large file) modes.
//!
//! `TextStore` is the primary abstraction for accessing document text. It is
//! intentionally object-safe so that call sites can hold a `Box<dyn TextStore>`
//! and remain unaware of whether the backing store is a `Rope`, a paged file, or
//! an overlay model.
//!
//! # Design note – object-safety over enum dispatch
//!
//! The trait is designed to be object-safe (`dyn TextStore`). This lets call
//! sites that only perform read operations remain decoupled from the concrete
//! storage backend. If performance profiling later shows vtable overhead is
//! significant, a thin enum wrapper can be added without changing public APIs.

pub mod rope_store;

// ---------------------------------------------------------------------------
// Document mode
// ---------------------------------------------------------------------------

/// The document mode determines which features and APIs are available for a
/// buffer. The mode is set when the document is opened and may not change after
/// that.
///
/// `DocumentMode` is defined here, adjacent to `TextStore`, so that TUI and
/// rendering code never need to own or define it themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentMode {
    /// Standard full-featured mode for files below 8 MiB / 30 K lines when
    /// line count is known, or below 8 MiB when line count is unknown.
    Normal,
    /// Editing-capable mode with some background features disabled; used for
    /// files in the 8-<30 MiB / 30-<50 K line transition band.
    ConstrainedNormal,
    /// Very Large File mode: read-only, paged, with a lazy newline index.
    /// Full-text extraction is explicitly forbidden in this mode.
    Vlf,
}

// ---------------------------------------------------------------------------
// Typed position wrappers
// ---------------------------------------------------------------------------

/// A byte offset into the document, 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub u64);

/// A UTF-16 code-unit offset into the document, 0-based.
///
/// Used when communicating with LSP servers or other UTF-16-native consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf16Offset(pub u64);

/// A logical (source) line number, 0-based.
///
/// A logical line corresponds to a `\n`-delimited line in the file, regardless
/// of visual line wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalLine(pub u64);

/// A visual row number after line wrapping, 0-based.
///
/// A single logical line may span multiple visual rows when soft-wrap is
/// enabled. Visual rows are a UI concern and must not be stored in the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VisualRow(pub u64);

/// A half-open byte range `[start, end)` within a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

impl ByteRange {
    /// Construct a `ByteRange` from raw `u64` values.
    pub fn new(start: u64, end: u64) -> Self {
        ByteRange { start: ByteOffset(start), end: ByteOffset(end) }
    }

    /// Byte length of the range.
    pub fn len(&self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }

    /// Returns `true` if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.end.0 <= self.start.0
    }
}

// ---------------------------------------------------------------------------
// Partial-result types
// ---------------------------------------------------------------------------

/// The known line count for the document.
///
/// For VLF documents the newline index is built lazily; the count may be
/// approximate or unknown until background scanning completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownLineCount {
    /// The exact number of logical lines in the document.
    Exact(u64),
    /// A lower-bound estimate; background scanning is still in progress.
    Approximate(u64),
    /// The line count is not yet available; no indexing has been done.
    Unknown,
}

/// Result of a `line_to_byte` lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineLookup {
    /// The exact byte offset at the start of the requested line.
    Exact(ByteOffset),
    /// An estimated byte offset computed by linear interpolation from the
    /// partially-scanned index.  The caller may use this for immediate
    /// goto-line positioning while background scanning catches up.
    ///
    /// The inner value is a best-effort approximation; it will converge to
    /// the exact offset as more pages are indexed.
    Approximate(ByteOffset),
    /// The required index region has not yet been scanned.
    Pending,
    /// The requested line number is outside the known document range.
    OutOfRange,
}

/// Result of a UTF-16 ↔ byte-offset conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Utf16Lookup {
    /// The exact byte offset corresponding to the UTF-16 code-unit offset.
    Exact(ByteOffset),
    /// The required index region has not yet been scanned (VLF lazy index).
    Pending,
    /// The requested offset is outside the document range.
    OutOfRange,
}

// ---------------------------------------------------------------------------
// Full-text extraction policy
// ---------------------------------------------------------------------------

/// Controls whether a `TextStore` permits full-document text extraction.
///
/// VLF documents set this to `Forbidden` so that no code path can accidentally
/// load the entire file into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullTextPolicy {
    /// Full-text extraction is allowed (normal and constrained-normal modes).
    Allowed,
    /// Full-text extraction is forbidden; callers must use chunk/range APIs.
    Forbidden,
}

// ---------------------------------------------------------------------------
// Feature gate matrix
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ConstrainedNormal threshold constants
// ---------------------------------------------------------------------------

/// Maximum file size, in bytes, for which LSP full-document sync is enabled in
/// `ConstrainedNormal` mode.
///
/// Above this threshold the editor will not push whole-document text to the
/// LSP server unless the server explicitly advertises bounded range-sync
/// support.  The threshold mirrors the default hard-open threshold for
/// web/unknown files (50 MiB).
pub const CONSTRAINED_LSP_SYNC_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Maximum document length, in Unicode scalar values (chars), for which
/// heap-heavy whole-document operations (global indent, format-document,
/// whole-file diff, …) remain enabled in `ConstrainedNormal` mode.
///
/// Above this threshold those operations are disabled to prevent UI stalls
/// caused by allocating a single large buffer for the entire document.
pub const CONSTRAINED_WHOLE_DOC_MAX_CHARS: u64 = 256_000_000;

/// Availability of editor features for a given [`DocumentMode`].
///
/// Every field is `true` when the feature is enabled for the mode and `false`
/// when it is disabled.  Call [`DocumentMode::feature_gates`] to obtain the
/// matrix for a mode.
///
/// Features disabled in VLF mode can be surfaced to the user through
/// [`VlfStatus::disabled_features`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VlfFeatureGates {
    /// Insert/delete/replace edits are permitted.
    pub editing: bool,
    /// Writing to disk (`:w`, `:x`, `:wqa`, …) is permitted.
    pub save: bool,
    /// Undo/redo history is maintained.
    pub undo: bool,
    /// Text search (find, search-and-replace, grep) is permitted.
    pub search: bool,
    /// Backend syntax highlighting is enabled.
    pub syntax: bool,
    /// Git diff signs in the gutter are shown.
    pub git_signs: bool,
    /// LSP features (hover, completion, go-to-definition, …) are active.
    pub lsp: bool,
    /// LSP full-document sync is enabled.
    ///
    /// When `false` (ConstrainedNormal above 50 MB), the editor skips
    /// whole-document text pushes to LSP servers that have not advertised
    /// bounded range-sync support.  Hover, completion, and diagnostics that
    /// arrive without requiring a full sync are still shown (`lsp` remains
    /// `true`).
    pub lsp_full_sync: bool,
    /// Diagnostic annotations (errors, warnings) are displayed.
    pub diagnostics: bool,
    /// Visual line-wrap is permitted.
    pub wrap: bool,
    /// Heap-heavy whole-document operations are enabled.
    ///
    /// When `false` (ConstrainedNormal above 256 M chars), commands such as
    /// global indent, format-document, whole-file diff, and similar operations
    /// that allocate a single large buffer for the entire document are
    /// disabled to prevent UI stalls.
    pub whole_doc_ops: bool,
}

impl VlfFeatureGates {
    /// Return an iterator over the names of all disabled features.
    ///
    /// The strings are stable identifiers suitable for display in status bars
    /// or notification messages.
    pub fn disabled_features(&self) -> impl Iterator<Item = &'static str> + '_ {
        [
            (!self.editing).then_some("editing"),
            (!self.save).then_some("save"),
            (!self.undo).then_some("undo"),
            (!self.search).then_some("search"),
            (!self.syntax).then_some("syntax"),
            (!self.git_signs).then_some("git-signs"),
            (!self.lsp).then_some("lsp"),
            (!self.lsp_full_sync).then_some("lsp-full-sync"),
            (!self.diagnostics).then_some("diagnostics"),
            (!self.wrap).then_some("wrap"),
            (!self.whole_doc_ops).then_some("whole-doc-ops"),
        ]
        .into_iter()
        .flatten()
    }
}

impl DocumentMode {
    /// Feature gate matrix for this document mode.
    ///
    /// - `Normal` — all features enabled.
    /// - `ConstrainedNormal` — editing and navigation enabled; LSP full-document
    ///   sync and heap-heavy whole-document operations are disabled to prevent
    ///   UI stalls for files above [`CONSTRAINED_LSP_SYNC_MAX_BYTES`] /
    ///   [`CONSTRAINED_WHOLE_DOC_MAX_CHARS`] respectively.
    /// - `Vlf` — read-only; only search is permitted.
    pub fn feature_gates(self) -> VlfFeatureGates {
        match self {
            DocumentMode::Normal => VlfFeatureGates {
                editing: true,
                save: true,
                undo: true,
                search: true,
                syntax: true,
                git_signs: true,
                lsp: true,
                lsp_full_sync: true,
                diagnostics: true,
                wrap: true,
                whole_doc_ops: true,
            },
            // ConstrainedNormal: editing-capable; background-heavy features
            // that can stall the UI on large files are downgraded.
            DocumentMode::ConstrainedNormal => VlfFeatureGates {
                editing: true,
                save: true,
                undo: true,
                search: true,
                syntax: true,
                git_signs: true,
                lsp: true,
                // Disabled: whole-document LSP sync causes server OOM and
                // network stalls above CONSTRAINED_LSP_SYNC_MAX_BYTES.
                lsp_full_sync: false,
                diagnostics: true,
                wrap: true,
                // Disabled: format-document, global indent, whole-file diff
                // allocate a single large buffer above
                // CONSTRAINED_WHOLE_DOC_MAX_CHARS.
                whole_doc_ops: false,
            },
            // VLF: read-only milestone — search/navigation only.
            DocumentMode::Vlf => VlfFeatureGates {
                editing: false,
                save: false,
                undo: false,
                search: true,
                syntax: true,
                git_signs: false,
                lsp: false,
                lsp_full_sync: false,
                diagnostics: false,
                wrap: false,
                whole_doc_ops: false,
            },
        }
    }

    /// A user-facing status notice emitted when the editor downgrades from
    /// full `Normal` mode to `ConstrainedNormal` mode.
    ///
    /// Returns `Some(message)` only for `ConstrainedNormal`; `None` for
    /// `Normal` (no downgrade) and `Vlf` (handled by VLF status overlay).
    pub fn downgrade_notice(self) -> Option<&'static str> {
        match self {
            DocumentMode::ConstrainedNormal => Some(
                "Editor switched to constrained-normal mode: \
                 LSP full-sync and whole-document operations are disabled \
                 for this file size.",
            ),
            DocumentMode::Normal | DocumentMode::Vlf => None,
        }
    }
}

// ---------------------------------------------------------------------------
// User-facing VLF status
// ---------------------------------------------------------------------------

/// Snapshot of user-visible status for a document.
///
/// For normal-mode buffers most fields are fixed (no disabled features, no
/// indexing progress).  For VLF buffers the fields reflect the current pager
/// and index state and should be refreshed on every status-bar render.
///
/// Obtain via [`TextStore::doc_status`].
#[derive(Debug, Clone)]
pub struct DocStatus {
    /// Total file size in bytes as reported by the storage backend.
    pub file_size_bytes: u64,
    /// Human-readable mode label (e.g. `"normal"`, `"constrained-normal"`, `"vlf"`).
    pub mode_name: &'static str,
    /// Features disabled in this mode, as stable identifiers.
    ///
    /// Empty for `Normal`.  Non-empty for `ConstrainedNormal` (lists
    /// `"lsp-full-sync"` and `"whole-doc-ops"`) and `Vlf`.
    pub disabled_features: Vec<&'static str>,
    /// Background newline-index scan progress in the range `[0.0, 1.0]`.
    ///
    /// `1.0` for fully indexed or non-VLF documents.
    pub indexing_progress: f64,
    /// A one-line notice to display in the status bar when the editor
    /// downgrades from full Normal mode to ConstrainedNormal mode.
    ///
    /// `None` for `Normal` (no downgrade occurred) and `Vlf` (VLF status
    /// overlay handles messaging).  `Some(msg)` for `ConstrainedNormal`;
    /// callers should display this once and then dismiss.
    pub downgrade_notice: Option<&'static str>,
}

// ---------------------------------------------------------------------------
// Edit permission
// ---------------------------------------------------------------------------

/// Whether edit commands are permitted on a document.
///
/// Command dispatch layers (e.g. TUI key handlers, RPC command routers) **must**
/// call [`TextStore::edit_permission`] before applying any mutation.  When
/// `Forbidden`, the `reason` string is a user-facing explanation that should be
/// surfaced as a status message or notification.
///
/// Read operations (copy, search, navigation) remain allowed regardless of this
/// value because they operate over [`TextStore`] chunk APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPermission {
    /// Edits are allowed (Normal and ConstrainedNormal modes).
    Allowed,
    /// Edits are forbidden; `reason` is a user-facing explanation.
    Forbidden { reason: &'static str },
}

/// A decoded chunk of UTF-8 text together with its source byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub byte_range: ByteRange,
}

/// Result of a text-chunk read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextChunkResult {
    /// The chunk is available and decoded.
    Ready(TextChunk),
    /// The page backing this range has not yet been read from disk.
    Pending,
    /// The read was cancelled (e.g. by a newer viewport request).
    Cancelled,
    /// The operation is not supported in the current document mode (e.g.
    /// full-text extraction on a VLF buffer).
    Unsupported,
}

// ---------------------------------------------------------------------------
// TextStore trait
// ---------------------------------------------------------------------------

/// Stable document API for all buffer modes.
///
/// This trait is object-safe. Implementations must not add generic methods or
/// `Self`-returning methods. Callers that only need read access should depend on
/// `dyn TextStore` rather than on a concrete type.
///
/// Edit mutations remain on the `Editor`/`Rope` path for now. This trait covers
/// read-only and query operations only until `RopeTextStore` tests pass.
pub trait TextStore {
    /// The current document mode.
    fn mode(&self) -> DocumentMode;

    /// Total byte length of the document content.
    fn len_bytes(&self) -> u64;

    /// The known line count, which may be exact, approximate, or unknown.
    ///
    /// For `DocumentMode::Normal` and `DocumentMode::ConstrainedNormal` this is
    /// always `KnownLineCount::Exact`.
    fn known_line_count(&self) -> KnownLineCount;

    /// Read decoded UTF-8 text for the given byte range.
    ///
    /// Returns `TextChunkResult::Unsupported` if the range is out of bounds.
    /// Returns `TextChunkResult::Pending` when the page is not yet loaded.
    fn read_byte_range(&self, range: ByteRange) -> TextChunkResult;

    /// Map a logical line number to the byte offset of its first character.
    fn line_to_byte(&self, line: LogicalLine) -> LineLookup;

    /// Map a byte offset to the logical line that contains it.
    ///
    /// Returns `None` when the offset is out of range.
    fn byte_to_line(&self, offset: ByteOffset) -> Option<LogicalLine>;

    /// Iterate decoded chunks over a byte range.
    ///
    /// The iterator is boxed so the trait remains object-safe. Each item is a
    /// `TextChunkResult`; callers should handle `Pending` and `Cancelled`
    /// variants appropriately.
    fn iter_chunks(&self, range: ByteRange) -> Box<dyn Iterator<Item = TextChunkResult> + '_>;

    /// An opaque, monotonically increasing revision identifier.
    ///
    /// Two equal `snapshot_id` values imply identical content. Callers may use
    /// this to invalidate caches without comparing text.
    fn snapshot_id(&self) -> u64;

    // -----------------------------------------------------------------------
    // UTF-16 coordinate conversions
    // -----------------------------------------------------------------------
    // These live on `TextStore` so that TUI and rendering code never need
    // local conversion helpers. All coordinate math stays at the storage layer.

    /// Map a byte offset to the number of UTF-16 code units before it.
    ///
    /// Returns `None` when `offset` is out of range or not on a codepoint
    /// boundary.
    fn byte_to_utf16(&self, offset: ByteOffset) -> Option<Utf16Offset>;

    /// Map a UTF-16 code-unit offset to the corresponding byte offset.
    fn utf16_to_byte(&self, offset: Utf16Offset) -> Utf16Lookup;

    // -----------------------------------------------------------------------
    // Full-text extraction policy
    // -----------------------------------------------------------------------

    /// The full-text extraction policy for this document.
    ///
    /// Callers **must** check this before requesting a full-document read.
    /// VLF documents return `FullTextPolicy::Forbidden`.
    fn full_text_policy(&self) -> FullTextPolicy;

    /// Whether edit commands are permitted on this document.
    ///
    /// Default is [`EditPermission::Allowed`].  VLF documents override to
    /// [`EditPermission::Forbidden`].  Command dispatch layers must check this
    /// **before** applying mutations; read operations (copy, search, navigation)
    /// remain available regardless.
    fn edit_permission(&self) -> EditPermission {
        EditPermission::Allowed
    }

    /// Extract the full document text if the policy permits it.
    ///
    /// Returns `TextChunkResult::Unsupported` when
    /// `full_text_policy() == FullTextPolicy::Forbidden`.
    /// Returns `TextChunkResult::Ready` with the complete content otherwise.
    ///
    /// Prefer `iter_chunks` / `read_byte_range` for any path that may be
    /// called in VLF mode.
    fn read_full_text(&self) -> TextChunkResult {
        match self.full_text_policy() {
            FullTextPolicy::Forbidden => TextChunkResult::Unsupported,
            FullTextPolicy::Allowed => self.read_byte_range(ByteRange::new(0, self.len_bytes())),
        }
    }

    /// A snapshot of user-visible document status suitable for a status bar.
    ///
    /// The default implementation derives status entirely from other
    /// `TextStore` methods; backends with richer state (e.g. `VlfStore`)
    /// should override this to expose indexing progress and accurate file
    /// sizes.
    fn doc_status(&self) -> DocStatus {
        let mode = self.mode();
        let gates = mode.feature_gates();
        DocStatus {
            file_size_bytes: self.len_bytes(),
            mode_name: match mode {
                DocumentMode::Normal => "normal",
                DocumentMode::ConstrainedNormal => "constrained-normal",
                DocumentMode::Vlf => "vlf",
            },
            disabled_features: gates.disabled_features().collect(),
            indexing_progress: 1.0,
            downgrade_notice: mode.downgrade_notice(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_mode_all_features_enabled() {
        let gates = DocumentMode::Normal.feature_gates();
        assert!(gates.editing);
        assert!(gates.save);
        assert!(gates.undo);
        assert!(gates.search);
        assert!(gates.syntax);
        assert!(gates.git_signs);
        assert!(gates.lsp);
        assert!(gates.diagnostics);
        assert!(gates.wrap);
        assert_eq!(gates.disabled_features().count(), 0);
    }

    #[test]
    fn vlf_mode_only_search_enabled() {
        let gates = DocumentMode::Vlf.feature_gates();
        assert!(!gates.editing);
        assert!(!gates.save);
        assert!(!gates.undo);
        assert!(gates.search, "search must remain available in VLF");
        assert!(gates.syntax, "visible-range syntax must remain available in VLF");
        assert!(!gates.git_signs);
        assert!(!gates.lsp);
        assert!(!gates.diagnostics);
        assert!(!gates.wrap);
    }

    #[test]
    fn vlf_disabled_features_contains_expected_names() {
        let gates = DocumentMode::Vlf.feature_gates();
        let disabled: Vec<&str> = gates.disabled_features().collect();
        assert!(disabled.contains(&"editing"), "editing disabled in VLF");
        assert!(disabled.contains(&"save"), "save disabled in VLF");
        assert!(disabled.contains(&"undo"), "undo disabled in VLF");
        assert!(!disabled.contains(&"syntax"), "visible-range syntax must NOT be disabled in VLF");
        assert!(disabled.contains(&"git-signs"), "git-signs disabled in VLF");
        assert!(disabled.contains(&"lsp"), "lsp disabled in VLF");
        assert!(disabled.contains(&"diagnostics"), "diagnostics disabled in VLF");
        assert!(disabled.contains(&"wrap"), "wrap disabled in VLF");
        assert!(!disabled.contains(&"search"), "search must NOT be disabled in VLF");
    }

    #[test]
    fn constrained_normal_editing_enabled() {
        let gates = DocumentMode::ConstrainedNormal.feature_gates();
        assert!(gates.editing, "editing must be enabled in constrained-normal");
        assert!(gates.save, "save must be enabled in constrained-normal");
        assert!(gates.undo, "undo must be enabled in constrained-normal");
        assert!(gates.search, "search must be enabled in constrained-normal");
        assert!(gates.syntax, "syntax must be enabled in constrained-normal");
        assert!(gates.git_signs, "git-signs must be enabled in constrained-normal");
        assert!(gates.lsp, "lsp must be enabled in constrained-normal");
        assert!(gates.diagnostics, "diagnostics must be enabled in constrained-normal");
        assert!(gates.wrap, "wrap must be enabled in constrained-normal");
    }

    #[test]
    fn constrained_normal_background_features_disabled() {
        let gates = DocumentMode::ConstrainedNormal.feature_gates();
        assert!(!gates.lsp_full_sync, "lsp-full-sync must be disabled in constrained-normal");
        assert!(!gates.whole_doc_ops, "whole-doc-ops must be disabled in constrained-normal");
        let disabled: Vec<&str> = gates.disabled_features().collect();
        assert!(disabled.contains(&"lsp-full-sync"));
        assert!(disabled.contains(&"whole-doc-ops"));
    }

    #[test]
    fn constrained_normal_downgrade_notice_is_some() {
        assert!(
            DocumentMode::ConstrainedNormal.downgrade_notice().is_some(),
            "downgrade_notice must return a message for ConstrainedNormal"
        );
    }

    #[test]
    fn normal_and_vlf_downgrade_notice_is_none() {
        assert!(DocumentMode::Normal.downgrade_notice().is_none());
        assert!(DocumentMode::Vlf.downgrade_notice().is_none());
    }

    #[test]
    fn constrained_normal_doc_status_has_downgrade_notice() {
        use crate::text_store::rope_store::RopeTextStore;
        use xi_rope::Rope;
        // Force constrained-normal mode by using the policy-override path.
        let rope = Rope::from("hello world");
        let store = RopeTextStore::new_with_mode(rope, DocumentMode::ConstrainedNormal);
        let status = store.doc_status();
        assert_eq!(status.mode_name, "constrained-normal");
        assert!(status.downgrade_notice.is_some());
        assert!(status.disabled_features.contains(&"lsp-full-sync"));
        assert!(status.disabled_features.contains(&"whole-doc-ops"));
    }
}
