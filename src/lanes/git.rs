//! Git for lanes (LANE req. 15b, 60-61; WS req. 21): worktree creation and non-forced removal, the
//! unsent-work check and the bounded work fingerprint. Every call runs without a shell, with the
//! repository-redirecting variables absent (the runner passes only its allow-list) and the C locale,
//! here or on the lane's worker ([`Place`]).

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use super::checks::CLEAN_ENV;
use crate::sys::command::{self, Output, Spec};
use crate::worker::Place;

const LOCAL: Duration = Duration::from_secs(60);
const SAMPLE: Duration = Duration::from_secs(10);
const CAP: u64 = 1024 * 1024;
/// Paths in one work fingerprint at most (WS req. 21).
const SAMPLE_PATHS: usize = 256;

pub(super) async fn git(
    place: &Place,
    cwd: &Path,
    args: &[&str],
    limit: Duration,
) -> Result<(bool, String), String> {
    let mut line = vec!["git".to_string()];
    line.extend(args.iter().map(|a| (*a).to_string()));
    let done = place
        .run(&Spec {
            argv: &line,
            cwd: Some(cwd),
            env: &CLEAN_ENV,
            timeout: limit,
            kill_grace: command::KILL_GRACE,
            output_cap: CAP,
            output: Output::Capture,
        })
        .await?;
    if done.timed_out || done.truncated {
        return Err(format!(
            "git {} did not finish within its bounds",
            args.join(" ")
        ));
    }
    Ok((
        done.success(),
        String::from_utf8_lossy(&done.stdout).into_owned(),
    ))
}

/// Creates a worktree on a new branch; refused when the path exists (LANE req. 15b).
pub(crate) async fn add_worktree(
    repo: &Path,
    path: &str,
    branch: &str,
    base: &str,
) -> Result<(), String> {
    if Path::new(path).exists() {
        return Err(format!("the worktree path {path} exists"));
    }
    let (ok, _) = git(
        &Place::Local,
        repo,
        &["worktree", "add", "-b", branch, path, base],
        LOCAL,
    )
    .await?;
    if ok {
        Ok(())
    } else {
        Err(format!("git worktree add {path} failed"))
    }
}

/// Non-forced removal; the failure is named in the receipt (LANE req. 61).
pub(crate) async fn remove_worktree(place: &Place, path: &str) -> Result<(), String> {
    let (ok, _) = git(place, Path::new(path), &["worktree", "remove", path], LOCAL).await?;
    if ok {
        Ok(())
    } else {
        Err(format!("git worktree remove {path} refused; kept"))
    }
}

/// Unsent work in one checkout: tracked changes or commits on no remote. Untracked files do not
/// count; git state that cannot be read counts (LANE req. 60). `None` when it is not a checkout.
pub(crate) async fn unsent(place: &Place, dir: &Path) -> Option<String> {
    match git(place, dir, &["rev-parse", "--is-inside-work-tree"], LOCAL).await {
        Ok((true, out)) if out.trim() == "true" => {}
        Ok(_) => return None,
        Err(e) => return Some(format!("{}: git state unreadable ({e})", dir.display())),
    }
    match git(
        place,
        dir,
        &["status", "--porcelain", "--untracked-files=no"],
        LOCAL,
    )
    .await
    {
        Ok((true, out)) if out.trim().is_empty() => {}
        Ok((true, _)) => return Some(format!("{}: uncommitted tracked changes", dir.display())),
        _ => return Some(format!("{}: git status unreadable", dir.display())),
    }
    match git(
        place,
        dir,
        &["rev-list", "--count", "HEAD", "--not", "--remotes"],
        LOCAL,
    )
    .await
    {
        Ok((true, out)) if out.trim() == "0" => None,
        Ok((true, out)) => Some(format!(
            "{}: {} commits on no remote",
            dir.display(),
            out.trim()
        )),
        // A repository without commits has nothing to send.
        Ok((false, _)) => match git(place, dir, &["rev-parse", "HEAD"], LOCAL).await {
            Ok((false, _)) => None,
            _ => Some(format!("{}: commits unreadable", dir.display())),
        },
        Err(e) => Some(format!("{}: commits unreadable ({e})", dir.display())),
    }
}

/// The work fingerprint (WS req. 21): changed paths with size and modification time. `None` is an
/// incomplete sample (over 256 paths or a git failure); "not a git work tree" is an empty sample.
/// On a worker the paths are read without their stamps: one `stat` per path would not be light.
pub(crate) async fn sample(place: &Place, dir: &Path) -> Option<String> {
    match git(place, dir, &["rev-parse", "--is-inside-work-tree"], SAMPLE).await {
        Ok((true, out)) if out.trim() == "true" => {}
        Ok(_) => return Some(String::new()),
        Err(_) => return None,
    }
    let (ok, out) = git(
        place,
        dir,
        &["status", "--porcelain", "--untracked-files=all"],
        SAMPLE,
    )
    .await
    .ok()?;
    if !ok {
        return None;
    }
    let paths: Vec<&str> = out
        .lines()
        .filter_map(|l| l.get(3..))
        .filter(|p| !p.split('/').any(|part| part == "node_modules"))
        .collect();
    if paths.len() > SAMPLE_PATHS {
        return None;
    }
    let mut fingerprint = String::new();
    for p in paths {
        let stamp = match place {
            Place::Local => std::fs::metadata(dir.join(p))
                .ok()
                .map(|m| (m.len(), m.modified().ok())),
            Place::Worker(_) => None,
        };
        let _ = writeln!(fingerprint, "{p}:{stamp:?}");
    }
    Some(fingerprint)
}
