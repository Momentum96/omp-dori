//! `cataly install | update | rollback | uninstall | check | version | extension | service`, and
//! `cataly status --install` (DESIGN 11; IR req. 13-35, 40). Each operation that changes the
//! program folder holds its lock, verifies before it switches, and ends in one of four outcomes with
//! its own exit status: done, held (nothing changed), switched back, needs attention.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::json;

use super::args::Parsed;
use super::client::{Client, ClientError};
use super::{Failure, exit, out};
use crate::proto::{Status, method};
use crate::sys::install::{self as layout_ops, Layout, Staged};
use crate::sys::lock::{self, HubLock, LockOutcome};
use crate::sys::paths::Paths;
use crate::sys::release::{self, Version};

/// The daemon socket answer that decides "running" (IR req. 26).
const HUB_ANSWER: Duration = Duration::from_secs(1);
/// The orderly stop: asked until accepted, at most this long (IR req. 33).
const STOP_WAIT: Duration = Duration::from_mins(1);
/// The new hub must answer with its version within this time.
const START_WAIT: Duration = Duration::from_secs(30);
/// `omp --version`: 5 s (IR req. 25).
const OMP_TIMEOUT: Duration = Duration::from_secs(5);

/// The four outcomes (IR req. 34).
enum Outcome {
    Done(String),
    Held(String),
    SwitchedBack(String),
    NeedsAttention(String),
}

impl From<Outcome> for Result<(), Failure> {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Done(m) => {
                out::line(&m);
                Ok(())
            }
            Outcome::Held(m) => Err(Failure::Status(exit::HELD, format!("held: {m}"))),
            Outcome::SwitchedBack(m) => Err(Failure::Status(
                exit::SWITCHED_BACK,
                format!("switched back: {m}"),
            )),
            Outcome::NeedsAttention(m) => Err(Failure::Status(
                exit::NEEDS_ATTENTION,
                format!("needs attention: {m}"),
            )),
        }
    }
}

/// Handles the install verbs; `None` when `p` is not one of them.
pub(crate) fn run(p: &Parsed, paths: &Paths) -> Option<Result<(), Failure>> {
    let outcome = match p.spec.path {
        ["version"] => {
            version(p.flag("--json"));
            return Some(Ok(()));
        }
        ["extension"] => {
            out::text(&crate::omp::extension::file());
            return Some(Ok(()));
        }
        ["status"] if p.flag("--install") => return Some(status(p)),
        ["check"] => return Some(block_on(check(p, paths))),
        ["install"] => block_on(install(p, paths)),
        ["update"] => block_on(update(p, paths)),
        ["rollback"] => block_on(rollback(p, paths)),
        ["uninstall"] => block_on(uninstall(p, paths)),
        ["service", verb @ ("enable" | "disable")] => block_on(service(*verb == "enable", paths)),
        _ => return None,
    };
    Some(outcome.into())
}

pub(super) fn block_on<T>(work: impl Future<Output = T>) -> T {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(work),
        Err(e) => {
            out::error_line(&format!("cataly: no runtime: {e}"));
            std::process::exit(i32::from(exit::INTERNAL));
        }
    }
}

fn version(as_json: bool) {
    let r = release::report();
    if as_json {
        out::json(&serde_json::to_value(&r).unwrap_or_default());
    } else {
        out::line(&format!(
            "cataly {} ({}, commit {}; OMP {} or newer; bridge protocol {})",
            r.version, r.target, r.commit, r.omp_minimum, r.extension_protocol
        ));
    }
}

fn layout(p: &Parsed) -> Result<Layout, String> {
    match p.value("--prefix") {
        Some(prefix) => Layout::from_env(Some(Path::new(prefix))),
        None => Layout::of_running(),
    }
}

/// Takes the one install lock without waiting (IR req. 22).
fn take_lock(l: &Layout) -> Result<HubLock, Outcome> {
    layout_ops::prepare_root(l).map_err(Outcome::Held)?;
    match lock::acquire(&l.lock()) {
        Ok(LockOutcome::Acquired(held)) => Ok(held),
        Ok(LockOutcome::Held { .. }) => {
            Err(Outcome::Held("another install operation is running".into()))
        }
        Err(e) => Err(Outcome::Held(e.to_string())),
    }
}

/// The running hub's version when one answers on its socket within 1 s.
fn hub_version(paths: &Paths) -> Option<String> {
    let mut client = Client::connect(&paths.socket).ok()?;
    let reply = client.call(method::STATUS, json!({}), HUB_ANSWER).ok()?;
    serde_json::from_value::<Status>(reply)
        .ok()
        .map(|s| s.version)
}

/// `omp --version` as `(a, b, c)`, or `None` when OMP is absent or says nothing usable.
async fn omp_version() -> Option<(u64, u64, u64)> {
    use crate::sys::command::{KILL_GRACE, Output, Spec};
    let argv = ["omp".to_string(), "--version".to_string()];
    let done = crate::sys::command::run(&Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: OMP_TIMEOUT,
        kill_grace: KILL_GRACE,
        output_cap: 4096,
        output: Output::Capture,
    })
    .await
    .ok()?;
    let v = crate::omp::Version::parse_output(&String::from_utf8_lossy(&done.stdout))?;
    Some((v.0, v.1, v.2))
}

/// IR req. 25: a first install warns; an update or rollback is held while OMP is too old.
async fn omp_gate(minimum: &str, first: bool) -> Result<(), Outcome> {
    let Some(min) = Version::parse(minimum) else {
        return Ok(());
    };
    match omp_version().await {
        None => {
            out::line("warning: OMP was not found; install it before starting the hub");
            Ok(())
        }
        Some(found) if found < min.core => {
            let (a, b, c) = found;
            let why = format!(
                "OMP {a}.{b}.{c} is older than the {minimum} this version needs; update OMP first with `omp update`"
            );
            if first {
                out::line(&format!("warning: {why}"));
                Ok(())
            } else {
                Err(Outcome::Held(why))
            }
        }
        Some(_) => Ok(()),
    }
}

fn path_note(l: &Layout) {
    let dir = l.command.parent().map_or_default(Path::to_path_buf);
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    if !on_path {
        out::line(&format!(
            "note: {} is not on PATH; add it in your shell profile to run `cataly` by name",
            dir.display()
        ));
    }
}

/// `cataly install --from <folder>`: a first install, a reinstall (no change) or an update from a
/// local folder of release assets (IR req. 19).
async fn install(p: &Parsed, paths: &Paths) -> Outcome {
    let Some(from) = p.value("--from").map(PathBuf::from) else {
        return Outcome::Held("--from <folder of release assets> is required".into());
    };
    let l = match layout(p) {
        Ok(l) => l,
        Err(e) => return Outcome::Held(e),
    };
    // A foreign file at a link path refuses before anything, the program folder included, exists.
    if let Err(e) = layout_ops::check_links(&l) {
        return Outcome::Held(e);
    }
    let _lock = match take_lock(&l) {
        Ok(g) => g,
        Err(o) => return o,
    };
    if let Err(e) = layout_ops::clear_staging(&l) {
        return Outcome::Held(e);
    }
    let source = std::fs::canonicalize(&from)
        .unwrap_or(from)
        .display()
        .to_string();
    from_folder(&l, paths, Path::new(&source), &source, false).await
}

/// Verifies the folder, then installs, reinstalls or updates. Holds the lock.
async fn from_folder(
    l: &Layout,
    paths: &Paths,
    from: &Path,
    source: &str,
    restart: bool,
) -> Outcome {
    if let Err(e) = layout_ops::check_links(l) {
        return Outcome::Held(e);
    }
    let staged = match layout_ops::stage(l, from, source).await {
        Ok(s) => s,
        Err(e) => return Outcome::Held(format!("{e}; nothing changed")),
    };
    let Some(current) = l.current_version() else {
        return first_install(l, staged).await;
    };
    let (new, old) = (
        Version::parse(&staged.record.version),
        Version::parse(&current),
    );
    match (new, old) {
        (Some(n), Some(o)) if n == o => {
            let result = layout_ops::place(l, staged).and_then(|_| layout_ops::create_links(l));
            match result {
                Ok(()) => Outcome::Done(format!(
                    "cataly {current} is already installed; nothing changed"
                )),
                Err(e) => Outcome::Held(e),
            }
        }
        (Some(n), Some(o)) if n < o => {
            staged.discard();
            Outcome::Held(format!(
                "{n} is older than the installed {o}; only `cataly rollback` goes back"
            ))
        }
        _ => switch_to(l, paths, staged, &current, restart).await,
    }
}

async fn first_install(l: &Layout, staged: Staged) -> Outcome {
    let report = release::candidate_report(&staged.binary()).await;
    let minimum = report.map_or_default(|r| r.omp_minimum);
    let _ = omp_gate(&minimum, true).await;
    let version = staged.record.version.clone();
    let result = layout_ops::place(l, staged)
        .and_then(|_| layout_ops::switch(l, &version))
        .and_then(|()| layout_ops::settle_records(l, None).map(drop))
        .and_then(|()| layout_ops::create_links(l))
        .and_then(|()| layout_ops::retain(l));
    match result {
        Ok(()) => {
            path_note(l);
            Outcome::Done(format!(
                "cataly {version} installed in {}; nothing was started (see `cataly setup`)",
                l.root.display()
            ))
        }
        Err(e) => Outcome::NeedsAttention(format!("{e}; run `cataly status --install`")),
    }
}

/// Activates a newer staged version over `current`: held while a hub answers unless `restart`.
async fn switch_to(
    l: &Layout,
    paths: &Paths,
    staged: Staged,
    current: &str,
    restart: bool,
) -> Outcome {
    let report = match release::candidate_report(&staged.binary()).await {
        Ok(r) => r,
        Err(e) => {
            staged.discard();
            return Outcome::Held(e);
        }
    };
    if let Err(o) = omp_gate(&report.omp_minimum, false).await {
        staged.discard();
        return o;
    }
    let running = hub_version(paths).is_some();
    if running && !restart {
        staged.discard();
        return Outcome::Held("the hub is running; stop it first or add --restart".into());
    }
    let closed = if running {
        if let Err(why) = stop_hub(paths).await {
            staged.discard();
            return Outcome::Held(format!("{why}; nothing changed"));
        }
        Some(Instant::now())
    } else {
        None
    };
    let version = staged.record.version.clone();
    if let Err(e) = layout_ops::place(l, staged) {
        if running && start_and_wait(l, paths, current).await.is_err() {
            return Outcome::NeedsAttention(format!("{e}; the hub did not start again"));
        }
        return Outcome::Held(e);
    }
    if let Some(closed) = closed {
        return restart_switch(l, paths, &version, current, closed).await;
    }
    match activate(l, &version, current) {
        Ok(()) => Outcome::Done(format!("cataly {current} -> {version}")),
        Err(e) => Outcome::NeedsAttention(format!("{e}; run `cataly status --install`")),
    }
}

fn activate(l: &Layout, version: &str, was: &str) -> Result<(), String> {
    layout_ops::switch(l, version)?;
    layout_ops::settle_records(l, Some(was))?;
    layout_ops::retain(l)
}

/// IR req. 33: orderly stop (refused while busy, held after 60 s), switch, start the new hub the
/// way the old one ran, and wait 30 s for it to report the new version; else switch back.
async fn restart_switch(
    l: &Layout,
    paths: &Paths,
    version: &str,
    was: &str,
    closed: Instant,
) -> Outcome {
    if let Err(e) = layout_ops::switch(l, version)
        .and_then(|()| layout_ops::settle_records(l, Some(was)).map(drop))
    {
        return Outcome::NeedsAttention(format!(
            "{e}; the hub is stopped; run `cataly status --install`"
        ));
    }
    match start_and_wait(l, paths, version).await {
        Ok(()) => {
            let _ = layout_ops::retain(l);
            Outcome::Done(format!(
                "cataly {was} -> {version}; the new hub answered {} ms after the old one closed",
                closed.elapsed().as_millis()
            ))
        }
        Err(why) => {
            let _ = stop_hub(paths).await;
            let back = layout_ops::switch(l, was)
                .and_then(|()| layout_ops::settle_records(l, Some(version)).map(drop));
            if let Err(e) = back {
                return Outcome::NeedsAttention(format!(
                    "{why}; switching back to {was} failed: {e}"
                ));
            }
            match start_and_wait(l, paths, was).await {
                Ok(()) => Outcome::SwitchedBack(format!(
                    "{version} did not come up ({why}); {was} runs again"
                )),
                Err(e) => Outcome::NeedsAttention(format!(
                    "{version} did not come up ({why}) and {was} did not either ({e})"
                )),
            }
        }
    }
}

/// Asks the hub to stop until it accepts (busy sessions refuse), then waits for its socket to close.
async fn stop_hub(paths: &Paths) -> Result<(), String> {
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        let answer = Client::connect(&paths.socket)
            .and_then(|mut c| c.call(method::HUB_STOP, json!({}), Duration::from_secs(5)));
        match answer {
            Ok(_) | Err(ClientError::NoHub | ClientError::Io(_)) => break,
            Err(ClientError::Remote(body)) if Instant::now() < deadline => {
                out::line(&format!("waiting: {}", body.message));
            }
            Err(e) => return Err(format!("the hub did not stop: {e}")),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    while hub_version(paths).is_some() {
        if Instant::now() >= deadline {
            return Err("the hub's socket stayed open".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Starts the hub through the user service Cataly created, else detached, and waits for it.
async fn start_and_wait(l: &Layout, paths: &Paths, version: &str) -> Result<(), String> {
    if l.service_marker().exists() {
        crate::sys::service::start().await?;
    } else {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(l.command_target())
            .arg("hub")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("the hub did not start: {e}"))?;
    }
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if hub_version(paths).as_deref() == Some(version) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(format!("no hub reported {version} within 30 s"))
}

/// `cataly update [--from <folder> | --tag <tag>] [--restart]`.
async fn update(p: &Parsed, paths: &Paths) -> Outcome {
    let l = match layout(p) {
        Ok(l) => l,
        Err(e) => return Outcome::Held(e),
    };
    let _lock = match take_lock(&l) {
        Ok(g) => g,
        Err(o) => return o,
    };
    let Some(current) = l.current_version() else {
        return Outcome::Held("Cataly is not installed here; use install.sh first".into());
    };
    let restart = p.flag("--restart");
    if let Err(e) = layout_ops::clear_staging(&l) {
        return Outcome::Held(e);
    }
    if let Some(from) = p.value("--from") {
        let source = std::fs::canonicalize(from)
            .map_or_else(|_| from.to_string(), |p| p.display().to_string());
        return from_folder(&l, paths, Path::new(&source), &source, restart).await;
    }
    let release = match fetch(paths, p.value("--tag")).await {
        Ok(r) => r,
        Err(e) => return Outcome::Held(e),
    };
    if Version::parse(&current).is_some_and(|c| release.version <= c) {
        return Outcome::Done(format!(
            "cataly {current} is up to date ({} is the release)",
            release.tag
        ));
    }
    let dir = match download(&l, &release).await {
        Ok(d) => d,
        Err(e) => return Outcome::Held(format!("{e}; nothing changed")),
    };
    let source = format!(
        "https://github.com/{}/releases/tag/{}",
        release::REPOSITORY,
        release.tag
    );
    let outcome = from_folder(&l, paths, &dir, &source, restart).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Release metadata through the config's API base (a loopback fake in tests).
async fn fetch(paths: &Paths, tag: Option<&str>) -> Result<release::Release, String> {
    let settings = if paths.config_file.exists() {
        crate::config::load(&paths.config_file).map_or_default(|c| c.update)
    } else {
        crate::config::release::UpdateSettings::default()
    };
    match release::fetch_release(&settings.api()?, tag).await? {
        Some(r) if r.version.stable() || tag.is_some() => Ok(r),
        _ => Err("no stable release is published yet; name one with --tag v<version>".into()),
    }
}

/// Downloads the four assets this host needs into a fresh staging folder.
async fn download(l: &Layout, r: &release::Release) -> Result<PathBuf, String> {
    let dir = layout_ops::fresh_staging(l, "download")?;
    let v = r.version.to_string();
    for (name, cap) in [
        (release::SUMS.to_string(), release::SUMS_MAX),
        (release::SIGNATURE.to_string(), release::SIGNATURE_MAX),
        (release::binary_asset(&v), release::BINARY_MAX),
        (release::extension_asset(&v), release::EXTENSION_MAX),
    ] {
        let url = r.asset(&name)?;
        release::download(url, cap, &dir.join(&name)).await?;
    }
    Ok(dir)
}

/// `cataly rollback [--expect <version>] [--restart]` (IR req. 30).
async fn rollback(p: &Parsed, paths: &Paths) -> Outcome {
    let l = match layout(p) {
        Ok(l) => l,
        Err(e) => return Outcome::Held(e),
    };
    let _lock = match take_lock(&l) {
        Ok(g) => g,
        Err(o) => return o,
    };
    let (Some(current), Some(previous)) = (l.current_version(), layout_ops::role(&l, "previous"))
    else {
        return Outcome::Held("there is no previous version to roll back to".into());
    };
    if let Some(expect) = p.value("--expect")
        && expect.trim_start_matches('v') != previous.version
    {
        return Outcome::Held(format!(
            "the previous version is {}, not {expect}",
            previous.version
        ));
    }
    if let Err(e) = layout_ops::verify_files(&l, &previous) {
        return Outcome::Held(format!("{e}; nothing changed"));
    }
    let binary = l.version_dir(&previous.version).join(layout_ops::BINARY);
    match release::candidate_report(&binary).await {
        Ok(r) => {
            if let Err(o) = omp_gate(&r.omp_minimum, false).await {
                return o;
            }
        }
        Err(e) => return Outcome::Held(e),
    }
    let running = hub_version(paths).is_some();
    if running && !p.flag("--restart") {
        return Outcome::Held("the hub is running; stop it first or add --restart".into());
    }
    if running && let Err(why) = stop_hub(paths).await {
        return Outcome::Held(format!("{why}; nothing changed"));
    }
    // Only now is the restart known not to be held: the version rolled back from is never
    // announced again (IR req. 37).
    if let Err(e) = crate::sys::fs::write_atomic(&l.silenced(), current.as_bytes()) {
        if running && start_and_wait(&l, paths, &current).await.is_err() {
            return Outcome::NeedsAttention(format!("{e}; the hub did not start again"));
        }
        return Outcome::Held(e.to_string());
    }
    if running {
        return restart_switch(&l, paths, &previous.version, &current, Instant::now()).await;
    }
    match layout_ops::switch(&l, &previous.version)
        .and_then(|()| layout_ops::settle_records(&l, Some(&current)).map(drop))
    {
        Ok(()) => Outcome::Done(format!(
            "rolled back: cataly {current} -> {}",
            previous.version
        )),
        Err(e) => Outcome::NeedsAttention(format!("{e}; run `cataly status --install`")),
    }
}

/// `cataly uninstall` (IR req. 35).
async fn uninstall(p: &Parsed, paths: &Paths) -> Outcome {
    let l = match layout(p) {
        Ok(l) => l,
        Err(e) => return Outcome::Held(e),
    };
    let lock = match take_lock(&l) {
        Ok(g) => g,
        Err(o) => return o,
    };
    if hub_version(paths).is_some() {
        return Outcome::Held("the hub is running; stop it first".into());
    }
    if let Err(e) = crate::sys::service::disable(&l).await {
        return Outcome::Held(e);
    }
    drop(lock);
    match layout_ops::remove_all(&l) {
        Ok(()) => Outcome::Done(format!(
            "removed {}, the command link and the extension link; config and state are kept",
            l.root.display()
        )),
        Err(e) => Outcome::NeedsAttention(e),
    }
}

/// `cataly service enable|disable` (IR req. 40).
async fn service(enable: bool, paths: &Paths) -> Outcome {
    let l = match Layout::of_running() {
        Ok(l) => l,
        Err(e) => return Outcome::Held(e),
    };
    if !enable {
        return match crate::sys::service::disable(&l).await {
            Ok(Some(path)) => {
                Outcome::Done(format!("service stopped and {} removed", path.display()))
            }
            Ok(None) => Outcome::Done("no service created by Cataly; nothing changed".into()),
            Err(e) => Outcome::Held(e),
        };
    }
    if l.current_version().is_none() {
        return Outcome::Held(
            "install Cataly first; the service runs the installed command".into(),
        );
    }
    let config = match crate::config::load(&paths.config_file) {
        Ok(c) => c,
        Err(e) => {
            return Outcome::Held(format!(
                "{} is not a valid config ({}); fix it first",
                paths.config_file.display(),
                crate::config::describe(&e)
            ));
        }
    };
    let Some(omp) = crate::omp::resolve(&config.omp.path) else {
        return Outcome::Held(format!(
            "OMP ('{}') is not an executable file or on PATH; install it or set [omp] path",
            config.omp.path
        ));
    };
    // The service's PATH is not the shell's: a bare name would not be found there.
    let mut notes = Vec::new();
    if !Path::new(&config.omp.path).is_absolute() {
        let value = omp.display().to_string();
        if let Err(e) = super::setup::write_one(paths, "omp", "path", &value) {
            return Outcome::Held(e);
        }
        notes.push(format!("[omp] path set to {value}"));
    }
    // The folders of the tools the hub runs, as found now.
    let tools = [
        config.omp.path.as_str(),
        "bun",
        "gh",
        "git",
        &config.sessions.tern,
    ];
    let found: Vec<PathBuf> = tools
        .iter()
        .filter_map(|t| crate::omp::resolve(t))
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    let path_line = crate::sys::service::path_line(found.iter().map(PathBuf::as_path));
    let env_path = service_env_file(paths);
    let keys = match crate::sys::service::env_file(&env_path, &config.sessions.credentials, |k| {
        std::env::var(k).ok()
    }) {
        Ok(k) => k,
        Err(e) => return Outcome::Held(e),
    };
    let unit = crate::sys::service::Unit {
        command: &l.command,
        path: &path_line,
        env_file: &env_path,
    };
    let file = match crate::sys::service::enable(&l, &unit).await {
        Ok(path) => path,
        Err(e) => return Outcome::Held(e),
    };
    notes.push(format!("PATH={path_line}"));
    let verb = if keys.created {
        "written to"
    } else {
        "read from"
    };
    notes.push(format!("provider keys {verb} {}", env_path.display()));
    if !keys.missing.is_empty() {
        notes.push(format!(
            "{} not set there: add NAME=value lines, then restart the service",
            keys.missing.join(", ")
        ));
    }
    Outcome::Done(format!(
        "service written to {} and started\n  {}",
        file.display(),
        notes.join("\n  ")
    ))
}

/// The owner-only environment file the service reads its provider keys from.
fn service_env_file(paths: &Paths) -> PathBuf {
    paths
        .config_file
        .parent()
        .unwrap_or(Path::new("/"))
        .join("secrets")
        .join("service.env")
}

/// `cataly check [--tag <tag>]` (IR req. 18, 32): changes nothing.
async fn check(p: &Parsed, paths: &Paths) -> Result<(), Failure> {
    let installed = Layout::of_running()
        .ok()
        .and_then(|l| l.current_version())
        .unwrap_or_else(|| release::VERSION.to_string());
    let r = fetch(paths, p.value("--tag"))
        .await
        .map_err(Failure::Refused)?;
    let mine = Version::parse(&installed)
        .ok_or(Failure::Refused("unreadable installed version".into()))?;
    let verdict = match r.version.cmp(&mine) {
        std::cmp::Ordering::Greater => "newer",
        std::cmp::Ordering::Equal => "same",
        std::cmp::Ordering::Less => "older",
    };
    if p.flag("--json") {
        out::json(
            &json!({ "installed": installed, "release": r.version.to_string(), "verdict": verdict }),
        );
    } else {
        out::line(&format!(
            "installed {installed}; release {}: {verdict}",
            r.version
        ));
        if verdict == "newer" {
            out::line("install it with `cataly update`");
        }
    }
    Ok(())
}

/// `cataly status --install` (IR req. 31): repairs records only; reports the rest with a command.
fn status(p: &Parsed) -> Result<(), Failure> {
    let l = layout(p).map_err(Failure::Refused)?;
    let locked = lock::acquire(&l.lock());
    let (repaired, problems) = match locked {
        Ok(LockOutcome::Acquired(_guard)) => layout_ops::inspect(&l),
        _ => {
            return Err(Failure::Status(
                exit::HELD,
                "held: another install operation is running".into(),
            ));
        }
    };
    let current = layout_ops::role(&l, "current");
    let previous = layout_ops::role(&l, "previous");
    if p.flag("--json") {
        out::json(&json!({
            "root": l.root.display().to_string(),
            "current": current,
            "previous": previous,
            "repaired": repaired,
            "problems": problems,
        }));
    } else {
        out::line(&format!("program folder {}", l.root.display()));
        for (name, r) in [("current", &current), ("previous", &previous)] {
            match r {
                Some(r) => out::line(&format!(
                    "{name:<9}{} (commit {}, from {}, installed {})",
                    r.version,
                    r.commit,
                    r.source,
                    crate::jobs::instant_text(r.installed_ms)
                )),
                None => out::line(&format!("{name:<9}none")),
            }
        }
        for r in &repaired {
            out::line(&format!("repaired record: {r}"));
        }
        for pr in &problems {
            out::line(&format!(
                "problem: {}: {}; repair: {}",
                pr.path, pr.what, pr.repair
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::Status(
            exit::NEEDS_ATTENTION,
            format!("{} problem(s) found", problems.len()),
        ))
    }
}
