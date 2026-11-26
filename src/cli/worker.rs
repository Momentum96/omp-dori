//! `cataly worker …` (docs/design/workers.md 3, 4.2): `add` and `init` act on this machine's files
//! (the hub's keys and pins, or the worker's `authorized_keys`); `list`, `doctor` and `remove` ask
//! the hub; `serve` is worker mode, started by sshd.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::args::Parsed;
use super::client::Client;
use super::install::block_on;
use super::{Failure, decode, out};
use crate::proto::method;
use crate::sys::paths::Paths;
use crate::worker::enrol::{self, Destination, HubKey};
use crate::worker::link::{Check, ConnectError, Link, Target, View};

/// `worker doctor` connects and runs checks; `remove` may wait for the worker to answer.
const SLOW_CALL: Duration = Duration::from_secs(180);
const CALL: Duration = Duration::from_secs(30);

/// The verbs that run without a hub; `None` when `p` is not one of them.
pub(super) fn local(p: &Parsed, paths: &Paths) -> Option<Result<(), Failure>> {
    match p.spec.path {
        ["worker", "add"] => Some(block_on(add(p, paths))),
        ["worker", "init"] => Some(block_on(init(p, paths))),
        ["worker", "serve"] => Some(match p.value("--hub") {
            Some(hub) => match crate::worker::serve::run(hub, paths) {
                0 => Ok(()),
                code => Err(Failure::Status(code, "worker mode ended".into())),
            },
            None => Err(Failure::Usage("worker serve needs --hub <id>".into())),
        }),
        _ => None,
    }
}

/// The verbs the hub answers; `None` when `p` is not one of them.
pub(super) fn remote(p: &Parsed, hub: &mut Client, as_json: bool) -> Option<Result<(), Failure>> {
    let name = p.args.first().cloned();
    let (m, timeout) = match p.spec.path {
        ["worker", "list"] => (method::WORKER_LIST, CALL),
        ["worker", "doctor"] => (method::WORKER_DOCTOR, SLOW_CALL),
        ["worker", "remove"] => (method::WORKER_REMOVE, SLOW_CALL),
        _ => return None,
    };
    let reply = match hub.call(m, json!({ "name": name }), timeout) {
        Ok(r) => r,
        Err(e) => return Some(Err(e.into())),
    };
    if as_json {
        out::json(&reply);
        if m == method::WORKER_DOCTOR && !all_ok(&reply) {
            return Some(Err(Failure::Status(
                super::exit::FAILED,
                "a check failed".into(),
            )));
        }
        return Some(Ok(()));
    }
    Some(match m {
        method::WORKER_LIST => {
            decode::<Vec<View>>(reply.get("workers").cloned().unwrap_or_default())
                .map(|views| print_list(&views))
        }
        method::WORKER_DOCTOR => print_doctor(&reply),
        _ => {
            out::line(&format!("{} removed", name.unwrap_or_default()));
            if let Some(todo) = reply.get("todo").and_then(Value::as_str) {
                out::line(todo);
            }
            Ok(())
        }
    })
}

fn all_ok(reply: &Value) -> bool {
    reply
        .get("workers")
        .and_then(Value::as_object)
        .is_none_or(|all| {
            all.values()
                .filter_map(Value::as_array)
                .flatten()
                .all(|c| c.get("ok").and_then(Value::as_bool).unwrap_or(false))
        })
}

fn print_doctor(reply: &Value) -> Result<(), Failure> {
    let all = reply
        .get("workers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if all.is_empty() {
        out::line("no workers; enrol one with cataly worker add user@host");
        return Ok(());
    }
    for (name, checks) in &all {
        out::line(&format!("{name}:"));
        let checks: Vec<Check> = serde_json::from_value(checks.clone()).unwrap_or_default();
        for c in checks {
            let mark = if c.ok { "ok  " } else { "FAIL" };
            out::line(&format!("  {mark} {:<16} {}", c.name, c.detail));
        }
    }
    if all_ok(reply) {
        Ok(())
    } else {
        Err(Failure::Status(
            super::exit::FAILED,
            "a check failed".into(),
        ))
    }
}

/// One line per worker: name, destination, state, version and compatibility, lanes, last seen.
fn print_list(views: &[View]) {
    if views.is_empty() {
        out::line("no workers; enrol one with cataly worker add user@host");
        return;
    }
    for v in views {
        out::line(&describe(v));
    }
}

/// The line `worker list` and `cataly status` show for one worker.
pub(super) fn describe(v: &View) -> String {
    let state = serde_json::to_value(v.state)
        .ok()
        .and_then(|s| s.as_str().map(str::to_string))
        .unwrap_or_default();
    let port = if v.port == 22 {
        String::new()
    } else {
        format!(":{}", v.port)
    };
    let mut line = format!(
        "{:<14} {}{port}  {state}  lanes {}/{}",
        v.name, v.destination, v.lanes, v.lanes_max
    );
    if let (Some(version), Some(os), Some(arch)) = (&v.version, &v.os, &v.arch) {
        let _ = write!(line, "  cataly {version} ({os} {arch})");
    }
    if let Some(omp) = &v.omp {
        let _ = write!(line, "  {omp}");
    }
    if let Some(seen) = v.seen_ms {
        let _ = write!(line, "  seen {}", crate::jobs::instant_text(seen));
    }
    if let Some(reason) = &v.reason {
        let _ = write!(line, "\n{:<14} {reason}", "");
    }
    line
}

fn config_dir(paths: &Paths) -> Result<PathBuf, Failure> {
    paths
        .config_file
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Failure::Refused("the config file has no folder".into()))
}

/// What `worker add` enrols.
struct Enrolment {
    name: String,
    /// The destination as typed, and the machine name in it.
    typed: String,
    machine: String,
    dest: Destination,
    tailscale: bool,
    lanes: u32,
    repos: Vec<String>,
}

/// `--lanes` and `--repo`.
fn add_options(p: &Parsed) -> Result<(u32, Vec<String>), Failure> {
    let lanes = match p.value("--lanes") {
        Some(n) => n
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=crate::worker::LANES_MAX).contains(n))
            .ok_or_else(|| Failure::Usage("--lanes takes 1 to 8".into()))?,
        None => 1,
    };
    let repos = match p.value("--repo") {
        Some(list) => list
            .split(',')
            .map(|r| {
                crate::config::workers::normalise_origin(r)
                    .ok_or_else(|| Failure::Usage(format!("--repo: '{r}' is not a Git origin")))
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    Ok((lanes, repos))
}

/// `worker add user@host[:port]`: the first run makes the key and prints the `worker init` line;
/// with `--host-key` it pins, connects, checks and writes the config section.
async fn add(p: &Parsed, paths: &Paths) -> Result<(), Failure> {
    let as_json = p.flag("--json");
    let typed = p.args.first().cloned().unwrap_or_default();
    let mut dest = Destination::parse(&typed).map_err(Failure::Usage)?;
    let tailscale = p.flag("--tailscale");
    let machine = dest.host.clone();
    let mut from = Vec::new();
    if tailscale {
        if crate::omp::resolve("tailscale").is_none() {
            return Err(Failure::Refused(
                "--tailscale needs the tailscale command on this machine".into(),
            ));
        }
        dest.host = enrol::tailscale_ip(&machine)
            .await
            .map_err(|e| Failure::Refused(format!("{e}; nothing was enrolled")))?;
        from = enrol::own_tailscale_ips().await.map_err(Failure::Refused)?;
    }
    let name = match p.value("--name") {
        Some(n) => n.to_string(),
        None => machine.split('.').next().unwrap_or(&machine).to_string(),
    };
    if !enrol::name_ok(&name) {
        return Err(Failure::Usage(format!(
            "'{name}' is not a worker name; give one with --name"
        )));
    }
    let (lanes, repos) = add_options(p)?;
    let dir = config_dir(paths)?;
    let hub = enrol::hub_id(&dir).map_err(Failure::Refused)?;
    let key = enrol::key_path(&dir, &name);
    let public = enrol::ensure_key(&key, &hub)
        .await
        .map_err(Failure::Refused)?;
    let enrolment = Enrolment {
        name,
        typed,
        machine,
        dest,
        tailscale,
        lanes,
        repos,
    };
    let Some(expected) = p.value("--host-key") else {
        print_init(&enrolment, &public, &from, p.value("--name"), as_json);
        return Ok(());
    };
    pin_and_write(&enrolment, expected, key, paths, as_json).await
}

/// The first step's answer: the line to run on the worker, then the second step here.
fn print_init(e: &Enrolment, public: &str, from: &[String], name: Option<&str>, as_json: bool) {
    let from_arg = if from.is_empty() {
        String::new()
    } else {
        format!(" --from {}", from.join(","))
    };
    let init = format!("cataly worker init --hub-key '{public}'{from_arg}");
    let again = format!(
        "cataly worker add {}{}{} --host-key SHA256:…",
        e.typed,
        if e.tailscale { " --tailscale" } else { "" },
        name.map_or_else(String::new, |n| format!(" --name {n}"))
    );
    if as_json {
        out::json(&json!({ "name": e.name, "init": init, "then": again }));
    } else {
        out::line(&format!(
            "On {}, as the user its lanes will run as (cataly installed, OMP logged in), run:",
            e.machine
        ));
        out::line(&format!("  {init}"));
        out::line("It prints the machine's host-key fingerprint. Then run here:");
        out::line(&format!("  {again}"));
    }
}

/// The second step: pin the host key `worker init` printed, connect, check, write the section.
async fn pin_and_write(
    e: &Enrolment,
    expected: &str,
    key: PathBuf,
    paths: &Paths,
    as_json: bool,
) -> Result<(), Failure> {
    let (name, dest, tailscale, lanes) = (&e.name, &e.dest, e.tailscale, e.lanes);
    let dir = config_dir(paths)?;
    // Pin: the scanned key must be the one `worker init` printed.
    let blob = enrol::scan(dest).await.map_err(Failure::Refused)?;
    let found = enrol::fingerprint(&blob).unwrap_or_default();
    if found != expected.trim() {
        return Err(Failure::Refused(format!(
            "the host key of {} is {found}, not the {expected} worker init printed; nothing was \
             pinned. If Tailscale SSH takes port 22 of the tailnet, run OpenSSH on another port \
             (for example 2222) and enrol user@host:2222",
            dest.pin_name()
        )));
    }
    let pins = enrol::known_hosts(&dir);
    let text = std::fs::read_to_string(&pins).unwrap_or_default();
    crate::sys::fs::write_atomic(
        &pins,
        enrol::set_pin(&text, &dest.pin_name(), &blob).as_bytes(),
    )
    .map_err(|e| Failure::Refused(e.to_string()))?;
    let target = Target {
        name: name.clone(),
        dest: dest.clone(),
        key,
        known_hosts: pins,
        stderr: paths.logs_dir.join(format!("worker-{name}.err")),
    };
    let link = match Link::connect(&target).await {
        Ok((link, _)) => link,
        Err(ConnectError::NoForcedCommand) => {
            return Err(Failure::Refused(format!(
                "{}; nothing was enrolled",
                ConnectError::NoForcedCommand
            )));
        }
        Err(e) => {
            return Err(Failure::Refused(format!(
                "{name}: {e}; nothing was enrolled"
            )));
        }
    };
    let hello = link.hello.clone();
    link.close().await;
    write_section(e, &paths.config_file)?;
    let reloaded = match Client::connect(&paths.socket) {
        Ok(mut hub) => hub.call(method::CONFIG_RELOAD, json!({}), CALL).is_ok(),
        Err(_) => false,
    };
    let omp = hello
        .omp
        .version
        .clone()
        .unwrap_or_else(|| hello.omp.text());
    if as_json {
        out::json(&json!({
            "name": name,
            "destination": dest.target(),
            "port": dest.port,
            "tailscale": tailscale,
            "hello": hello,
            "reloaded": reloaded,
        }));
    } else {
        out::line(&format!(
            "{name} enrolled · {} {} · cataly {} (compatible) · {omp} · up to {lanes} lane{}",
            hello.os,
            hello.arch,
            hello.version,
            if lanes == 1 { "" } else { "s" }
        ));
        if let Some(e) = &hello.bridge_link {
            out::line(&format!("warning: on {name}, {e}"));
        }
        if !reloaded {
            out::line("the hub is not running; it reads [workers] when it starts");
        }
    }
    Ok(())
}

/// `[workers.<name>]` through the shared config writer.
fn write_section(e: &Enrolment, config_file: &Path) -> Result<(), Failure> {
    let section = format!("workers.{}", e.name);
    crate::config::file::edit(config_file, "", |text| {
        use crate::config::file::{set_key, set_rendered, string_list};
        let mut t = set_key(text, &section, "destination", &e.dest.target());
        t = set_rendered(&t, &section, "port", &e.dest.port.to_string());
        t = set_key(&t, &section, "name", &e.machine);
        let tailscale = if e.tailscale { "true" } else { "false" };
        t = set_rendered(&t, &section, "tailscale", tailscale);
        t = set_rendered(&t, &section, "repos", &string_list(&e.repos));
        t = set_rendered(&t, &section, "lanes", &e.lanes.to_string());
        Ok(t)
    })
    .map(|_| ())
    .map_err(Failure::Refused)
}

/// The program the forced command runs: the installed command link (or the package's binary),
/// which an update keeps; else this binary.
fn command_path() -> Result<PathBuf, Failure> {
    if let Ok(layout) = crate::sys::install::Layout::of_running() {
        let command = layout.service_command();
        if command.exists() {
            return Ok(command);
        }
    }
    std::env::current_exe().map_err(|e| Failure::Refused(format!("this binary's path: {e}")))
}

/// Links the bridge for a package install, writes `worker.toml` (OMP's path and this `PATH`, which
/// sshd does not give the forced command), and lists what a lane would miss here.
fn prepare_worker(paths: &Paths) -> Result<Vec<String>, Failure> {
    // An npm or Bun install links its bridge when its hub starts; a worker runs no hub.
    if let Some(package) = crate::sys::package::Package::of_running()
        && let Err(e) = package.link_extension()
    {
        out::error_line(&format!("bridge link: {e}"));
    }
    let omp = crate::omp::resolve("omp");
    let worker_file = crate::worker::serve::WorkerFile {
        omp: omp.as_ref().map(|p| p.display().to_string()),
        path: std::env::var("PATH").ok(),
    };
    let wf = crate::worker::serve::WorkerFile::path(paths);
    if let Some(dir) = wf.parent() {
        crate::sys::fs::claim_private_dir(dir).map_err(|e| Failure::Refused(e.to_string()))?;
    }
    crate::sys::fs::write_atomic(&wf, worker_file.render().as_bytes())
        .map_err(|e| Failure::Refused(e.to_string()))?;
    let mut problems = Vec::new();
    if omp.is_none() {
        problems.push(
            "omp is not on PATH; install OMP and log in, then run worker init again".to_string(),
        );
    }
    if crate::omp::resolve("git").is_none() {
        problems.push("git is not on PATH".to_string());
    }
    if let Err(e) = crate::omp::extension::check() {
        problems.push(e);
    }
    Ok(problems)
}

/// `worker init --hub-key '…'`: on the worker, the forced-command line, the checks and the
/// fingerprint for `worker add`.
async fn init(p: &Parsed, paths: &Paths) -> Result<(), Failure> {
    let as_json = p.flag("--json");
    let key = HubKey::parse(p.value("--hub-key").ok_or_else(|| {
        Failure::Usage("worker init needs --hub-key '<the line worker add printed>'".into())
    })?)
    .map_err(|e| Failure::Usage(format!("--hub-key: {e}")))?;
    let file = crate::worker::serve::authorized_keys().map_err(Failure::Refused)?;
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    if p.flag("--remove") {
        let kept = enrol::replace_line(&text, &key.hub, None);
        if kept != text {
            crate::sys::fs::write_atomic(&file, kept.as_bytes())
                .map_err(|e| Failure::Refused(e.to_string()))?;
        }
        out::line(&format!(
            "the line of hub {} is {} {}",
            key.hub,
            if kept == text {
                "not in"
            } else {
                "removed from"
            },
            file.display()
        ));
        return Ok(());
    }
    let from: Vec<String> = p
        .value("--from")
        .map_or_default(|f| f.split(',').map(|s| s.trim().to_string()).collect());
    let program = command_path()?;
    let line = enrol::authorized_line(&program, &key, &from).map_err(Failure::Usage)?;
    // `~/.ssh` is the user's: made owner-only when missing, left as it is otherwise (sshd checks it).
    if let Some(dir) = file.parent()
        && std::fs::symlink_metadata(dir).is_err()
    {
        crate::sys::fs::ensure_private_dir(dir).map_err(|e| Failure::Refused(e.to_string()))?;
    }
    crate::sys::fs::write_atomic(
        &file,
        enrol::replace_line(&text, &key.hub, Some(&line)).as_bytes(),
    )
    .map_err(|e| Failure::Refused(e.to_string()))?;
    let problems = prepare_worker(paths)?;
    let host_key_file = p.value("--host-key-file").map_or_else(
        || PathBuf::from("/etc/ssh/ssh_host_ed25519_key.pub"),
        PathBuf::from,
    );
    let fingerprint = std::fs::read_to_string(&host_key_file)
        .ok()
        .and_then(|t| enrol::ed25519_key(&t))
        .and_then(|b| enrol::fingerprint(&b));
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let host = this_host().await;
    let add = fingerprint
        .as_ref()
        .map(|fp| format!("cataly worker add {user}@{host} --host-key {fp}"));
    if as_json {
        out::json(&json!({
            "line": line,
            "authorized_keys": file.display().to_string(),
            "fingerprint": fingerprint,
            "add": add,
            "problems": problems,
        }));
    } else {
        out::line(&format!(
            "hub {} may now start worker mode as {user} ({})",
            key.hub,
            file.display()
        ));
        out::line(&format!(
            "Lanes from that hub run tools with {user}'s rights: they can read and change everything \
             {user} can. On a machine someone uses daily, enrol a dedicated OS user with its own OMP login."
        ));
        for problem in &problems {
            out::line(&format!("warning: {problem}"));
        }
        if !from.is_empty() {
            out::line(
                "If sshd listens on the tailnet address, start it after tailscaled (Linux: a drop-in \
                 for ssh.service with After=tailscaled.service and Wants=tailscaled.service).",
            );
        }
        match add {
            Some(add) => {
                out::line("On the hub, run:");
                out::line(&format!("  {add}"));
            }
            None => out::line(&format!(
                "no ed25519 host key at {}; give one with --host-key-file",
                host_key_file.display()
            )),
        }
    }
    Ok(())
}

/// This machine's name as `uname -n` prints it.
async fn this_host() -> String {
    let argv = ["uname".to_string(), "-n".to_string()];
    let done = crate::sys::command::run(&crate::sys::command::Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: Duration::from_secs(5),
        kill_grace: Duration::from_secs(1),
        output_cap: 1024,
        output: crate::sys::command::Output::Capture,
    })
    .await;
    done.ok()
        .filter(crate::sys::command::Finished::success)
        .and_then(|d| String::from_utf8(d.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this-host".into())
}
