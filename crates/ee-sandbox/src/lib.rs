//! OS sandbox command construction for ee terminal and agent processes.
//!
//! The crate is deliberately small and deterministic: capability probes run
//! real binaries, while argv/profile generation is a pure function so callers
//! can test the exact sandbox shape without spawning anything. Linux uses
//! bubblewrap; macOS uses Seatbelt through `sandbox-exec`.
//!
//! Sandboxing is a containment layer, not a sandbox guarantee against kernel
//! bugs or deliberately over-broad grants.

mod limits;
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod probe;
mod spec;

pub use limits::{LimitBreach, ProcessLimits, ProcessTreeMonitor};
pub use linux::{bwrap_command, bwrap_direct_command};
#[cfg(target_os = "macos")]
pub use macos::{seatbelt_command, seatbelt_direct_command};
pub use probe::{
    SandboxBackend, SandboxProbe, cached_probe, find_program, probe_linux, probe_macos,
};
pub use spec::{
    NetworkAccess, ReadAccess, SandboxCommand, SandboxError, SandboxSpec, git_metadata_path,
};

/// Default `PATH` for sandboxed commands when the caller does not supply one.
pub const DEFAULT_SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// Private temporary mount used by both backends.
pub const SANDBOX_TEMP_DIR: &str = "/tmp";
