//! Lanes on workers (docs/design/workers.md 2.4, 2.6, 2.7): admission on a worker, the worker's own
//! clone of the repository with a worktree per lane, checks routed to where their facts are, the
//! session lease read on the worker, and the automatic reattach of lanes a dropped connection lost.
//!
//! The repository on a worker is always Cataly's own clone under its root, matched by the hub
//! checkout's Git origin, never a folder of the worker's user. The lane starts at a commit that is on
//! that origin; uncommitted edits of the hub checkout are not carried.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::Lanes;
use super::checks::{CLEAN_ENV, Outcome, Site};
use super::git::git;
use super::record::{Record, State, Validated};
use crate::config::Config;
use crate::host::{Control as _, Runtime};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::sys::command::{KILL_GRACE, Output, Spec};
use crate::worker::Place;
use crate::worker::link::Link;

/// Reading the hub checkout's origin, commit and state.
const LOCAL: Duration = Duration::from_secs(30);
/// A first clone or a fetch on the worker.
const FETCH: Duration = Duration::from_mins(10);
/// A worktree add, a commit check or a lease read on the worker.
const STEP: Duration = Duration::from_secs(60);

/// What the worker got for a new lane: its worktree and the commit it starts at.
pub(super) struct Prepared {
    /// The worktree on the worker, resolved there.
    pub(super) cwd: String,
    pub(super) commit: String,
    /// The hub checkout had uncommitted edits or untracked files, which stay behind.
    pub(super) dirty: bool,
    /// `linux-box (Linux x64)`.
    pub(super) machine: String,
}

impl Prepared {
    /// The line the thread and the open reply carry (design 3, "스레드에 보이는 것").
    pub(super) fn note(&self) -> String {
        let short = self.commit.get(..7).unwrap_or(&self.commit);
        let edits = if self.dirty {
            "the hub checkout's uncommitted edits are not on the worker"
        } else {
            "uncommitted edits on the hub are never carried"
        };
        format!(
            "🖥️ {} · {short} · worktree and session file are on the worker; {edits}",
            self.machine
        )
    }
}

/// Refusals of a placement that need nothing but the config (design 2.6): an unknown worker, a
/// terminal lane.
pub(super) fn placement(config: &Config, v: &Validated) -> Result<(), String> {
    let Some(worker) = &v.worker else {
        return Ok(());
    };
    if !config.workers.contains_key(worker) {
        return Err(format!("no worker named {worker}; see lane.workers"));
    }
    if config.sessions.runtime == crate::config::session::RuntimeKind::Tern {
        return Err(format!(
            "a lane on {worker} runs headless only; the terminal runtime needs this screen"
        ));
    }
    Ok(())
}

impl<R: Runtime> Lanes<R> {
    /// A lane for a worker that cannot be reached now waits for it (the owner's decision): queued
    /// with its thread made now, it starts by itself once the worker answers. `None` when the
    /// lane names no worker or its worker answers.
    pub(super) async fn wait_for_worker(
        &self,
        open: &super::record::OpenInput,
        v: &Validated,
        input: &mut Value,
        thread_from: Option<&str>,
        kickoff: Option<&str>,
    ) -> Result<Option<Value>, String> {
        let Some(worker) = &v.worker else {
            return Ok(None);
        };
        let Err(why) = self.worker_up(worker).await else {
            return Ok(None);
        };
        if let Some(source) = thread_from {
            let thread = self.thread_for(&v.key, &v.title, source).await?;
            if let Some(map) = input.as_object_mut() {
                map.insert("thread".into(), json!(thread));
            }
        }
        let kickoff = kickoff.map(str::to_string);
        self.queue(open, v, input.clone(), kickoff).await?;
        Ok(Some(json!({
            "key": v.key,
            "outcome": format!("queued: {why}; it starts once {worker} is reachable"),
        })))
    }

    /// The live connection to `worker`, or why it is not reachable now. A worker that just failed
    /// is asked again only after a minute.
    pub(super) async fn worker_up(&self, worker: &str) -> Result<Arc<Link>, String> {
        let workers = crate::worker::link::registry().ok_or("workers run only under the hub")?;
        if !self.cfg().workers.contains_key(worker) {
            return Err(format!("no worker named {worker}"));
        }
        if workers.reachable(worker).await
            && let Some(link) = workers.live(worker)
        {
            return Ok(link);
        }
        let reason = workers
            .list()
            .into_iter()
            .find(|v| v.name == worker)
            .and_then(|v| v.reason)
            .unwrap_or_else(|| "not reachable".into());
        Err(format!("{worker} is offline ({reason})"))
    }

    /// Admission of a lane on a worker (design 2.6): the lead's span, the worker reachable and
    /// compatible, and below its own `lanes`. It does not count toward this machine's session cap.
    pub(super) async fn remote_admission(
        &self,
        config: &Config,
        worker: &str,
    ) -> Result<PathBuf, String> {
        let exe = self
            .env
            .omp
            .path()
            .ok_or("the OMP executable was not found")?;
        let settings = config
            .workers
            .get(worker)
            .ok_or_else(|| format!("no worker named {worker}"))?;
        let live = self.live_map().values().filter(|s| s.alive()).count();
        let max = usize::try_from(config.lanes.max).unwrap_or(usize::MAX);
        if max > 0 && live >= max {
            return Err(format!("{live} live lanes reach the maximum of {max}"));
        }
        let link = self.worker_up(worker).await?;
        let used = link.lanes.load(std::sync::atomic::Ordering::Acquire);
        if used >= usize::try_from(settings.lanes).unwrap_or(usize::MAX) {
            return Err(format!(
                "{worker} runs {used} lane(s), its maximum of {}",
                settings.lanes
            ));
        }
        self.overlay(config)?;
        Ok(exe)
    }

    /// The worker's clone of the hub checkout's origin, fetched, and a worktree at the requested
    /// commit on the lane's branch (design 2.6).
    pub(super) async fn prepare(&self, v: &Validated, worker: &str) -> Result<Prepared, String> {
        let here = Place::Local;
        let local = |args: &'static [&'static str]| git(&here, &v.cwd, args, LOCAL);
        let (ok, url) = local(&["remote", "get-url", "origin"]).await?;
        let url = url.trim().to_string();
        if !ok || url.is_empty() {
            return Err(format!(
                "{} has no Git origin; a lane on {worker} starts from a pushed commit",
                v.cwd.display()
            ));
        }
        let origin = crate::config::workers::normalise_origin(&url)
            .ok_or_else(|| format!("the origin {url} cannot be matched on a worker"))?;
        let base = v.worktree.as_ref().map_or("HEAD", |w| w.base.as_str());
        let (ok, commit) = git(
            &here,
            &v.cwd,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
            ],
            LOCAL,
        )
        .await?;
        let commit = commit.trim().to_string();
        if !ok || commit.is_empty() {
            return Err(format!("{base} names no commit in {}", v.cwd.display()));
        }
        let (_, status) = local(&["status", "--porcelain"]).await?;
        let link = self.worker_up(worker).await?;
        let root = PathBuf::from(&link.hello.root);
        let clone = root.join("repos").join(format!("{origin}.git"));
        let lane = root.join("lanes").join(&v.key);
        let there = Place::Worker(worker.to_string());
        let run = |argv: Vec<String>, cwd: PathBuf, limit: Duration| {
            let there = there.clone();
            async move {
                let done = there
                    .run(&Spec {
                        argv: &argv,
                        cwd: Some(&cwd),
                        env: &CLEAN_ENV,
                        timeout: limit,
                        kill_grace: KILL_GRACE,
                        output_cap: 64 * 1024,
                        output: Output::Capture,
                    })
                    .await?;
                if done.success() {
                    Ok(String::from_utf8_lossy(&done.stdout).trim().to_string())
                } else {
                    Err(format!(
                        "{} failed on {worker}: {}",
                        argv.join(" "),
                        String::from_utf8_lossy(&done.stderr).trim()
                    ))
                }
            }
        };
        let text = |p: &Path| p.display().to_string();
        if !stat(&link, &clone).await?.0 {
            let argv = [
                "git",
                "clone",
                "--bare",
                "--quiet",
                "--",
                &url,
                &text(&clone),
            ];
            run(argv.map(str::to_string).to_vec(), root.clone(), FETCH).await?;
            // A bare clone keeps no remote-tracking refs; the unsent check needs them.
            let argv = [
                "git",
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ];
            run(argv.map(str::to_string).to_vec(), clone.clone(), STEP).await?;
        }
        let argv = ["git", "fetch", "--quiet", "--prune", "origin"];
        run(argv.map(str::to_string).to_vec(), clone.clone(), FETCH).await?;
        let argv = ["git", "cat-file", "-e", &format!("{commit}^{{commit}}")];
        if run(argv.map(str::to_string).to_vec(), clone.clone(), STEP)
            .await
            .is_err()
        {
            return Err(format!(
                "commit {} is not on the origin {origin}; push it first, or open the lane here",
                commit.get(..7).unwrap_or(&commit)
            ));
        }
        if stat(&link, &lane).await?.0 {
            return Err(format!("{} exists on {worker}", lane.display()));
        }
        let branch = v
            .worktree
            .as_ref()
            .map_or_else(|| format!("cataly/{}", v.key), |w| w.branch.clone());
        let argv = [
            "git",
            "worktree",
            "add",
            "--quiet",
            "-b",
            &branch,
            &text(&lane),
            &commit,
        ];
        run(argv.map(str::to_string).to_vec(), clone.clone(), STEP).await?;
        let cwd = stat(&link, &lane).await?.1.unwrap_or_else(|| text(&lane));
        let machine = format!("{worker} ({} {})", capital(&link.hello.os), link.hello.arch);
        Ok(Prepared {
            cwd,
            commit,
            dirty: !status.trim().is_empty(),
            machine,
        })
    }

    /// Whether OMP on `worker` shows the session's lease free; `None` when unknown.
    pub(super) async fn lease_free_on(worker: &str, id: &str) -> Option<bool> {
        let argv: Vec<String> = ["omp", "session", "lease", "--json", "--", id]
            .map(str::to_string)
            .to_vec();
        let done = Place::Worker(worker.to_string())
            .run(&Spec {
                argv: &argv,
                cwd: None,
                env: &[],
                timeout: STEP,
                kill_grace: KILL_GRACE,
                output_cap: 1024 * 1024,
                output: Output::Capture,
            })
            .await
            .ok()?;
        if !done.success() || done.timed_out {
            return None;
        }
        match crate::omp::usage::parse_lease(&done.stdout) {
            Ok(l) if l.held.iter().any(|h| h == id) => Some(false),
            Ok(l) if l.unknown.iter().any(|u| u == id) => None,
            Ok(_) => Some(true),
            Err(_) => None,
        }
    }

    /// The folder a lane's session file must stay inside: on a worker, the one its OMP wrote it
    /// in, as the hub records it.
    pub(super) fn session_root(&self, rec: &Record) -> PathBuf {
        match (&rec.worker, &rec.session_file) {
            (Some(_), Some(file)) => Path::new(file)
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf),
            _ => self
                .env
                .state_dir
                .join("sessions")
                .join("lanes")
                .join(&rec.key),
        }
    }

    /// Reads a lane's done checks back live where their facts are (design 2.7).
    pub(super) async fn read_checks(&self, rec: &Record) -> Vec<Outcome> {
        let identities = self.cfg().lanes.identities.clone();
        let place = rec.place();
        let cwd = PathBuf::from(&rec.cwd);
        let site = match &place {
            Place::Local => Site::local(&cwd),
            Place::Worker(_) => Site {
                place: &place,
                cwd: &cwd,
                hub_dir: &self.env.state_dir,
            },
        };
        super::checks::run_all(&rec.checks, &site, &identities).await
    }

    /// Brings back every lane a dropped worker connection lost, once its worker answers again
    /// (design 2.4, Option B): through the same restart as after a hub restart, on the exact
    /// session file. Deliveries in doubt at the drop stay in doubt; nothing is resent.
    pub(super) async fn reattach(self: &Arc<Self>) {
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let Ok(rows) = self.env.store.read(|r| r.lanes(false)).await else {
            return;
        };
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                continue;
            };
            let Some(worker) = rec.worker.clone() else {
                continue;
            };
            if rec.lost.is_none()
                || !rec.open()
                || rec.intent.is_some()
                || rec.state == State::Starting
                || rec.session_id.is_none()
                || self.session(&rec.key).is_some()
                || self.worker_up(&worker).await.is_err()
            {
                continue;
            }
            let line = self.recover_one(&rec.key).await;
            let back = self.session(&rec.key).is_some_and(|s| s.alive());
            self.emit(
                Emit::new(
                    EventKind::LaneRecap,
                    if back {
                        Severity::Info
                    } else {
                        Severity::Warning
                    },
                    true,
                    &rec.key,
                )
                .with("lane", &rec.key)
                .with("detail", format!("{worker} is back: {line}")),
            );
        }
    }
}

/// Whether `path` exists on the worker, and its resolved path.
async fn stat(link: &Link, path: &Path) -> Result<(bool, Option<String>), String> {
    let answer: Value = link
        .call(
            crate::worker::method::STAT,
            json!({ "path": path.display().to_string() }),
            Duration::from_secs(10),
        )
        .await?;
    Ok((
        answer.get("exists").and_then(Value::as_bool) == Some(true),
        answer
            .get("real")
            .and_then(Value::as_str)
            .map(str::to_string),
    ))
}

/// `linux` → `Linux`, `macos` → `macOS`.
fn capital(os: &str) -> String {
    match os {
        "macos" => "macOS".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().chain(chars).collect::<String>()
            })
        }
    }
}
