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

//! VLF (Very Large File) storage primitives.
//!
//! # Core invariant
//!
//! **The file on disk is the single source of truth.**  The VLF engine holds
//! only:
//!
//! - [`page_index::PageDescriptor`] metadata (byte ranges, line counts, seam
//!   flags) — no raw page bytes,
//! - cached decoded page windows in [`pager::FilePager`] — evicted under a
//!   configurable byte budget,
//! - future overlay edits (not yet implemented) — stored separately, never
//!   inflated into a `Rope`.
//!
//! This invariant is enforced structurally: [`store::VlfStore`] does **not**
//! expose a method that returns the full document as a `String` or `Rope`.
//! Calling [`crate::text_store::TextStore::read_full_text`] on a `VlfStore`
//! always returns [`crate::text_store::TextChunkResult::Unsupported`].
//!
//! # Modules
//!
//! - [`pager`] — owns the `File` handle, performs bounded `pread` I/O, and
//!   maintains a configurable LRU byte cache.
//! - [`page_index`] — stores `PageDescriptor` records, indexed by absolute
//!   byte offset, for O(log n) byte-to-page and line-to-page lookups.
//! - [`store`] — `VlfStore` implements [`crate::text_store::TextStore`] using
//!   `FilePager` and `PageIndex` for all read and navigation operations./// - [`overlay`] — `PieceOverlay` represents sparse edits as an ordered
///   sequence of `Original` (file-range) and `Inserted` (buffer-slice) pieces.
pub mod overlay;
pub mod page_index;
pub mod pager;
pub mod search;
pub mod store;
