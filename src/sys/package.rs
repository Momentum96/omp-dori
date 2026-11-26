//! A Cataly installed by npm or Bun: the `cataly` package's launcher runs the binary of the
//! per-target package `cataly-<target>`, which holds the binary and its extension side by side.
//! A global install through the same package manager replaces that folder in place, so its path
//! stays valid across versions: the user service runs the binary there, and OMP's extension link
//! points at the extension there. `cataly update` hands the upgrade to the package manager that
//! installed it, found from the folder's place.
//!
//! ```text
//! npm  <prefix>/lib/node_modules/cataly/node_modules/cataly-<target>/{cataly, cataly.ts}
//!      <prefix>/lib/node_modules/cataly-<target>/...                 (hoisted)
//! Bun  <BUN_INSTALL>/install/global/node_modules/cataly-<target>/...
//! ```

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{KILL_GRACE, Output, Spec, run};
use super::install::{BINARY, EXTENSION};
use super::release::TARGET;

/// The npm name of the package carrying the binary and the extension.
pub(crate) const NAME: &str = "cataly";
/// A package manager's install: download and unpack, bounded.
const INSTALL_TIMEOUT: Duration = Duration::from_mins(5);
const INSTALL_OUTPUT: u64 = 256 * 1024;
/// What a package manager needs from the owner's shell besides the variables every child gets:
/// its own settings, proxies and certificates.
const MANAGER_ENV: [&str; 12] = [
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "NPM_CONFIG_USERCONFIG",
    "NPM_CONFIG_REGISTRY",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
];

/// The package manager that installed Cataly, and where it installs globally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Manager {
    /// npm, with its global prefix.
    Npm { prefix: PathBuf },
    /// Bun, with its install folder (`BUN_INSTALL`).
    Bun { home: PathBuf },
}

impl Manager {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Npm { .. } => "npm",
            Self::Bun { .. } => "bun",
        }
    }

    /// The command the owner would type to install `version` globally.
    pub(crate) fn install_hint(&self, version: &str) -> String {
        match self {
            Self::Npm { .. } => format!("npm install --global {NAME}@{version}"),
            Self::Bun { .. } => format!("bun add --global {NAME}@{version}"),
        }
    }

    /// The command that removes the global install.
    pub(crate) fn remove_hint(&self) -> String {
        match self {
            Self::Npm { .. } => format!("npm uninstall --global {NAME}"),
            Self::Bun { .. } => format!("bun remove --global {NAME}"),
        }
    }

    /// The argument list and extra environment that install `version` into this same place:
    /// the executable found on PATH, else the one beside the install.
    fn install_command(&self, version: &str) -> (Vec<String>, Vec<(&'static str, String)>) {
        let spec = format!("{NAME}@{version}");
        let executable = |name: &str, beside: PathBuf| {
            crate::omp::resolve(name)
                .unwrap_or(beside)
                .display()
                .to_string()
        };
        match self {
            Self::Npm { prefix } => (
                vec![
                    executable("npm", prefix.join("bin/npm")),
                    "install".into(),
                    "--global".into(),
                    "--prefix".into(),
                    prefix.display().to_string(),
                    spec,
                ],
                Vec::new(),
            ),
            Self::Bun { home } => (
                vec![
                    executable("bun", home.join("bin/bun")),
                    "add".into(),
                    "--global".into(),
                    spec,
                ],
                vec![("BUN_INSTALL", home.display().to_string())],
            ),
        }
    }

    /// Installs `version` globally through this package manager.
    pub(crate) async fn install(&self, version: &str) -> Result<(), String> {
        let (argv, own) = self.install_command(version);
        let mut values: Vec<(&str, String)> = MANAGER_ENV
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (*k, v)))
            .collect();
        values.extend(own);
        let env: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let name = self.name();
        let done = run(&Spec {
            argv: &argv,
            cwd: None,
            env: &env,
            timeout: INSTALL_TIMEOUT,
            kill_grace: KILL_GRACE,
            output_cap: INSTALL_OUTPUT,
            output: Output::Capture,
        })
        .await
        .map_err(|e| format!("{name} did not run: {e}"))?;
        if done.timed_out {
            return Err(format!("{name} did not finish within 5 minutes"));
        }
        if done.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&done.stderr);
        let last = said
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no message");
        Err(format!("`{}` failed: {last}", self.install_hint(version)))
    }
}

/// Cataly as a package install: the per-target package folder and, when its place says so, the
/// package manager that put it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Package {
    /// `.../node_modules/cataly-<target>`.
    pub(crate) dir: PathBuf,
    pub(crate) manager: Option<Manager>,
}

impl Package {
    /// The running binary's package, when it runs from one.
    pub(crate) fn of_running() -> Option<Self> {
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .ok()?;
        Self::at(&exe)
    }

    /// The package `exe` belongs to, when it is `.../node_modules/cataly-<target>/cataly`.
    pub(crate) fn at(exe: &Path) -> Option<Self> {
        let dir = exe.parent()?;
        let folder = format!("{NAME}-{TARGET}");
        if exe.file_name()? != BINARY || dir.file_name()? != folder.as_str() {
            return None;
        }
        let modules = dir.parent().filter(|m| named(m, "node_modules"))?;
        Some(Self {
            dir: dir.to_path_buf(),
            manager: manager(modules),
        })
    }

    pub(crate) fn binary(&self) -> PathBuf {
        self.dir.join(BINARY)
    }

    pub(crate) fn extension(&self) -> PathBuf {
        self.dir.join(EXTENSION)
    }

    /// Points OMP's extension link at this package's extension: made when absent, replaced when
    /// it is a link that does not hold this binary's bridge (a removed or older install), left
    /// alone when it already does. Anything that is not a link is not Cataly's and is refused.
    pub(crate) fn link_extension(&self) -> Result<(), String> {
        link_extension_at(&crate::omp::extension::link()?, &self.extension())
    }

    /// Removes OMP's extension link when it points at this package.
    pub(crate) fn unlink_extension(&self) -> Result<(), String> {
        let link = crate::omp::extension::link()?;
        if std::fs::read_link(&link).ok().as_deref() == Some(self.extension().as_path()) {
            std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
        Ok(())
    }
}

fn named(path: &Path, name: &str) -> bool {
    path.file_name() == Some(OsStr::new(name))
}

/// The package manager whose global layout holds `modules` (the `node_modules` folder above the
/// per-target package).
fn manager(modules: &Path) -> Option<Manager> {
    let up = modules.parent()?;
    if named(up, "global") {
        let install = up.parent().filter(|i| named(i, "install"))?;
        return Some(Manager::Bun {
            home: install.parent()?.to_path_buf(),
        });
    }
    // npm: hoisted into `<prefix>/lib/node_modules`, or nested under the `cataly` package there.
    let lib = if named(up, "lib") {
        up
    } else if named(up, NAME) {
        let outer = up.parent().filter(|o| named(o, "node_modules"))?;
        outer.parent().filter(|l| named(l, "lib"))?
    } else {
        return None;
    };
    Some(Manager::Npm {
        prefix: lib.parent()?.to_path_buf(),
    })
}

fn link_extension_at(link: &Path, target: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            return Err(format!(
                "{} exists and is not Cataly's link; move it away first",
                link.display()
            ));
        }
        Ok(_) if std::fs::read(link).ok() == std::fs::read(target).ok() => return Ok(()),
        _ => {}
    }
    let dir = link.parent().ok_or("the extension link has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = dir.join(format!(".{}.part", super::install::EXTENSION));
    let _ = std::fs::remove_file(&part);
    std::os::unix::fs::symlink(target, &part).map_err(|e| format!("{}: {e}", part.display()))?;
    std::fs::rename(&part, link).map_err(|e| format!("{}: {e}", link.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    fn exe(base: &str) -> PathBuf {
        Path::new(base)
            .join(format!("{NAME}-{TARGET}"))
            .join(BINARY)
    }

    #[test]
    fn finds_the_package_and_its_manager_from_the_path() {
        let npm = Package::at(&exe("/usr/local/lib/node_modules/cataly/node_modules")).unwrap();
        assert_eq!(
            npm.manager,
            Some(Manager::Npm {
                prefix: "/usr/local".into()
            })
        );
        assert_eq!(
            npm.binary(),
            exe("/usr/local/lib/node_modules/cataly/node_modules")
        );
        let hoisted = Package::at(&exe("/opt/n/lib/node_modules")).unwrap();
        assert_eq!(
            hoisted.manager,
            Some(Manager::Npm {
                prefix: "/opt/n".into()
            })
        );
        let bun = Package::at(&exe("/home/u/.bun/install/global/node_modules")).unwrap();
        assert_eq!(
            bun.manager,
            Some(Manager::Bun {
                home: "/home/u/.bun".into()
            })
        );
        assert_eq!(bun.extension(), bun.dir.join(EXTENSION));
        let local = Package::at(&exe("/work/app/node_modules")).unwrap();
        assert_eq!(local.manager, None, "a project's own node_modules");

        for other in [
            PathBuf::from("/home/u/.local/share/cataly/versions/0.2.0/cataly"),
            PathBuf::from("/x/node_modules/cataly-other/cataly"),
            Path::new("/x/lib")
                .join(format!("{NAME}-{TARGET}"))
                .join(BINARY),
            Path::new("/x/node_modules")
                .join(format!("{NAME}-{TARGET}"))
                .join("other"),
        ] {
            assert_eq!(Package::at(&other), None, "{}", other.display());
        }
    }

    #[test]
    fn install_commands_target_the_same_place() {
        let npm = Manager::Npm {
            prefix: "/p".into(),
        };
        let (argv, env) = npm.install_command("0.3.0");
        assert_eq!(
            argv.get(1..).unwrap(),
            ["install", "--global", "--prefix", "/p", "cataly@0.3.0"]
        );
        assert_eq!(env, Vec::<(&str, String)>::new());
        let bun = Manager::Bun { home: "/b".into() };
        let (argv, env) = bun.install_command("0.3.0");
        assert_eq!(argv.get(1..).unwrap(), ["add", "--global", "cataly@0.3.0"]);
        assert_eq!(env, [("BUN_INSTALL", "/b".to_string())]);
        assert_eq!(bun.remove_hint(), "bun remove --global cataly");
    }

    #[test]
    fn the_extension_link_follows_the_package() {
        let dir = scratch("package-link");
        let link = dir.join("agent/extensions/cataly.ts");
        let (old, new) = (dir.join("old.ts"), dir.join("new.ts"));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        link_extension_at(&link, &old).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), old);
        link_extension_at(&link, &new).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            new,
            "a stale link is replaced"
        );
        std::fs::write(&old, "new").unwrap();
        link_extension_at(&link, &old).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            new,
            "a link holding the same bridge stays"
        );
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "mine").unwrap();
        assert!(link_extension_at(&link, &new).is_err());
        assert_eq!(std::fs::read(&link).unwrap(), b"mine");
    }
}
