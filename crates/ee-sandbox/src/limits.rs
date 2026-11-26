//! Bounded resource limits and process-tree sampling for sandboxed terminals.

use std::fmt;
use std::time::{Duration, Instant};

/// Resource ceiling for one sandboxed command. `None` means unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessLimits {
    pub wall_seconds: Option<u64>,
    pub max_rss_bytes: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
    pub max_open_files: Option<u64>,
    pub max_file_bytes: Option<u64>,
    pub max_processes: Option<u64>,
}

impl ProcessLimits {
    /// Whether any limit is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.wall_seconds.is_some()
            || self.max_rss_bytes.is_some()
            || self.max_cpu_seconds.is_some()
            || self.max_open_files.is_some()
            || self.max_file_bytes.is_some()
            || self.max_processes.is_some()
    }

    /// Returns the first RSS/CPU breach for one sample. Wall time is checked
    /// by [`ProcessTreeMonitor`].
    #[must_use]
    pub fn breach(&self, rss_bytes: u64, cpu_seconds: f64) -> Option<LimitBreach> {
        if self.max_rss_bytes.is_some_and(|limit| rss_bytes > limit) {
            return Some(LimitBreach::Rss);
        }
        if self.max_cpu_seconds.is_some_and(|limit| cpu_seconds > limit as f64) {
            return Some(LimitBreach::Cpu);
        }
        None
    }

    /// Human-readable summary for status/doctor output.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.is_enabled() {
            return String::from("unlimited");
        }
        let mut parts = Vec::new();
        if let Some(value) = self.wall_seconds {
            parts.push(format!("wall={value}s"));
        }
        if let Some(value) = self.max_rss_bytes {
            parts.push(format!("rss={value}B"));
        }
        if let Some(value) = self.max_cpu_seconds {
            parts.push(format!("cpu={value}s"));
        }
        if let Some(value) = self.max_open_files {
            parts.push(format!("nofile={value}"));
        }
        if let Some(value) = self.max_file_bytes {
            parts.push(format!("fsize={value}B"));
        }
        if let Some(value) = self.max_processes {
            parts.push(format!("nproc={value}"));
        }
        parts.join(" ")
    }
}

/// Why a monitored process tree was terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitBreach {
    WallTime,
    Rss,
    Cpu,
    /// The process disappeared before a sample was taken.
    Exited,
}

impl fmt::Display for LimitBreach {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WallTime => write!(formatter, "wall time limit"),
            Self::Rss => write!(formatter, "RSS limit"),
            Self::Cpu => write!(formatter, "CPU limit"),
            Self::Exited => write!(formatter, "process exited"),
        }
    }
}

/// Samples one sandboxed process tree against fixed limits.
#[derive(Debug, Clone)]
pub struct ProcessTreeMonitor {
    limits: ProcessLimits,
    started: Instant,
}

impl ProcessTreeMonitor {
    /// Creates a monitor starting at `started`.
    #[must_use]
    pub fn new(limits: ProcessLimits, started: Instant) -> Self {
        Self { limits, started }
    }

    /// Creates a monitor starting now.
    #[must_use]
    pub fn started_now(limits: ProcessLimits) -> Self {
        Self::new(limits, Instant::now())
    }

    /// Samples the process tree rooted at `root_pid`.
    #[must_use]
    pub fn observe(&self, root_pid: u32) -> Option<LimitBreach> {
        if let Some(limit) = self.limits.wall_seconds
            && self.started.elapsed() >= Duration::from_secs(limit)
        {
            return Some(LimitBreach::WallTime);
        }
        let sample = sample_tree(root_pid)?;
        if let Some(breach) = self.limits.breach(sample.rss_bytes, sample.cpu_seconds) {
            return Some(breach);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TreeSample {
    rss_bytes: u64,
    cpu_seconds: f64,
}

#[cfg(target_os = "linux")]
fn sample_tree(root_pid: u32) -> Option<TreeSample> {
    let pids = process_tree(root_pid);
    if pids.is_empty() {
        return None;
    }
    let mut rss_bytes = 0u64;
    let mut ticks = 0u64;
    for pid in pids {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            rss_bytes = rss_bytes.saturating_add(parse_vm_rss(&status).unwrap_or(0));
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            ticks = ticks.saturating_add(parse_cpu_ticks(&stat).unwrap_or(0));
        }
    }
    Some(TreeSample { rss_bytes, cpu_seconds: ticks as f64 / 100.0 })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_tree(_root_pid: u32) -> Option<TreeSample> {
    None
}

#[cfg(target_os = "macos")]
fn sample_tree(root_pid: u32) -> Option<TreeSample> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,rss=,time="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let (Ok(pid), Ok(ppid), Ok(rss), Some(cpu)) = (
            fields[0].parse::<u32>(),
            fields[1].parse::<u32>(),
            fields[2].parse::<u64>(),
            parse_ps_cpu_seconds(fields[3]),
        ) else {
            continue;
        };
        rows.push((pid, ppid, rss.saturating_mul(1024), cpu));
    }
    if rows.is_empty() {
        return None;
    }
    let mut selected = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        selected.push(pid);
        for (child, parent, _, _) in &rows {
            if *parent == pid && !selected.contains(child) {
                stack.push(*child);
            }
        }
    }
    let mut rss_bytes = 0u64;
    let mut cpu_seconds = 0f64;
    for pid in selected {
        if let Some((_, _, rss, cpu)) = rows.iter().find(|(row_pid, _, _, _)| *row_pid == pid) {
            rss_bytes = rss_bytes.saturating_add(*rss);
            cpu_seconds += *cpu;
        }
    }
    Some(TreeSample { rss_bytes, cpu_seconds })
}

#[cfg(target_os = "macos")]
fn parse_ps_cpu_seconds(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split(':').collect();
    match parts.as_slice() {
        [minutes, seconds] => {
            Some(minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
        }
        [hours, minutes, seconds] => Some(
            hours.parse::<f64>().ok()? * 3600.0
                + minutes.parse::<f64>().ok()? * 60.0
                + seconds.parse::<f64>().ok()?,
        ),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn process_tree(root_pid: u32) -> Vec<u32> {
    let mut parents = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some((ppid, _)) = parse_stat_fields(&stat) {
            parents.insert(pid, ppid);
        }
    }
    let mut selected = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        selected.push(pid);
        for (child, parent) in &parents {
            if *parent == pid && !selected.contains(child) {
                stack.push(*child);
            }
        }
    }
    selected
}

#[cfg(target_os = "linux")]
fn parse_stat_fields(stat: &str) -> Option<(u32, u64)> {
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((ppid, utime.saturating_add(stime)))
}

#[cfg(target_os = "linux")]
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let value = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    value.checked_mul(1024)
}

#[cfg(target_os = "linux")]
fn parse_cpu_ticks(stat: &str) -> Option<u64> {
    parse_stat_fields(stat).map(|(_, ticks)| ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_limits_are_unlimited() {
        let limits = ProcessLimits::default();
        assert!(!limits.is_enabled());
        assert_eq!(limits.summary(), "unlimited");
    }

    #[test]
    fn wall_limit_breaches_after_deadline() {
        let limits = ProcessLimits { wall_seconds: Some(0), ..ProcessLimits::default() };
        let monitor = ProcessTreeMonitor::started_now(limits);
        assert_eq!(monitor.observe(std::process::id()), Some(LimitBreach::WallTime));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_process_samples_without_breach() {
        let monitor = ProcessTreeMonitor::started_now(ProcessLimits {
            max_rss_bytes: Some(u64::MAX),
            max_cpu_seconds: Some(u64::MAX),
            ..ProcessLimits::default()
        });
        assert_eq!(monitor.observe(std::process::id()), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_fields_parse_ppid_and_cpu() {
        let stat = "42 (name with ) paren) S 7 0 0 0 0 0 0 0 0 0 250 50 0 0";
        assert_eq!(parse_stat_fields(stat), Some((7, 300)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ps_cpu_time_parses() {
        assert_eq!(parse_ps_cpu_seconds("01:02"), Some(62.0));
        assert_eq!(parse_ps_cpu_seconds("01:02:03"), Some(3723.0));
    }
}
