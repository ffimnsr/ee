//! Backend discovery and live capability probes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Sandbox implementation selected for the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Linux bubblewrap.
    Bubblewrap,
    /// macOS Seatbelt through `sandbox-exec`.
    Seatbelt,
}

impl SandboxBackend {
    /// Stable label for status and doctor output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "seatbelt",
        }
    }
}

/// Result of probing the host sandbox backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProbe {
    /// Selected backend.
    pub backend: SandboxBackend,
    /// Resolved backend executable.
    pub program: Option<PathBuf>,
    /// Whether a sandbox can actually be created.
    pub available: bool,
    /// Whether the resolved bwrap binary is setuid-root.
    pub setuid: bool,
    /// Whether the smoke command succeeded.
    pub smoke_ok: bool,
    /// Human-readable failure detail.
    pub error: Option<String>,
}

impl SandboxProbe {
    fn unavailable(
        backend: SandboxBackend,
        program: Option<PathBuf>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            program,
            available: false,
            setuid: false,
            smoke_ok: false,
            error: Some(error.into()),
        }
    }

    /// One-line status summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let program = self
            .program
            .as_ref()
            .map_or("<not found>", |path| path.to_str().unwrap_or("<non-utf8 path>"));
        if self.available {
            format!("{} available ({program})", self.backend.label())
        } else {
            let reason = self.error.as_deref().unwrap_or("probe failed");
            format!("{} unavailable ({program}): {reason}", self.backend.label())
        }
    }
}

/// Cached host probe. Connection setup and doctor calls share one result.
#[must_use]
pub fn cached_probe() -> &'static SandboxProbe {
    static PROBE: OnceLock<SandboxProbe> = OnceLock::new();
    PROBE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            probe_linux()
        }
        #[cfg(target_os = "macos")]
        {
            probe_macos()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            SandboxProbe::unavailable(
                SandboxBackend::Bubblewrap,
                None,
                "sandboxing is supported only on Linux and macOS",
            )
        }
    })
}

/// Probes `bwrap` on Linux.
#[must_use]
pub fn probe_linux() -> SandboxProbe {
    let Some(program) = find_program("bwrap") else {
        return SandboxProbe::unavailable(
            SandboxBackend::Bubblewrap,
            None,
            "bwrap was not found on PATH; install bubblewrap",
        );
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&program).map(|meta| meta.permissions().mode()).unwrap_or(0);
        if mode & 0o4000 != 0 {
            return SandboxProbe {
                backend: SandboxBackend::Bubblewrap,
                program: Some(program),
                available: false,
                setuid: true,
                smoke_ok: false,
                error: Some(String::from(
                    "refusing a setuid-root bwrap binary; install the non-setuid bubblewrap package",
                )),
            };
        }
    }
    let output = Command::new(&program)
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup",
            "--ro-bind",
            "/",
            "/",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => SandboxProbe {
            backend: SandboxBackend::Bubblewrap,
            program: Some(program),
            available: true,
            setuid: false,
            smoke_ok: true,
            error: None,
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            SandboxProbe::unavailable(
                SandboxBackend::Bubblewrap,
                Some(program),
                format!(
                    "bubblewrap smoke test failed: {}",
                    stderr.trim().lines().next().unwrap_or("unknown error")
                ),
            )
        }
        Err(error) => SandboxProbe::unavailable(
            SandboxBackend::Bubblewrap,
            Some(program),
            format!("bubblewrap could not run: {error}"),
        ),
    }
}

/// Probes `sandbox-exec` on macOS.
#[must_use]
pub fn probe_macos() -> SandboxProbe {
    let path = Path::new("/usr/bin/sandbox-exec");
    if !path.is_file() {
        return SandboxProbe::unavailable(
            SandboxBackend::Seatbelt,
            None,
            "sandbox-exec is missing; macOS sandboxing is unavailable",
        );
    }
    let output = Command::new(path)
        .args(["-p", "(version 1)(allow default)", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => SandboxProbe {
            backend: SandboxBackend::Seatbelt,
            program: Some(path.to_path_buf()),
            available: true,
            setuid: false,
            smoke_ok: true,
            error: None,
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            SandboxProbe::unavailable(
                SandboxBackend::Seatbelt,
                Some(path.to_path_buf()),
                format!(
                    "sandbox-exec smoke test failed: {}",
                    stderr.trim().lines().next().unwrap_or("unknown error")
                ),
            )
        }
        Err(error) => SandboxProbe::unavailable(
            SandboxBackend::Seatbelt,
            Some(path.to_path_buf()),
            format!("sandbox-exec could not run: {error}"),
        ),
    }
}

/// Finds an executable on `PATH`.
#[must_use]
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_setuid_probe_shape_is_stable() {
        let probe = SandboxProbe::unavailable(SandboxBackend::Bubblewrap, None, "missing");
        assert!(!probe.available);
        assert!(probe.summary().contains("unavailable"));
    }

    #[test]
    fn find_program_returns_none_for_absent_name() {
        assert!(find_program("ee-sandbox-definitely-missing-program").is_none());
    }
}
