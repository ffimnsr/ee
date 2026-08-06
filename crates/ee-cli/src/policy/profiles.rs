//! Curated command validation profiles (Phase 4).
//!
//! The profile registry is application-owned and versioned: stable profile
//! ids (`git_readonly`, `rust_validate`) map to fixed structured
//! executable/argv entries, each bound to the workspace cwd, a timeout cap,
//! an output cap, finite use/expiry requirements (via the shared rule
//! scope), and the execute category.  Config stores profile ids only and
//! can never mutate the registry; unknown ids and versions fail closed.

use std::time::Duration;

/// Version of the application-owned profile registry.
pub(crate) const PROFILE_REGISTRY_VERSION: u64 = 1;

/// One fixed structured entry inside a curated profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileEntry {
    pub(crate) executable: &'static str,
    pub(crate) argv: &'static [&'static str],
    /// Bound on one profile command run (registry metadata; the terminal
    /// pipeline caps and cancellation paths are unchanged).
    pub(crate) timeout_cap: Duration,
    /// Bound on retained output for one profile command.
    pub(crate) output_cap: usize,
}

/// One curated profile: stable id plus fixed structured entries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CuratedProfile {
    pub(crate) id: &'static str,
    pub(crate) entries: &'static [ProfileEntry],
}

/// The application-owned curated profile registry.
///
/// Entries are exact structured argv matches and never include VCS
/// mutation, package install, package scripts, publish, network, or shell
/// commands.
pub(crate) const PROFILES: &[CuratedProfile] = &[
    CuratedProfile {
        id: "git_readonly",
        entries: &[
            ProfileEntry {
                executable: "git",
                argv: &["status"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["diff"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["log"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["show"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "--show-current"],
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: "rust_validate",
        entries: &[
            ProfileEntry {
                executable: "cargo",
                argv: &["fmt", "--check"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["test", "--quiet"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["clippy"],
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
        ],
    },
];

/// Exact structured lookup of one command in the curated registry; returns
/// the profile id and the matched entry.
pub(crate) fn match_profile_entry(
    executable: &str,
    argv: &[String],
) -> Option<(&'static str, &'static ProfileEntry)> {
    for profile in PROFILES {
        for entry in profile.entries {
            if entry.executable == executable && entry.argv == argv {
                return Some((profile.id, entry));
            }
        }
    }
    None
}

/// Whether `id` is a known curated profile; unknown ids are rejected.
pub(crate) fn is_known_profile(id: &str) -> bool {
    PROFILES.iter().any(|profile| profile.id == id)
}
