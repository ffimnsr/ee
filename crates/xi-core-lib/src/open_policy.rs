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

//! VLF open policy and threshold evaluation.
//!
//! This module decides which [`DocumentMode`] to use when opening a file,
//! based on file size, estimated line count, available memory, file location,
//! and any user-supplied mode override.
//!
//! # Thresholds (VS Code-inspired)
//!
//! | Zone               | Condition                                  |
//! |--------------------|--------------------------------------------|
//! | **Normal**         | size < 20 MB **and** line count < 300 K    |
//! | **ConstrainedNormal** | above Normal, still fits in RAM         |
//! | **Vlf**            | at/above `vlf_bytes` or too big for RAM    |
//!
//! Hard confirmation thresholds require explicit user acknowledgement before
//! the file is opened at all:
//!
//! | Location | Default threshold |
//! |----------|-------------------|
//! | Local    | 1 GB              |
//! | Remote   | 10 MB             |
//! | Web / unknown | 50 MB        |
//!
//! Files that exceed the confirmation threshold return
//! [`OpenDecision::ConfirmationRequired`]; callers must surface this to the
//! user and re-invoke with the appropriate [`ModeOverride`] after they accept.
//!
//! # Fail-closed rule
//!
//! When the file size cannot be determined from metadata (e.g., `stat` fails or
//! returns zero for a non-empty path), the policy returns
//! [`OpenDecision::Reject`].  **No caller may fall back to a whole-file read**
//! in this case.

use crate::text_store::DocumentMode;

// ---------------------------------------------------------------------------
// File location
// ---------------------------------------------------------------------------

/// Where the file being opened lives.
///
/// Location affects the hard-confirmation threshold and (in future) the
/// streaming-read strategy used by VLF mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLocation {
    /// A file on a locally-mounted filesystem.
    Local,
    /// A file accessed over a remote protocol (SSH, SFTP, …).
    Remote,
    /// A file from a web URL or a source whose location is unknown.
    Web,
}

// ---------------------------------------------------------------------------
// Mode override
// ---------------------------------------------------------------------------

/// User-supplied override for the document mode.
///
/// When the policy would choose VLF but the user explicitly asks for Normal
/// mode (because they know the file fits), they set `ForceNormal`.  The policy
/// respects this override but still enforces the confirmation threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeOverride {
    /// Let the policy decide based on thresholds.
    #[default]
    Auto,
    /// Open in Normal mode even if the file is above the normal threshold.
    ///
    /// Only safe when the caller has already confirmed that the file fits in
    /// RAM.  The policy still rejects files above the confirmation threshold
    /// regardless of this override.
    ForceNormal,
    /// Open in VLF mode even if the file is below the normal threshold.
    ///
    /// Useful for testing or for files that the user knows are being appended
    /// to rapidly.
    ForceVlf,
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

/// Size and line-count thresholds that drive the open policy.
///
/// All byte values are inclusive upper bounds for the *lower* tier.  For
/// example, `normal_bytes = 20 * 1024 * 1024` means files **strictly
/// smaller** than 20 MiB are opened in Normal mode (when line count also
/// qualifies).
#[derive(Debug, Clone)]
pub struct OpenThresholds {
    /// Maximum file size (bytes) for full Normal mode.
    ///
    /// Default: 20 MiB.
    pub normal_bytes: u64,

    /// Maximum logical line count for full Normal mode.
    ///
    /// This hint is only used when the caller provides a known line count (e.g.
    /// from a previously cached index).  When unknown, only `normal_bytes` is
    /// used.
    ///
    /// Default: 300 000.
    pub normal_lines: u64,

    /// File size threshold at which ConstrainedNormal switches to Vlf.
    ///
    /// Files in `[normal_bytes, vlf_bytes)` use `ConstrainedNormal`; files at
    /// or above `vlf_bytes` use `Vlf`.  This value should be set to a size
    /// that the editor can comfortably keep in RAM.
    ///
    /// Default: 256 MiB.
    pub vlf_bytes: u64,

    /// Hard-open confirmation threshold for **local** files.
    ///
    /// Files at or above this size require explicit user confirmation before
    /// any data is read.
    ///
    /// Default: 1 GiB.
    pub confirm_local_bytes: u64,

    /// Hard-open confirmation threshold for **remote** files.
    ///
    /// Default: 10 MiB.
    pub confirm_remote_bytes: u64,

    /// Hard-open confirmation threshold for **web / unknown** files.
    ///
    /// Default: 50 MiB.
    pub confirm_web_bytes: u64,
}

impl Default for OpenThresholds {
    fn default() -> Self {
        OpenThresholds {
            normal_bytes: 20 * 1024 * 1024,
            normal_lines: 300_000,
            vlf_bytes: 256 * 1024 * 1024,
            confirm_local_bytes: 1024 * 1024 * 1024,
            confirm_remote_bytes: 10 * 1024 * 1024,
            confirm_web_bytes: 50 * 1024 * 1024,
        }
    }
}

impl OpenThresholds {
    /// Returns the hard-confirmation byte threshold for `location`.
    pub fn confirm_bytes_for(&self, location: FileLocation) -> u64 {
        match location {
            FileLocation::Local => self.confirm_local_bytes,
            FileLocation::Remote => self.confirm_remote_bytes,
            FileLocation::Web => self.confirm_web_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// Open decision
// ---------------------------------------------------------------------------

/// The decision reached by [`OpenPolicy::decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDecision {
    /// Open the file immediately in the given mode.
    Open(DocumentMode),

    /// The file exceeds the hard confirmation threshold.
    ///
    /// The caller must surface `reason` to the user and, if they accept, retry
    /// the open with an explicit [`ModeOverride`].  The suggested mode is
    /// provided so the caller can pass it back without re-running the policy.
    ConfirmationRequired {
        /// User-facing explanation of why confirmation is needed.
        reason: &'static str,
        /// The mode that would be selected after user confirmation.
        mode: DocumentMode,
    },

    /// The file cannot be opened safely.
    ///
    /// `reason` is a user-facing message.  Callers **must not** fall back to a
    /// whole-file read.
    Reject {
        /// User-facing explanation.
        reason: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Open policy
// ---------------------------------------------------------------------------

/// Evaluates the open policy for a file.
///
/// Create one per `FileManager` (or application) and call [`decide`] for each
/// file before opening it.
///
/// [`decide`]: OpenPolicy::decide
#[derive(Debug, Clone, Default)]
pub struct OpenPolicy {
    pub thresholds: OpenThresholds,
}

impl OpenPolicy {
    /// Create a policy from the provided thresholds.
    pub fn new(thresholds: OpenThresholds) -> Self {
        OpenPolicy { thresholds }
    }

    /// Decide which [`DocumentMode`] to use for a file.
    ///
    /// # Parameters
    ///
    /// - `file_size_bytes` — byte size from `fs::metadata`.  Must come from a
    ///   trusted `stat(2)` call on the actual file path.  Pass `None` when the
    ///   metadata is unavailable; the policy fails-closed in that case.
    /// - `line_count_hint` — cached logical line count if known; `None` means
    ///   unknown.  The policy uses this only to gate Normal mode; it is not
    ///   used to select VLF.
    /// - `available_memory_bytes` — current process RSS headroom from the OS,
    ///   or `None` if unavailable.  Used to choose between ConstrainedNormal
    ///   and Vlf when the file is above the normal threshold.
    /// - `location` — where the file lives (affects confirmation thresholds).
    /// - `mode_override` — user-supplied override; see [`ModeOverride`].
    pub fn decide(
        &self,
        file_size_bytes: Option<u64>,
        line_count_hint: Option<u64>,
        available_memory_bytes: Option<u64>,
        location: FileLocation,
        mode_override: ModeOverride,
    ) -> OpenDecision {
        // Fail-closed: if we cannot trust the file size, never attempt a
        // whole-file load.
        let size = match file_size_bytes {
            Some(s) => s,
            None => {
                return OpenDecision::Reject {
                    reason: "file size could not be determined; refusing to load to avoid \
                             exhausting memory",
                };
            }
        };

        // Determine the target mode from size + hints, ignoring override for now.
        let natural_mode = self.natural_mode(size, line_count_hint, available_memory_bytes);

        // Apply user override.
        let chosen_mode = match mode_override {
            ModeOverride::Auto => natural_mode,
            ModeOverride::ForceNormal => DocumentMode::Normal,
            ModeOverride::ForceVlf => DocumentMode::Vlf,
        };

        // Check hard confirmation threshold.
        let confirm_threshold = self.thresholds.confirm_bytes_for(location);
        if size >= confirm_threshold {
            let reason = match location {
                FileLocation::Local => {
                    "file exceeds 1 GiB; open anyway? (this may exhaust available memory)"
                }
                FileLocation::Remote => {
                    "remote file exceeds 10 MiB; open anyway? (large remote reads may stall)"
                }
                FileLocation::Web => {
                    "web/unknown file exceeds 50 MiB; open anyway? (may exhaust memory)"
                }
            };
            return OpenDecision::ConfirmationRequired { reason, mode: chosen_mode };
        }

        OpenDecision::Open(chosen_mode)
    }

    /// Select the natural document mode from file size and hints, without
    /// considering user overrides or confirmation thresholds.
    fn natural_mode(
        &self,
        size: u64,
        line_count_hint: Option<u64>,
        available_memory_bytes: Option<u64>,
    ) -> DocumentMode {
        let t = &self.thresholds;

        // Normal: below both the byte threshold AND the line threshold (if
        // line count is known).
        let below_normal_bytes = size < t.normal_bytes;
        let below_normal_lines = line_count_hint.map(|lc| lc < t.normal_lines).unwrap_or(true);

        if below_normal_bytes && below_normal_lines {
            return DocumentMode::Normal;
        }

        // Above normal threshold: decide ConstrainedNormal vs Vlf.
        //
        // Use Vlf if:
        //   a) file is at/above the explicit vlf_bytes threshold, OR
        //   b) available_memory_bytes is known and the file would exceed half
        //      the headroom (leaving room for the rest of the process).
        let file_exceeds_vlf_bytes = size >= t.vlf_bytes;
        let file_exceeds_memory =
            available_memory_bytes.map(|avail| size >= avail / 2).unwrap_or(false);

        if file_exceeds_vlf_bytes || file_exceeds_memory {
            DocumentMode::Vlf
        } else {
            DocumentMode::ConstrainedNormal
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> OpenPolicy {
        OpenPolicy::default()
    }

    // --- Normal mode ---

    #[test]
    fn small_file_opens_normal() {
        let d = policy().decide(Some(1024), None, None, FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::Normal));
    }

    #[test]
    fn file_just_below_normal_threshold_is_normal() {
        let size = OpenThresholds::default().normal_bytes - 1;
        let d = policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::Normal));
    }

    #[test]
    fn file_at_normal_threshold_is_constrained() {
        let size = OpenThresholds::default().normal_bytes;
        let d = policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::ConstrainedNormal));
    }

    #[test]
    fn line_count_above_threshold_forces_constrained_even_for_small_bytes() {
        // 1 MiB but 500 K lines → ConstrainedNormal because lines > 300 K.
        let d = policy().decide(
            Some(1024 * 1024),
            Some(500_000),
            None,
            FileLocation::Local,
            ModeOverride::Auto,
        );
        assert_eq!(d, OpenDecision::Open(DocumentMode::ConstrainedNormal));
    }

    #[test]
    fn unknown_line_count_does_not_override_byte_threshold() {
        // 5 MiB, line count unknown → should still be Normal (below byte threshold).
        let d = policy().decide(
            Some(5 * 1024 * 1024),
            None,
            None,
            FileLocation::Local,
            ModeOverride::Auto,
        );
        assert_eq!(d, OpenDecision::Open(DocumentMode::Normal));
    }

    // --- VLF mode ---

    #[test]
    fn file_at_vlf_threshold_opens_vlf() {
        let size = OpenThresholds::default().vlf_bytes;
        let d = policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::Vlf));
    }

    #[test]
    fn large_file_exceeding_available_memory_opens_vlf() {
        // 100 MiB file, only 100 MiB available → file >= avail/2.
        let size = 100 * 1024 * 1024u64;
        let avail = 100 * 1024 * 1024u64;
        let d =
            policy().decide(Some(size), None, Some(avail), FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::Vlf));
    }

    #[test]
    fn file_using_less_than_half_memory_is_constrained_not_vlf() {
        // 30 MiB file, 512 MiB available → 30 < 256 → ConstrainedNormal.
        let size = 30 * 1024 * 1024u64;
        let avail = 512 * 1024 * 1024u64;
        let d =
            policy().decide(Some(size), None, Some(avail), FileLocation::Local, ModeOverride::Auto);
        assert_eq!(d, OpenDecision::Open(DocumentMode::ConstrainedNormal));
    }

    // --- Confirmation thresholds ---

    #[test]
    fn local_file_above_1gb_requires_confirmation() {
        let size = OpenThresholds::default().confirm_local_bytes;
        let d = policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::Auto);
        assert!(matches!(d, OpenDecision::ConfirmationRequired { mode: DocumentMode::Vlf, .. }));
    }

    #[test]
    fn remote_file_above_10mb_requires_confirmation() {
        let size = OpenThresholds::default().confirm_remote_bytes;
        let d = policy().decide(Some(size), None, None, FileLocation::Remote, ModeOverride::Auto);
        assert!(matches!(d, OpenDecision::ConfirmationRequired { .. }));
    }

    #[test]
    fn web_file_above_50mb_requires_confirmation() {
        let size = OpenThresholds::default().confirm_web_bytes;
        let d = policy().decide(Some(size), None, None, FileLocation::Web, ModeOverride::Auto);
        assert!(matches!(d, OpenDecision::ConfirmationRequired { .. }));
    }

    #[test]
    fn web_file_just_below_50mb_does_not_require_confirmation() {
        let size = OpenThresholds::default().confirm_web_bytes - 1;
        let d = policy().decide(Some(size), None, None, FileLocation::Web, ModeOverride::Auto);
        // Above normal (20 MiB) but below confirmation (50 MiB) → ConstrainedNormal.
        assert_eq!(d, OpenDecision::Open(DocumentMode::ConstrainedNormal));
    }

    // --- Fail-closed ---

    #[test]
    fn missing_size_metadata_rejects_open() {
        let d = policy().decide(None, None, None, FileLocation::Local, ModeOverride::Auto);
        assert!(matches!(d, OpenDecision::Reject { .. }));
    }

    // --- Overrides ---

    #[test]
    fn force_normal_overrides_constrained_mode() {
        let size = OpenThresholds::default().normal_bytes + 1;
        let d =
            policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::ForceNormal);
        // Still below the confirmation threshold → open is allowed, but in Normal.
        assert_eq!(d, OpenDecision::Open(DocumentMode::Normal));
    }

    #[test]
    fn force_vlf_overrides_normal_for_small_file() {
        let d =
            policy().decide(Some(1024), None, None, FileLocation::Local, ModeOverride::ForceVlf);
        assert_eq!(d, OpenDecision::Open(DocumentMode::Vlf));
    }

    #[test]
    fn force_normal_still_requires_confirmation_above_hard_threshold() {
        // ForceNormal does not bypass the confirmation gate.
        let size = OpenThresholds::default().confirm_local_bytes;
        let d =
            policy().decide(Some(size), None, None, FileLocation::Local, ModeOverride::ForceNormal);
        assert!(matches!(d, OpenDecision::ConfirmationRequired { mode: DocumentMode::Normal, .. }));
    }

    #[test]
    fn custom_thresholds_are_respected() {
        let thresholds = OpenThresholds {
            normal_bytes: 1024,
            normal_lines: 10,
            vlf_bytes: 4096,
            confirm_local_bytes: 8192,
            confirm_remote_bytes: 2048,
            confirm_web_bytes: 2048,
        };
        let policy = OpenPolicy::new(thresholds);
        // 500 bytes → Normal.
        assert_eq!(
            policy.decide(Some(500), None, None, FileLocation::Local, ModeOverride::Auto),
            OpenDecision::Open(DocumentMode::Normal)
        );
        // 2048 bytes → ConstrainedNormal (above normal, below vlf).
        assert_eq!(
            policy.decide(Some(2048), None, None, FileLocation::Local, ModeOverride::Auto),
            OpenDecision::Open(DocumentMode::ConstrainedNormal)
        );
        // 4096 bytes → Vlf (at vlf_bytes).
        assert_eq!(
            policy.decide(Some(4096), None, None, FileLocation::Local, ModeOverride::Auto),
            OpenDecision::Open(DocumentMode::Vlf)
        );
        // 8192 bytes local → ConfirmationRequired (at confirm_local_bytes).
        assert!(matches!(
            policy.decide(Some(8192), None, None, FileLocation::Local, ModeOverride::Auto),
            OpenDecision::ConfirmationRequired { .. }
        ));
    }

    #[test]
    fn confirm_bytes_for_returns_correct_threshold_per_location() {
        let t = OpenThresholds::default();
        assert_eq!(t.confirm_bytes_for(FileLocation::Local), t.confirm_local_bytes);
        assert_eq!(t.confirm_bytes_for(FileLocation::Remote), t.confirm_remote_bytes);
        assert_eq!(t.confirm_bytes_for(FileLocation::Web), t.confirm_web_bytes);
    }
}
