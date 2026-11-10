//! Native sandbox policy and resource monitoring.
//!
//! Syscall filtering is built here as a pure policy so tests can assert the
//! allowlist without spawning a sandboxed process. The unsafe `pre_exec` and
//! Windows Job Object application lives in [`super::platform`].
//!
//! Resource sampling is opportunistic: the client samples `/proc` on each pump
//! or timeout tick instead of running a monitor thread. That keeps termination
//! ordering deterministic and avoids a thread that can outlive the child.

use std::time::{Duration, Instant};

use ee_plugin_api::manifest::{Limits, Permissions};

use crate::health::TerminationReason;

/// How often the client refreshes process resource samples.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Activity jiffies per second assumed by `/proc/<pid>/stat`.
///
/// `USER_HZ` is 100 on every mainstream Linux configuration. Deriving it would
/// require `sysconf`, which this crate keeps out of safe code; the monitor is
/// advisory and health reports carry the raw breach, not the exact tick rate.
const USER_HZ: f64 = 100.0;

/// Sandbox enforcement switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Apply platform isolation when spawning.
    pub enforce: bool,
    /// Resource sampling interval.
    pub sample_interval: Duration,
}

impl SandboxConfig {
    /// Isolation off. Used by low-level test constructors only.
    pub fn disabled() -> Self {
        Self { enforce: false, sample_interval: SAMPLE_INTERVAL }
    }

    /// Isolation on with the default sampling interval.
    pub fn enforced() -> Self {
        Self { enforce: true, sample_interval: SAMPLE_INTERVAL }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// One resource observation for a child process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
}

/// Stateful resource monitor.
///
/// Pure `observe` makes RSS/CPU breach tests deterministic; `poll_process`
/// supplies real samples on Linux.
#[derive(Debug, Clone)]
pub struct ResourceMonitor {
    limits: Limits,
    sample_interval: Duration,
    last_sample: Option<Instant>,
}

impl ResourceMonitor {
    pub fn new(limits: &Limits, config: SandboxConfig) -> Self {
        Self { limits: *limits, sample_interval: config.sample_interval, last_sample: None }
    }

    /// Returns a termination reason when a sample breaches a manifest limit.
    pub fn observe(&self, sample: ResourceSample) -> Option<TerminationReason> {
        if sample.rss_bytes > self.limits.max_memory_bytes {
            return Some(TerminationReason::RssLimit);
        }
        if sample.cpu_seconds > self.limits.max_cpu_seconds as f64 {
            return Some(TerminationReason::CpuLimit);
        }
        None
    }

    /// Whether enough time elapsed since the last refresh.
    pub fn sample_due(&mut self, now: Instant) -> bool {
        match self.last_sample {
            Some(last) if now.saturating_duration_since(last) < self.sample_interval => false,
            _ => {
                self.last_sample = Some(now);
                true
            }
        }
    }

    /// Sample the process and return a breach, if any.
    #[cfg(target_os = "linux")]
    pub fn poll_process(&mut self, pid: u32, now: Instant) -> Option<TerminationReason> {
        if !self.sample_due(now) {
            return None;
        }
        read_process_sample(pid).and_then(|sample| self.observe(sample))
    }

    /// Best-effort platforms without `/proc` report no breach.
    #[cfg(not(target_os = "linux"))]
    pub fn poll_process(&mut self, _pid: u32, now: Instant) -> Option<TerminationReason> {
        let _ = self.sample_due(now);
        None
    }
}

/// Read `/proc/<pid>/status` and `/proc/<pid>/stat` for one sample.
#[cfg(target_os = "linux")]
pub fn read_process_sample(pid: u32) -> Option<ResourceSample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_bytes = parse_vm_rss(&status)?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let cpu_seconds = parse_cpu_seconds(&stat)?;
    Some(ResourceSample { rss_bytes, cpu_seconds })
}

#[cfg(target_os = "linux")]
fn parse_vm_rss(status: &str) -> Option<u64> {
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let value = parts.next()?.parse::<u64>().ok()?;
        let unit = parts.next().unwrap_or("kB");
        let multiplier = match unit {
            "kB" => 1024,
            "mB" | "MB" => 1024 * 1024,
            "B" => 1,
            _ => return None,
        };
        return value.checked_mul(multiplier);
    }
    None
}

#[cfg(target_os = "linux")]
fn parse_cpu_seconds(stat: &str) -> Option<f64> {
    // Field 2 is `(comm)`, which may contain spaces; fields after the closing
    // parenthesis are stable. utime is field 14 and stime field 15 overall.
    let tail = stat.rsplit_once(')')?.1;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((utime + stime) as f64 / USER_HZ)
}

/// Syscalls denied to native plugins.
///
/// Filesystem access is intentionally *not* denied at the syscall layer: the
/// dynamic loader needs `openat` before plugin code runs, and path-scoped
/// filtering is not expressible in seccomp. Filesystem permissions are enforced
/// by the [`super::broker`] service path instead. Native plugins are the trusted
/// install tier; untrusted code must ship as Wasm.
///
/// `clone`/`clone3` stay allowed because the SDK runtime starts a reader thread
/// for stdio multiplexing. Process creation is still blocked by denying `fork`
/// and `vfork`; seccomp cannot distinguish thread flags from process flags on
/// this path, and the trusted-tier boundary is the manifest permission set.
pub fn denied_syscalls(permissions: &Permissions) -> Vec<i64> {
    #[cfg(target_os = "linux")]
    {
        let mut syscalls = vec![libc::SYS_fork, libc::SYS_vfork, libc::SYS_ptrace];

        #[cfg(target_arch = "x86")]
        {
            syscalls.push(libc::SYS_socketcall);
            if !permissions.grants("editor.network") && !permissions.grants("agent.network") {
                syscalls.push(libc::SYS_socket);
            }
        }

        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
        {
            if !permissions.grants("editor.network") && !permissions.grants("agent.network") {
                syscalls.push(libc::SYS_socket);
            }
        }

        syscalls.sort_unstable();
        syscalls.dedup();
        syscalls
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = permissions;
        Vec::new()
    }
}

/// Default Windows job memory limit when the manifest is silent.
pub const DEFAULT_WINDOWS_MAX_RSS_BYTES: u64 = 512 * 1024 * 1024;

/// Convert seconds to Windows 100 ns units without overflowing.
#[cfg(any(windows, test))]
pub fn seconds_to_windows_ticks(seconds: u64) -> i64 {
    seconds.saturating_mul(10_000_000).min(i64::MAX as u64) as i64
}

/// Parse a `/proc` `VmRSS` value; exported for unit tests.
#[cfg(all(target_os = "linux", test))]
pub fn test_parse_vm_rss(status: &str) -> Option<u64> {
    parse_vm_rss(status)
}

/// Parse `/proc/<pid>/stat` CPU fields; exported for unit tests.
#[cfg(all(target_os = "linux", test))]
pub fn test_parse_cpu_seconds(stat: &str) -> Option<f64> {
    parse_cpu_seconds(stat)
}

/// Page size fallback used by `/proc/<pid>/statm` consumers.
pub const STATM_PAGE_BYTES: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn limits() -> Limits {
        Limits {
            max_memory_bytes: 16 * 1024 * 1024,
            max_cpu_seconds: 2,
            rpc_timeout_ms: 1_000,
            max_contributions: 8,
        }
    }

    fn monitor() -> ResourceMonitor {
        ResourceMonitor::new(&limits(), SandboxConfig::enforced())
    }

    #[test]
    fn rss_breach_reports_rss_limit() {
        let monitor = monitor();
        let sample = ResourceSample { rss_bytes: limits().max_memory_bytes + 1, cpu_seconds: 0.0 };
        assert_eq!(monitor.observe(sample), Some(TerminationReason::RssLimit));
    }

    #[test]
    fn cpu_breach_reports_cpu_limit() {
        let monitor = monitor();
        let sample = ResourceSample {
            rss_bytes: limits().max_memory_bytes,
            cpu_seconds: limits().max_cpu_seconds as f64 + 0.1,
        };
        assert_eq!(monitor.observe(sample), Some(TerminationReason::CpuLimit));
    }

    #[test]
    fn in_budget_samples_pass() {
        let monitor = monitor();
        let sample = ResourceSample {
            rss_bytes: limits().max_memory_bytes,
            cpu_seconds: limits().max_cpu_seconds as f64,
        };
        assert_eq!(monitor.observe(sample), None);
    }

    #[test]
    fn sample_interval_throttles() {
        let mut monitor = monitor();
        let now = Instant::now();
        assert!(monitor.sample_due(now));
        assert!(!monitor.sample_due(now + Duration::from_millis(10)));
        assert!(monitor.sample_due(now + SAMPLE_INTERVAL));
    }

    #[test]
    fn network_permission_drops_socket_denial() {
        let denied = denied_syscalls(&Permissions::default());
        let granted = Permissions(BTreeMap::from([("editor.network".to_owned(), true)]));

        #[cfg(target_os = "linux")]
        {
            assert!(denied.contains(&libc::SYS_socket));
            assert!(!denied_syscalls(&granted).contains(&libc::SYS_socket));
            assert!(denied_syscalls(&granted).contains(&libc::SYS_ptrace));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(denied.is_empty());
            assert!(denied_syscalls(&granted).is_empty());
        }
    }

    #[test]
    fn windows_tick_conversion_saturates() {
        assert_eq!(seconds_to_windows_ticks(1), 10_000_000);
        assert_eq!(seconds_to_windows_ticks(u64::MAX), i64::MAX);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_samples_parse() {
        assert_eq!(test_parse_vm_rss("Name:\techo\nVmRSS:\t  2048 kB\n"), Some(2 * 1024 * 1024));
        let stat = "42 (name with ) paren) S 0 0 0 0 0 0 0 0 0 0 250 50 0 0";
        assert_eq!(test_parse_cpu_seconds(stat), Some(3.0));
    }
}
