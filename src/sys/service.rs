//! The optional user service (IR req. 40, DESIGN 11.2): a systemd user unit on Linux, a launchd agent
//! on macOS. It runs the hub through the command link, restarts it 5 s after a failure, and is
//! written, enabled, started, stopped and removed only by explicit commands. The service manager is
//! driven through its own command line under the bounded subprocess rules.
//!
//! A service starts with the manager's bare environment, so the definition carries what the hub
//! needs: a PATH holding the folders of the tools it runs, as found when the service was enabled,
//! and an owner-only environment file with the provider keys listed in `[sessions] credentials`.
//! systemd reads that file itself (`EnvironmentFile=`); launchd has no such key, so the agent runs
//! `/bin/sh`, which reads the file and replaces itself with the hub (`exec`, same process).
//!
//! On Linux the unit also caps glibc's malloc arenas at two (`MALLOC_ARENA_MAX`): with one arena per
//! thread the hub's freed memory stayed in a dozen arenas and its RSS kept rising while the live heap
//! stayed level (`docs/measurements.md`, "Heap slope"). Children never see it: their environment is
//! built from an allow-list (`sys::command`).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{KILL_GRACE, Output, Spec, run};
use super::install::Layout;

/// systemd unit name and launchd label.
const UNIT: &str = "cataly.service";
const LABEL: &str = "dev.cataly.hub";
const MANAGER_TIMEOUT: Duration = Duration::from_secs(15);
/// What the service manager's command line needs to reach this user's manager: systemd's user
/// instance is found through these, and `systemctl --user` fails without them ("Failed to connect
/// to bus"). Passed to the manager calls only, never to any other child.
const SESSION_ENV: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];
/// Folders always on the service's PATH, after the tools' own.
const BASE_PATH: [&str; 5] = ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];
/// The largest environment file read back.
const ENV_FILE_MAX: u64 = 64 * 1024;

/// The service definition file for this user.
pub(crate) fn file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or("HOME is not set to an absolute path")?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|d| d.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user")
            .join(UNIT)
    })
}

/// What the service runs and with which environment.
pub(crate) struct Unit<'a> {
    /// The command link the hub runs through.
    pub(crate) command: &'a Path,
    /// The PATH the hub gets (see [`path_line`]).
    pub(crate) path: &'a str,
    /// The owner-only environment file with the provider keys.
    pub(crate) env_file: &'a Path,
}

/// The PATH for the service: the given tool folders in order, then [`BASE_PATH`], each once. A
/// folder that cannot stand in a PATH (holding `:`, a quote, a backslash or a control character) is
/// left out.
pub(crate) fn path_line<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> String {
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    let fixed = BASE_PATH.iter().map(Path::new);
    for dir in dirs.into_iter().chain(fixed) {
        let Some(text) = dir.to_str() else { continue };
        let unusable = |c: char| matches!(c, ':' | '"' | '\\') || c.is_control();
        if text.is_empty() || text.contains(unusable) || !seen.insert(text.to_string()) {
            continue;
        }
        parts.push(text.to_string());
    }
    parts.join(":")
}

/// A value for a systemd unit line: `%` doubled so no specifier is expanded.
fn systemd_escape(text: &str) -> String {
    text.replace('%', "%%")
}

/// Text for a property list string.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The unit or agent text for `unit`.
pub(crate) fn definition(unit: &Unit<'_>) -> String {
    if cfg!(target_os = "macos") {
        let exe = xml_escape(&unit.command.display().to_string());
        let env_file = xml_escape(&unit.env_file.display().to_string());
        let path = xml_escape(unit.path);
        // `$0` is the environment file and `$1` the command: no value is ever parsed as shell text.
        let script = "set -a; if [ -f \"$0\" ]; then . \"$0\"; fi; set +a; exec \"$1\" hub";
        let script = xml_escape(script);
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{LABEL}</string>\n\
  <key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>{script}</string><string>{env_file}</string><string>{exe}</string></array>\n\
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
  <key>ThrottleInterval</key><integer>5</integer>\n\
</dict>\n</plist>\n"
        )
    } else {
        let exe = systemd_escape(&unit.command.display().to_string());
        let env_file = systemd_escape(&unit.env_file.display().to_string());
        let path = systemd_escape(unit.path);
        format!(
            "[Unit]\nDescription=Cataly hub\n\n[Service]\nType=notify\nNotifyAccess=main\n\
Environment=\"PATH={path}\"\nEnvironment=\"MALLOC_ARENA_MAX=2\"\nEnvironmentFile=-{env_file}\n\
ExecStart={exe} hub\nRestart=on-failure\nRestartSec=5\nWatchdogSec=30\n\n\
[Install]\nWantedBy=default.target\n"
        )
    }
}

/// What became of the environment file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EnvFile {
    /// The file was written by this call (it never replaces one that exists).
    pub(crate) created: bool,
    /// Listed credentials the file does not set.
    pub(crate) missing: Vec<String>,
}

/// Whether a value can stand unquoted in a line both systemd and `sh` read the same way.
fn plain_value(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | '+' | '=' | ':' | '@' | ',')
        })
}

/// The names an environment file sets (`NAME=value` lines; comments and blanks skipped).
fn names_set(text: &str) -> HashSet<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(name, _)| name.trim()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// Makes sure the owner-only environment file exists. A new one gets each listed credential whose
/// value `lookup` finds (and that needs no quoting), the rest as commented lines to fill in. An
/// existing file is never changed; the result lists what it lacks.
pub(crate) fn env_file(
    path: &Path,
    credentials: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<EnvFile, String> {
    if let Some(dir) = path.parent() {
        super::fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    if let Some(bytes) = super::fs::read_private(path, ENV_FILE_MAX).map_err(|e| e.to_string())? {
        let text = String::from_utf8_lossy(&bytes);
        let set = names_set(&text);
        let missing = credentials
            .iter()
            .filter(|n| !set.contains(n.as_str()))
            .cloned()
            .collect();
        return Ok(EnvFile {
            created: false,
            missing,
        });
    }
    let mut text = String::from(
        "# Provider keys for the Cataly service: one NAME=value line each, no quotes or spaces.\n\
         # The systemd unit reads this file (EnvironmentFile=); the launchd agent reads it with /bin/sh\n\
         # before it starts the hub. Only the names in [sessions] credentials reach OMP.\n\
         # Keep it mode 600. Restart the service after a change.\n",
    );
    let mut missing = Vec::new();
    for name in credentials {
        if let Some(value) = lookup(name).filter(|v| plain_value(v)) {
            let _ = writeln!(text, "{name}={value}");
        } else {
            let _ = writeln!(text, "# {name}=");
            missing.push(name.clone());
        }
    }
    super::fs::create_atomic(path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(EnvFile {
        created: true,
        missing,
    })
}

/// The service manager's command line and the session variables it needs.
pub(crate) struct Manager {
    program: String,
    env: Vec<(&'static str, String)>,
}

impl Manager {
    /// `systemctl` or `launchctl`, with this process's [`SESSION_ENV`] values.
    pub(crate) fn of_session() -> Self {
        let program = if cfg!(target_os = "macos") {
            "launchctl"
        } else {
            "systemctl"
        };
        Self::new(program, |k| std::env::var(k).ok())
    }

    /// `program` with the [`SESSION_ENV`] values `lookup` finds.
    pub(crate) fn new(program: &str, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let env = SESSION_ENV
            .iter()
            .filter_map(|k| lookup(k).filter(|v| !v.is_empty()).map(|v| (*k, v)))
            .collect();
        Self {
            program: program.to_string(),
            env,
        }
    }

    async fn call(&self, args: &[&str]) -> Result<(), String> {
        let mut command = vec![self.program.clone()];
        command.extend(args.iter().map(ToString::to_string));
        let env: Vec<(&str, &str)> = self.env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let done = run(&Spec {
            argv: &command,
            cwd: None,
            env: &env,
            timeout: MANAGER_TIMEOUT,
            kill_grace: KILL_GRACE,
            output_cap: 64 * 1024,
            output: Output::Capture,
        })
        .await
        .map_err(|e| e.to_string())?;
        if done.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&done.stderr);
        let mut message = format!(
            "`{}` failed: {}",
            command.join(" "),
            said.lines().next().unwrap_or("no message")
        );
        if !cfg!(target_os = "macos") && self.env.is_empty() {
            message.push_str("; XDG_RUNTIME_DIR is not set, run this from your own login session");
        }
        Err(message)
    }

    /// The manager's arguments, `--user` first for systemd.
    async fn user(&self, args: &[&str]) -> Result<(), String> {
        if cfg!(target_os = "macos") {
            self.call(args).await
        } else {
            let mut all = vec!["--user"];
            all.extend_from_slice(args);
            self.call(&all).await
        }
    }
}

fn gui_domain() -> String {
    format!("gui/{}", super::own_uid())
}

/// Writes the definition, enables and starts it, and records that Cataly created it.
pub(crate) async fn enable(layout: &Layout, unit: &Unit<'_>) -> Result<PathBuf, String> {
    let path = file()?;
    install(
        &path,
        &layout.service_marker(),
        unit,
        &Manager::of_session(),
    )
    .await?;
    Ok(path)
}

/// [`enable`] at an explicit file, marker and manager.
async fn install(
    path: &Path,
    marker: &Path,
    unit: &Unit<'_>,
    manager: &Manager,
) -> Result<(), String> {
    if path.exists() && !marker.exists() {
        return Err(format!(
            "{} exists and was not written by Cataly; move it away first",
            path.display()
        ));
    }
    let dir = path.parent().ok_or("the service file has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    super::fs::write_atomic(path, definition(unit).as_bytes()).map_err(|e| e.to_string())?;
    super::fs::write_atomic(marker, path.display().to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    if cfg!(target_os = "macos") {
        let plist = path.display().to_string();
        manager.user(&["bootstrap", &gui_domain(), &plist]).await
    } else {
        manager.user(&["daemon-reload"]).await?;
        manager.user(&["enable", "--now", UNIT]).await
    }
}

/// Stops, disables and removes the service Cataly created; anything else is left alone.
pub(crate) async fn disable(layout: &Layout) -> Result<Option<PathBuf>, String> {
    if !layout.service_marker().exists() {
        return Ok(None);
    }
    let path = file()?;
    let manager = Manager::of_session();
    if cfg!(target_os = "macos") {
        let target = format!("{}/{LABEL}", gui_domain());
        // Not loaded is not an error here: the goal is "not running from this definition".
        let _ = manager.user(&["bootout", &target]).await;
    } else {
        manager.user(&["disable", "--now", UNIT]).await?;
    }
    let _ = std::fs::remove_file(&path);
    if !cfg!(target_os = "macos") {
        let _ = manager.user(&["daemon-reload"]).await;
    }
    let _ = std::fs::remove_file(layout.service_marker());
    Ok(Some(path))
}

/// Starts the hub through the service Cataly created (used by the restart option).
pub(crate) async fn start() -> Result<(), String> {
    let manager = Manager::of_session();
    if cfg!(target_os = "macos") {
        let target = format!("{}/{LABEL}", gui_domain());
        manager.user(&["kickstart", &target]).await
    } else {
        manager.user(&["start", UNIT]).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::fs::tests::{make_executable, scratch};
    use super::*;

    fn unit<'a>(path: &'a str, env_file: &'a Path) -> Unit<'a> {
        Unit {
            command: Path::new("/h/.local/bin/cataly"),
            path,
            env_file,
        }
    }

    #[test]
    fn definition_restarts_after_failure_with_path_and_keys() {
        let line = path_line([Path::new("/h/.bun/bin"), Path::new("/opt/homebrew/bin")]);
        let env_file = Path::new("/h/.config/cataly/secrets/service.env");
        let text = definition(&unit(&line, env_file));
        if cfg!(target_os = "macos") {
            assert!(text.contains(
                "<string>/h/.config/cataly/secrets/service.env</string><string>/h/.local/bin/cataly</string></array>"
            ));
            assert!(text.contains("<string>/bin/sh</string><string>-c</string>"));
            assert!(text.contains("exec &quot;$1&quot; hub"));
            assert!(text.contains(
                "<key>PATH</key><string>/h/.bun/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>"
            ));
            assert!(text.contains("<key>SuccessfulExit</key><false/>"));
            assert!(text.contains("<integer>5</integer>"));
        } else {
            assert!(text.contains("ExecStart=/h/.local/bin/cataly hub"));
            assert!(text.contains("Restart=on-failure\nRestartSec=5"));
            assert!(text.contains(
                "Environment=\"PATH=/h/.bun/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\"\n"
            ));
            assert!(text.contains("EnvironmentFile=-/h/.config/cataly/secrets/service.env\n"));
            assert!(text.contains("Environment=\"MALLOC_ARENA_MAX=2\"\n"));
        }
    }

    #[test]
    fn both_definitions_escape_their_values() {
        let env_file = Path::new("/h/100%/a&b");
        let text = definition(&unit("/x%y", env_file));
        if cfg!(target_os = "macos") {
            assert!(text.contains("<string>/h/100%/a&amp;b</string>"));
        } else {
            assert!(text.contains("EnvironmentFile=-/h/100%%/a&b\n"));
            assert!(text.contains("Environment=\"PATH=/x%%y\"\n"));
        }
    }

    #[test]
    fn path_line_keeps_order_drops_repeats_and_unusable_folders() {
        let dirs = [
            Path::new("/a"),
            Path::new("/usr/bin"),
            Path::new("/a"),
            Path::new("/b:c"),
            Path::new("/q\"x"),
        ];
        assert_eq!(
            path_line(dirs),
            "/a:/usr/bin:/usr/local/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn env_file_is_owner_only_written_once_and_reports_what_it_lacks() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("service-env");
        let path = dir.join("secrets/service.env");
        let names = vec![
            "ANTHROPIC_API_KEY".to_string(),
            "OPENAI_API_KEY".to_string(),
            "ODD_KEY".to_string(),
        ];
        let lookup = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk-ant-1".to_string()),
            "ODD_KEY" => Some("has space".to_string()),
            _ => None,
        };
        let first = env_file(&path, &names, lookup).unwrap();
        assert_eq!(
            first,
            EnvFile {
                created: true,
                missing: vec!["OPENAI_API_KEY".into(), "ODD_KEY".into()],
            }
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nANTHROPIC_API_KEY=sk-ant-1\n"), "{text}");
        assert!(text.contains("\n# OPENAI_API_KEY=\n"), "{text}");
        assert!(!text.contains("has space"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(dir.join("secrets"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        // An existing file is left as the owner wrote it.
        std::fs::write(&path, "OPENAI_API_KEY=sk-2\n").unwrap();
        let again = env_file(&path, &names, |_| None).unwrap();
        assert_eq!(
            again,
            EnvFile {
                created: false,
                missing: vec!["ANTHROPIC_API_KEY".into(), "ODD_KEY".into()],
            }
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "OPENAI_API_KEY=sk-2\n"
        );
    }

    #[tokio::test]
    async fn the_manager_gets_the_session_variables_and_nothing_else() {
        let dir = scratch("service-manager");
        let log = dir.join("calls");
        let fake = dir.join("fake-manager");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$* | XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR DBUS=$DBUS_SESSION_BUS_ADDRESS SECRET=$CATALY_TEST_SECRET\" >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        make_executable(&fake);
        let manager = Manager::new(&fake.display().to_string(), |k| match k {
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
            "DBUS_SESSION_BUS_ADDRESS" => Some("unix:path=/run/user/1000/bus".into()),
            "CATALY_TEST_SECRET" => Some("leak".into()),
            _ => None,
        });
        let file = dir.join("units/cataly.service");
        let marker = dir.join("marker");
        let env_file = dir.join("service.env");
        let line = path_line([Path::new("/h/.bun/bin")]);
        install(&file, &marker, &unit(&line, &env_file), &manager)
            .await
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        let session = "| XDG_RUNTIME_DIR=/run/user/1000 DBUS=unix:path=/run/user/1000/bus SECRET=";
        if cfg!(target_os = "macos") {
            let expected = format!(
                "bootstrap gui/{} {} {session}\n",
                super::super::own_uid(),
                file.display()
            );
            assert_eq!(calls, expected);
        } else {
            assert_eq!(
                calls,
                format!(
                    "--user daemon-reload {session}\n--user enable --now cataly.service {session}\n"
                )
            );
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            definition(&unit(&line, &env_file))
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            file.display().to_string()
        );

        // A definition Cataly did not write is never replaced.
        std::fs::remove_file(&marker).unwrap();
        let err = install(&file, &marker, &unit(&line, &env_file), &manager)
            .await
            .unwrap_err();
        assert!(err.contains("was not written by Cataly"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_manager_without_a_session_says_why() {
        let dir = scratch("service-nobus");
        let fake = dir.join("fake-manager");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'Failed to connect to bus' >&2\nexit 1\n",
        )
        .unwrap();
        make_executable(&fake);
        let manager = Manager::new(&fake.display().to_string(), |_| None);
        let err = manager.user(&["daemon-reload"]).await.unwrap_err();
        assert!(err.contains("Failed to connect to bus"), "{err}");
        if !cfg!(target_os = "macos") {
            assert!(err.contains("XDG_RUNTIME_DIR is not set"), "{err}");
        }
    }
}
