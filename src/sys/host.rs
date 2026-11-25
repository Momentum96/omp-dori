//! Host readers: memory, load, disk and the hub's own resident memory, for Linux and macOS
//! (HOST req. 101). macOS values come from one bounded `sysctl` or `ps` call each; parsers are pure
//! functions, and the macOS ones are tested everywhere. Container headroom reads cgroups, so it and
//! its parsers exist on Linux only.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

/// Host memory figures in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Memory {
    pub(crate) total: u64,
    pub(crate) available: u64,
    /// Tightest cgroup memory limit visible to this process (Linux).
    pub(crate) cgroup_limit: Option<u64>,
    /// Memory charged to that cgroup (Linux).
    pub(crate) cgroup_used: Option<u64>,
    /// Kernel memory level in percent (macOS `kern.memorystatus_level`).
    pub(crate) level_percent: Option<u8>,
}

/// Load averages and the number of usable cores.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Load {
    pub(crate) one: f64,
    pub(crate) five: f64,
    pub(crate) fifteen: f64,
    pub(crate) cores: usize,
}

/// Space on the file system holding a path, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Disk {
    pub(crate) total: u64,
    pub(crate) available: u64,
}

/// A host value could not be read.
#[derive(Debug)]
pub(crate) enum HostError {
    Read(String),
    Parse(&'static str),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "read failed: {e}"),
            Self::Parse(what) => write!(f, "unexpected {what} format"),
        }
    }
}

/// Bound for the macOS helper calls.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const HELPER_TIMEOUT: Duration = Duration::from_secs(2);

/// Space on the file system that holds `path`.
pub(crate) fn disk(path: &Path) -> Result<Disk, HostError> {
    let s = rustix::fs::statvfs(path).map_err(|e| HostError::Read(e.to_string()))?;
    let unit = if s.f_frsize > 0 {
        s.f_frsize
    } else {
        s.f_bsize
    };
    Ok(Disk {
        total: s.f_blocks.saturating_mul(unit),
        available: s.f_bavail.saturating_mul(unit),
    })
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Parses `/proc/meminfo`: `MemTotal` and `MemAvailable`, in KiB.
#[cfg(target_os = "linux")]
pub(crate) fn parse_meminfo(text: &str) -> Result<(u64, u64), HostError> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .map(|kib| kib.saturating_mul(1024))
    };
    match (field("MemTotal"), field("MemAvailable")) {
        (Some(total), Some(available)) => Ok((total, available)),
        _ => Err(HostError::Parse("meminfo")),
    }
}

/// Parses `/proc/loadavg` or the `{ 1.0 2.0 3.0 }` form of macOS `vm.loadavg`.
pub(crate) fn parse_loadavg(text: &str) -> Result<(f64, f64, f64), HostError> {
    let mut nums = text
        .split(|c: char| c.is_whitespace() || c == '{' || c == '}')
        .filter(|w| !w.is_empty())
        .map(str::parse::<f64>);
    match (nums.next(), nums.next(), nums.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c)))
            if a.is_finite() && b.is_finite() && c.is_finite() =>
        {
            Ok((a, b, c))
        }
        _ => Err(HostError::Parse("load average")),
    }
}

/// Parses the cgroup v2 path from `/proc/self/cgroup` (`0::/path`).
#[cfg(target_os = "linux")]
pub(crate) fn parse_cgroup_path(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("0::"))
}

/// Parses `memory.max`; `max` means no limit.
#[cfg(target_os = "linux")]
pub(crate) fn parse_cgroup_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// v1 limits at or above this mean "no limit".
#[cfg(target_os = "linux")]
const V1_UNLIMITED: u64 = 1 << 60;

/// Headroom in per cent of `limit` (LANE req. 19): limit minus usage plus clean reclaimable page
/// cache. Reclaimable is file cache minus shared, mapped, dirty and writeback pages, never below zero
/// or above usage. `v2` selects the `memory.stat` field names.
#[cfg(target_os = "linux")]
#[allow(clippy::cast_precision_loss)]
pub(crate) fn headroom_percent(limit: u64, usage: u64, stat: &str, v2: bool) -> f64 {
    let field = |name: &str| {
        stat.lines()
            .find_map(|l| {
                l.strip_prefix(name)?
                    .strip_prefix(' ')?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0)
    };
    let (file, shared, mapped) = if v2 {
        (field("file"), field("shmem"), field("file_mapped"))
    } else {
        (field("cache"), field("shmem"), field("mapped_file"))
    };
    let (dirty, writeback) = if v2 {
        (field("file_dirty"), field("file_writeback"))
    } else {
        (field("dirty"), field("writeback"))
    };
    let reclaimable = file
        .saturating_sub(shared)
        .saturating_sub(mapped)
        .saturating_sub(dirty)
        .saturating_sub(writeback)
        .min(usage);
    let free = limit
        .saturating_sub(usage)
        .saturating_add(reclaimable)
        .min(limit);
    if limit == 0 {
        return 0.0;
    }
    free as f64 / limit as f64 * 100.0
}

/// A cgroup limit value; `max` and v1's huge sentinel mean no limit.
#[cfg(target_os = "linux")]
fn limit_value(text: &str) -> Option<u64> {
    parse_cgroup_limit(text).filter(|n| *n < V1_UNLIMITED)
}

/// Container memory headroom in per cent: the smallest over every visible cgroup level, hard and
/// soft (high) limits alike. `Ok(None)` when no limit applies; an error when a path contains `..`
/// or a control file cannot be read. Linux only: cgroups do not exist elsewhere.
#[cfg(target_os = "linux")]
pub(crate) fn headroom() -> Result<Option<f64>, HostError> {
    let text = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| HostError::Read(format!("/proc/self/cgroup: {e}")))?;
    let (root, rel, v2) = match parse_cgroup_path(&text) {
        Some(rel) => ("/sys/fs/cgroup".to_string(), rel.to_string(), true),
        None => match text.lines().find_map(|l| {
            let mut parts = l.splitn(3, ':');
            let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            controllers
                .split(',')
                .any(|c| c == "memory")
                .then(|| path.to_string())
        }) {
            Some(rel) => ("/sys/fs/cgroup/memory".to_string(), rel, false),
            None => return Ok(None),
        },
    };
    if rel.split('/').any(|p| p == "..") {
        return Err(HostError::Parse("cgroup path"));
    }
    let root = std::path::PathBuf::from(root);
    let (limits, usage_file): (&[&str], &str) = if v2 {
        (&["memory.max", "memory.high"], "memory.current")
    } else {
        (
            &["memory.limit_in_bytes", "memory.soft_limit_in_bytes"],
            "memory.usage_in_bytes",
        )
    };
    let read = |p: &std::path::Path| match std::fs::read_to_string(p) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HostError::Read(format!("{}: {e}", p.display()))),
    };
    let mut dir = Some(root.join(rel.trim_start_matches('/')));
    let mut lowest: Option<f64> = None;
    while let Some(d) = dir.filter(|d| d.starts_with(&root)) {
        for name in limits {
            let Some(limit) = read(&d.join(name))?.as_deref().and_then(limit_value) else {
                continue;
            };
            let usage = read(&d.join(usage_file))?
                .and_then(|t| t.trim().parse::<u64>().ok())
                .ok_or(HostError::Parse("cgroup usage"))?;
            let stat =
                read(&d.join("memory.stat"))?.ok_or(HostError::Parse("cgroup memory.stat"))?;
            let share = headroom_percent(limit, usage, &stat, v2);
            lowest = Some(lowest.map_or(share, |l: f64| l.min(share)));
        }
        dir = d.parent().map(std::path::Path::to_path_buf);
    }
    Ok(lowest)
}

/// Parses `ps -o rss=` output in KiB.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_ps_rss(text: &str) -> Result<u64, HostError> {
    text.trim()
        .parse::<u64>()
        .map(|k| k.saturating_mul(1024))
        .map_err(|_| HostError::Parse("ps rss"))
}

/// Parses `sysctl -n hw.memsize vm.pagesize vm.page_free_count kern.memorystatus_level`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_sysctl_memory(text: &str) -> Result<Memory, HostError> {
    let mut lines = text.lines().map(|l| l.trim().parse::<u64>().ok());
    let (Some(Some(total)), Some(Some(page)), Some(Some(free))) =
        (lines.next(), lines.next(), lines.next())
    else {
        return Err(HostError::Parse("sysctl memory"));
    };
    let level = lines
        .next()
        .flatten()
        .and_then(|l| u8::try_from(l).ok())
        .filter(|l| *l <= 100);
    let available = match level {
        Some(pct) => total / 100 * u64::from(pct),
        None => free.saturating_mul(page),
    };
    Ok(Memory {
        total,
        available,
        cgroup_limit: None,
        cgroup_used: None,
        level_percent: level,
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{
        HostError, Load, Memory, cores, parse_cgroup_limit, parse_cgroup_path, parse_loadavg,
        parse_meminfo,
    };
    use std::path::PathBuf;

    fn read(path: &str) -> Result<String, HostError> {
        std::fs::read_to_string(path).map_err(|e| HostError::Read(format!("{path}: {e}")))
    }

    pub(crate) async fn memory() -> Result<Memory, HostError> {
        let (total, available) = parse_meminfo(&read("/proc/meminfo")?)?;
        let mut memory = Memory {
            total,
            available,
            ..Memory::default()
        };
        if let Ok(text) = read("/proc/self/cgroup")
            && let Some(rel) = parse_cgroup_path(&text)
        {
            let own = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
            memory.cgroup_used = std::fs::read_to_string(own.join("memory.current"))
                .ok()
                .and_then(|t| t.trim().parse().ok());
            let mut dir = Some(own.as_path());
            while let Some(d) = dir.filter(|d| d.starts_with("/sys/fs/cgroup")) {
                if let Some(limit) = std::fs::read_to_string(d.join("memory.max"))
                    .ok()
                    .as_deref()
                    .and_then(parse_cgroup_limit)
                {
                    memory.cgroup_limit =
                        Some(memory.cgroup_limit.map_or(limit, |l: u64| l.min(limit)));
                }
                dir = d.parent();
            }
        }
        Ok(memory)
    }

    pub(crate) async fn load() -> Result<Load, HostError> {
        let (one, five, fifteen) = parse_loadavg(&read("/proc/loadavg")?)?;
        Ok(Load {
            one,
            five,
            fifteen,
            cores: cores(),
        })
    }

    pub(crate) async fn self_rss() -> Result<u64, HostError> {
        read("/proc/self/status")?
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map(|kib| kib.saturating_mul(1024))
            .ok_or(HostError::Parse("VmRSS"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{
        HELPER_TIMEOUT, HostError, Load, Memory, cores, parse_loadavg, parse_ps_rss,
        parse_sysctl_memory,
    };
    use crate::sys::command::{KILL_GRACE, Output, Spec, run};

    async fn helper(argv: &[&str]) -> Result<String, HostError> {
        let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
        let spec = Spec {
            argv: &argv,
            cwd: None,
            env: &[],
            timeout: HELPER_TIMEOUT,
            kill_grace: KILL_GRACE,
            output_cap: 4096,
            output: Output::Capture,
        };
        let done = run(&spec)
            .await
            .map_err(|e| HostError::Read(e.to_string()))?;
        if !done.success() {
            return Err(HostError::Read(format!("{} failed", argv.join(" "))));
        }
        String::from_utf8(done.stdout).map_err(|_| HostError::Parse("helper output"))
    }

    pub(crate) async fn memory() -> Result<Memory, HostError> {
        let text = helper(&[
            "/usr/sbin/sysctl",
            "-n",
            "hw.memsize",
            "vm.pagesize",
            "vm.page_free_count",
            "kern.memorystatus_level",
        ])
        .await?;
        parse_sysctl_memory(&text)
    }

    pub(crate) async fn load() -> Result<Load, HostError> {
        let (one, five, fifteen) =
            parse_loadavg(&helper(&["/usr/sbin/sysctl", "-n", "vm.loadavg"]).await?)?;
        Ok(Load {
            one,
            five,
            fifteen,
            cores: cores(),
        })
    }

    pub(crate) async fn self_rss() -> Result<u64, HostError> {
        let pid = std::process::id().to_string();
        parse_ps_rss(&helper(&["/bin/ps", "-o", "rss=", "-p", &pid]).await?)
    }
}

pub(crate) use platform::{load, memory, self_rss};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_formats() {
        let meminfo = "MemTotal:       16000000 kB\nMemFree:  1 kB\nMemAvailable:    8000000 kB\n";
        assert_eq!(
            parse_meminfo(meminfo).unwrap(),
            (16_000_000 * 1024, 8_000_000 * 1024)
        );
        assert!(parse_meminfo("MemTotal: 1 kB\n").is_err());
        assert_eq!(
            parse_cgroup_path("0::/user.slice/x.scope\n"),
            Some("/user.slice/x.scope")
        );
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("1048576\n"), Some(1_048_576));
    }

    #[test]
    fn parses_load_averages() {
        assert_eq!(
            parse_loadavg("0.50 1.25 2.00 1/300 4242\n").unwrap(),
            (0.5, 1.25, 2.0)
        );
    }

    #[test]
    fn parses_macos_formats() {
        assert_eq!(
            parse_loadavg("{ 1.10 1.20 1.30 }\n").unwrap(),
            (1.1, 1.2, 1.3)
        );
        let m = parse_sysctl_memory("17179869184\n16384\n100000\n80\n").unwrap();
        assert_eq!(m.level_percent, Some(80));
        assert_eq!(m.available, 17_179_869_184 / 100 * 80);
        let fallback = parse_sysctl_memory("17179869184\n16384\n100000\n").unwrap();
        assert_eq!(fallback.available, 100_000 * 16384);
        assert!(parse_sysctl_memory("x\n").is_err());
        assert_eq!(parse_ps_rss("  2048\n").unwrap(), 2048 * 1024);
    }

    #[tokio::test]
    async fn live_readers_answer() {
        let m = memory().await.unwrap();
        assert!(m.total > 0 && m.available <= m.total);
        assert!(load().await.unwrap().cores >= 1);
        assert!(self_rss().await.unwrap() > 0);
        assert!(disk(Path::new("/")).unwrap().total > 0);
    }
}
