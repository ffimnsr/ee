//! Platform isolation for native plugin processes.
//!
//! This is the only module in `ee-plugin-runtime` that opts out of
//! `deny(unsafe_code)`. Both code paths are FFI by definition: `pre_exec`
//! installs a seccomp filter in the forked child before `execve`, and Windows
//! Job Objects are raw Win32 handles. Safe wrappers for either do not exist in
//! the standard library.
#![allow(unsafe_code)]

use ee_plugin_api::error::PluginError;
use ee_plugin_api::manifest::{Limits, Permissions};

/// Denied syscalls for a plugin with the given permissions.
pub use super::sandbox::denied_syscalls;

/// Install the Linux seccomp filter on a command about to spawn.
#[cfg(target_os = "linux")]
pub fn apply_linux_sandbox(
    command: &mut std::process::Command,
    permissions: &Permissions,
) -> Result<(), PluginError> {
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::os::unix::process::CommandExt;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};

    let denied = denied_syscalls(permissions);
    let filter: BpfProgram = SeccompFilter::new(
        denied.into_iter().map(|syscall| (syscall, Vec::new())).collect::<BTreeMap<_, _>>(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into().map_err(|error| PluginError::Runtime {
            message: format!("unsupported seccomp target arch: {error:?}"),
        })?,
    )
    .map_err(|error| PluginError::Runtime {
        message: format!("seccomp filter build failed: {error}"),
    })?
    .try_into()
    .map_err(|error| PluginError::Runtime {
        message: format!("seccomp BPF compile failed: {error}"),
    })?;

    // SAFETY: `pre_exec` runs in the child after `fork` and before `execve`.
    // The closure only performs async-signal-safe syscalls (`prctl`,
    // `seccomp`) on a filter built before the fork; it allocates nothing.
    unsafe {
        command.pre_exec(move || {
            seccompiler::apply_filter(&filter).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("seccomp filter apply failed: {error}"),
                )
            })?;
            Ok(())
        });
    }
    Ok(())
}

/// Sample process RSS and CPU on macOS via `proc_pid_rusage`.
///
/// `ri_resident_size` is resident bytes; user and system times are in
/// nanoseconds. Returns `None` when the kernel cannot describe the process,
/// for example after it has already exited.
#[cfg(target_os = "macos")]
pub fn read_process_sample(pid: u32) -> Option<super::sandbox::ResourceSample> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // Apple's prototype takes `rusage_info_t *`, but the kernel writes the
    // flavor-sized struct at the pointer value (the canonical call is
    // `proc_pid_rusage(pid, flavor, (rusage_info_t *)&info)`), not through a
    // `void **` indirection.
    let buffer = (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>();
    // SAFETY: `buffer` points at the live `info` value and the flavor matches
    // its type, so the kernel writes exactly `size_of::<rusage_info_v4>()`
    // bytes into it and does not retain the pointer.
    let status = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V4, buffer) };
    if status != 0 {
        return None;
    }
    let cpu_ticks = info.ri_user_time.saturating_add(info.ri_system_time);
    let mut timebase = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: `timebase` is a valid out-parameter of the documented type.
    let timebase_status = unsafe { mach2::mach_time::mach_timebase_info(&mut timebase) };
    if timebase_status != 0 || timebase.denom == 0 {
        return None;
    }
    Some(super::sandbox::ResourceSample {
        rss_bytes: info.ri_resident_size,
        cpu_seconds: mach_ticks_to_seconds(cpu_ticks, timebase.numer, timebase.denom),
    })
}

/// Convert mach absolute time ticks to seconds using the host timebase.
#[cfg(any(target_os = "macos", test))]
fn mach_ticks_to_seconds(ticks: u64, numer: u32, denom: u32) -> f64 {
    if denom == 0 {
        return 0.0;
    }
    (u128::from(ticks) * u128::from(numer)) as f64 / u128::from(denom) as f64 / 1_000_000_000.0
}

/// Non-Linux best effort: the manifest limits are enforced by the resource
/// monitor, but no syscall filter is installed.
#[cfg(not(target_os = "linux"))]
pub fn apply_linux_sandbox(
    _command: &mut std::process::Command,
    _permissions: &Permissions,
) -> Result<(), PluginError> {
    Ok(())
}

/// Assign the child to a Job Object with process-time and memory caps.
#[cfg(windows)]
pub fn apply_windows_job_limits(
    child: &std::process::Child,
    limits: &Limits,
) -> Result<(), PluginError> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    use super::sandbox::{DEFAULT_WINDOWS_MAX_RSS_BYTES, seconds_to_windows_ticks};

    let memory_limit =
        usize::try_from(limits.max_memory_bytes).unwrap_or(DEFAULT_WINDOWS_MAX_RSS_BYTES as usize);
    let cpu_limit = seconds_to_windows_ticks(limits.max_cpu_seconds);

    // SAFETY: the Job Object is created and configured here, every raw handle
    // is closed on failure, and the handle is intentionally leaked to keep the
    // job alive for the child's lifetime (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    // kills the child when the host drops every handle).
    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return Err(PluginError::Runtime { message: "CreateJobObjectW failed".to_owned() });
        }

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        info.BasicLimitInformation.PerProcessUserTimeLimit = cpu_limit;
        info.JobMemoryLimit = memory_limit;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return Err(PluginError::Runtime {
                message: "SetInformationJobObject failed".to_owned(),
            });
        }

        let ok = AssignProcessToJobObject(job, child.as_raw_handle() as isize);
        if ok == 0 {
            CloseHandle(job);
            return Err(PluginError::Runtime {
                message: "AssignProcessToJobObject failed".to_owned(),
            });
        }
    }

    Ok(())
}

/// Non-Windows best effort.
#[cfg(not(windows))]
pub fn apply_windows_job_limits(
    _child: &std::process::Child,
    _limits: &Limits,
) -> Result<(), PluginError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mach_ticks_convert_to_seconds() {
        assert!((mach_ticks_to_seconds(1_000_000_000, 1, 1) - 1.0).abs() < 1e-9);
        // Apple Silicon timebase: 125/3. Ten seconds of CPU is ~240M ticks.
        assert!((mach_ticks_to_seconds(240_000_000, 125, 3) - 10.0).abs() < 1e-9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn self_sample_reports_resident_memory() {
        let sample = read_process_sample(std::process::id()).expect("sample current process");
        assert!(sample.rss_bytes > 0, "resident size must be populated");
    }
}
