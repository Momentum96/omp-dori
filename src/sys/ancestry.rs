//! A process's parent chain (DESIGN 8 "Authority above that"): the control socket places a caller by
//! walking from its peer PID up to PID 1. Linux reads `/proc/<pid>/stat`; macOS reads one bounded
//! `ps` listing of every process's parent.

use std::time::Duration;

/// Hops walked at most; a deeper chain counts as incomplete.
const MAX_DEPTH: usize = 128;
/// Bound on the whole walk.
const WALK_TIMEOUT: Duration = Duration::from_secs(5);

/// The PIDs from a process up towards PID 1, the process itself first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Chain {
    pub(crate) pids: Vec<u32>,
    /// The walk reached PID 1 (or a process without a parent); otherwise a parent could not be read
    /// and the chain stops early.
    pub(crate) complete: bool,
}

/// Walks up from `pid`, each step read through `parent`.
fn walk(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Chain {
    let mut chain = Chain {
        pids: vec![pid],
        complete: false,
    };
    let mut at = pid;
    while chain.pids.len() < MAX_DEPTH {
        if at <= 1 {
            chain.complete = true;
            break;
        }
        match parent(at) {
            Some(0) => {
                chain.complete = true;
                break;
            }
            Some(up) if !chain.pids.contains(&up) => {
                chain.pids.push(up);
                at = up;
            }
            _ => break,
        }
    }
    chain
}

#[cfg(any(target_os = "linux", test))]
/// The parent field of one `/proc/<pid>/stat` line: the second field after the command name, which is
/// in parentheses and may itself contain any character.
fn stat_parent(stat: &str) -> Option<u32> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = rest.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(any(not(target_os = "linux"), test))]
/// A `ps -A -o pid=,ppid=` listing as (pid, parent) pairs.
fn ps_pairs(listing: &str) -> Vec<(u32, u32)> {
    listing
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect()
}

#[cfg(target_os = "linux")]
async fn chain_now(pid: u32) -> Chain {
    use std::io::Read;
    let read = move || {
        walk(pid, |at| {
            let mut text = String::new();
            std::fs::File::open(format!("/proc/{at}/stat"))
                .ok()?
                .take(4096)
                .read_to_string(&mut text)
                .ok()?;
            stat_parent(&text)
        })
    };
    tokio::task::spawn_blocking(read).await.unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
async fn chain_now(pid: u32) -> Chain {
    use super::command::{self, Output, Spec};
    let argv = ["ps", "-A", "-o", "pid=,ppid="].map(String::from);
    let spec = Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: WALK_TIMEOUT,
        kill_grace: Duration::from_secs(1),
        output_cap: 1 << 20,
        output: Output::Capture,
    };
    let listing = match command::run(&spec).await {
        Ok(done) if done.success() && !done.truncated => {
            String::from_utf8_lossy(&done.stdout).into_owned()
        }
        _ => String::new(),
    };
    let pairs = ps_pairs(&listing);
    walk(pid, |at| {
        pairs.iter().find(|(p, _)| *p == at).map(|(_, up)| *up)
    })
}

/// The parent chain of `pid`, bounded in depth and time; a walk that does not finish in time is
/// incomplete.
pub(crate) async fn chain(pid: u32) -> Chain {
    tokio::time::timeout(WALK_TIMEOUT, chain_now(pid))
        .await
        .unwrap_or_else(|_| Chain {
            pids: vec![pid],
            complete: false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_to_the_root_and_stops_at_a_gap_or_a_loop() {
        let table = [(40, 30), (30, 20), (20, 1)];
        let up = |at| table.iter().find(|(p, _)| *p == at).map(|(_, u)| *u);
        let full = walk(40, up);
        assert_eq!(full.pids, [40, 30, 20, 1]);
        assert!(full.complete);
        let gap = walk(50, up);
        assert_eq!(gap.pids, [50]);
        assert!(!gap.complete);
        let looped = walk(7, |at| Some(if at == 7 { 8 } else { 7 }));
        assert_eq!(looped.pids, [7, 8]);
        assert!(!looped.complete);
    }

    #[test]
    fn parses_stat_lines_and_ps_listings() {
        assert_eq!(stat_parent("123 (a) b) c) S 77 123 123 0"), Some(77));
        assert_eq!(stat_parent("garbage"), None);
        assert_eq!(ps_pairs("  10     1\n 11 10\nx y\n"), [(10, 1), (11, 10)]);
    }

    #[tokio::test]
    async fn this_process_descends_from_its_parent() {
        let own = std::process::id();
        let chain = chain(own).await;
        assert_eq!(chain.pids.first(), Some(&own));
        assert!(chain.complete, "{chain:?}");
        assert!(chain.pids.contains(&std::os::unix::process::parent_id()));
    }
}
