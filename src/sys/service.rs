//! The optional user service (IR req. 40, DESIGN 11.2): a systemd user unit on Linux, a launchd agent
//! on macOS. It runs the hub through the command link, restarts it 5 s after a failure, and is
//! written, enabled, started, stopped and removed only by explicit commands. The service manager is
//! driven through its own command line under the bounded subprocess rules.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{KILL_GRACE, Output, Spec, run};
use super::install::Layout;

/// systemd unit name and launchd label.
const UNIT: &str = "cataly.service";
const LABEL: &str = "dev.cataly.hub";
const MANAGER_TIMEOUT: Duration = Duration::from_secs(15);

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

/// The unit or agent text for a hub run through `command`.
pub(crate) fn definition(command: &Path) -> String {
    let exe = command.display();
    if cfg!(target_os = "macos") {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{LABEL}</string>\n\
  <key>ProgramArguments</key><array><string>{exe}</string><string>hub</string></array>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
  <key>ThrottleInterval</key><integer>5</integer>\n\
</dict>\n</plist>\n"
        )
    } else {
        format!(
            "[Unit]\nDescription=Cataly hub\n\n[Service]\nType=notify\nNotifyAccess=main\n\
ExecStart={exe} hub\nRestart=on-failure\nRestartSec=5\nWatchdogSec=30\n\n\
[Install]\nWantedBy=default.target\n"
        )
    }
}

fn gui_domain() -> String {
    format!("gui/{}", super::own_uid())
}

async fn manager(argv: &[&str]) -> Result<(), String> {
    let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
    let done = run(&Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: MANAGER_TIMEOUT,
        kill_grace: KILL_GRACE,
        output_cap: 64 * 1024,
        output: Output::Capture,
    })
    .await
    .map_err(|e| e.to_string())?;
    if done.success() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&done.stderr);
        Err(format!(
            "`{}` failed: {}",
            argv.join(" "),
            said.lines().next().unwrap_or("no message")
        ))
    }
}

/// Writes the definition, enables and starts it, and records that Cataly created it.
pub(crate) async fn enable(layout: &Layout) -> Result<PathBuf, String> {
    let path = file()?;
    if path.exists() && !layout.service_marker().exists() {
        return Err(format!(
            "{} exists and was not written by Cataly; move it away first",
            path.display()
        ));
    }
    let dir = path.parent().ok_or("the service file has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    super::fs::write_atomic(&path, definition(&layout.command).as_bytes())
        .map_err(|e| e.to_string())?;
    super::fs::write_atomic(
        &layout.service_marker(),
        path.display().to_string().as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    if cfg!(target_os = "macos") {
        let plist = path.display().to_string();
        manager(&["launchctl", "bootstrap", &gui_domain(), &plist]).await?;
    } else {
        manager(&["systemctl", "--user", "daemon-reload"]).await?;
        manager(&["systemctl", "--user", "enable", "--now", UNIT]).await?;
    }
    Ok(path)
}

/// Stops, disables and removes the service Cataly created; anything else is left alone.
pub(crate) async fn disable(layout: &Layout) -> Result<Option<PathBuf>, String> {
    if !layout.service_marker().exists() {
        return Ok(None);
    }
    let path = file()?;
    if cfg!(target_os = "macos") {
        let target = format!("{}/{LABEL}", gui_domain());
        // Not loaded is not an error here: the goal is "not running from this definition".
        let _ = manager(&["launchctl", "bootout", &target]).await;
    } else {
        manager(&["systemctl", "--user", "disable", "--now", UNIT]).await?;
    }
    let _ = std::fs::remove_file(&path);
    if !cfg!(target_os = "macos") {
        let _ = manager(&["systemctl", "--user", "daemon-reload"]).await;
    }
    let _ = std::fs::remove_file(layout.service_marker());
    Ok(Some(path))
}

/// Starts the hub through the service Cataly created (used by the restart option).
pub(crate) async fn start() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        manager(&[
            "launchctl",
            "kickstart",
            &format!("{}/{LABEL}", gui_domain()),
        ])
        .await
    } else {
        manager(&["systemctl", "--user", "start", UNIT]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_restarts_after_failure() {
        let text = definition(Path::new("/h/.local/bin/cataly"));
        if cfg!(target_os = "macos") {
            assert!(text.contains("<string>/h/.local/bin/cataly</string><string>hub</string>"));
            assert!(text.contains("<key>SuccessfulExit</key><false/>"));
            assert!(text.contains("<integer>5</integer>"));
        } else {
            assert!(text.contains("ExecStart=/h/.local/bin/cataly hub"));
            assert!(text.contains("Restart=on-failure\nRestartSec=5"));
        }
    }
}
