//! `cataly setup --backup [--backup-repo owner/name]` (owner decision 16): the Cataly home (the state
//! folder) is backed up to a private GitHub repository, by default `<gh login>/cataly-home`. The
//! preview reads the `gh` login and the repository's visibility; an existing public repository is
//! refused, an absent one is only offered, and it is created private (`gh repo create --private`)
//! only when the owner confirms with `--apply --create`. Applying makes the state folder a git
//! working tree with a `.gitignore` that keeps token files, secrets and the live database files out,
//! sets the remote and adds the backup job, which copies the config file in and snapshots the store.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::Failure;
use super::args::Parsed;
use crate::sys::command::{KILL_GRACE, Output, Spec};
use crate::sys::paths::Paths;

const REPO: &str = "cataly-home";
/// The backup job the setup adds.
pub(super) const JOB: &str = "home";
const GH_TIMEOUT: Duration = Duration::from_secs(60);
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_CAP: u64 = 1024 * 1024;

/// Ignored in the home: token files and secrets, and the live store with its side files. The store
/// snapshot the job writes is let back in.
const IGNORE: [&str; 12] = [
    "*token*",
    "*secret*",
    "*.pem",
    "*.key",
    "*.db",
    "!/store/cataly.db",
    "*-wal",
    "*-shm",
    "*-journal",
    "*.tmp",
    "*.lock",
    "*.sock",
];
const IGNORE_HEAD: &str =
    "# Cataly home backup: never token files, secrets or live database files.";

/// What GitHub says about the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Private,
    Absent,
}

pub(super) struct Home {
    repo: String,
    visibility: Visibility,
    dir: PathBuf,
    name: String,
    email: String,
}

async fn command(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    let done = crate::sys::command::run(&Spec {
        argv,
        cwd,
        env: &[("GIT_TERMINAL_PROMPT", "0"), ("GH_PROMPT_DISABLED", "1")],
        timeout,
        kill_grace: KILL_GRACE,
        output_cap: OUTPUT_CAP,
        output: Output::Capture,
    })
    .await
    .map_err(|e| format!("{}: {e}", argv.first().map_or("", String::as_str)))?;
    if done.timed_out {
        return Err(format!(
            "{} did not finish within {} s",
            argv.first().map_or("", String::as_str),
            timeout.as_secs()
        ));
    }
    Ok((
        done.success(),
        String::from_utf8_lossy(&done.stdout).into_owned(),
        String::from_utf8_lossy(&done.stderr).into_owned(),
    ))
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_string()).collect()
}

fn first_line(text: &str) -> String {
    crate::sys::secrets::mask(
        text.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default(),
    )
    .into_owned()
}

async fn gh_json(rest: &[&str]) -> Result<Result<Value, String>, String> {
    let mut line = words(&["gh"]);
    line.extend(words(rest));
    let (ok, stdout, stderr) = command(&line, None, GH_TIMEOUT).await?;
    if !ok {
        return Ok(Err(stderr));
    }
    serde_json::from_str(&stdout)
        .map(Ok)
        .map_err(|_| format!("gh {} did not answer JSON", rest.join(" ")))
}

fn valid_repo(repo: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !s.starts_with('.')
    };
    repo.split_once('/')
        .is_some_and(|(o, n)| part(o) && part(n))
}

/// The repository's visibility; a public one is refused (decision 16).
async fn visibility(repo: &str) -> Result<Visibility, String> {
    match gh_json(&["api", &format!("repos/{repo}")]).await? {
        Ok(v) => match v.get("private").and_then(Value::as_bool) {
            Some(true) => Ok(Visibility::Private),
            Some(false) => Err(format!(
                "{repo} is public; the home backup never goes to a public repository. Make it \
                 private or name another with --backup-repo"
            )),
            None => Err(format!("gh did not say whether {repo} is private")),
        },
        Err(stderr) if stderr.contains("404") || stderr.contains("Not Found") => {
            Ok(Visibility::Absent)
        }
        Err(stderr) => Err(format!("gh could not read {repo}: {}", first_line(&stderr))),
    }
}

/// Reads the `gh` login and the repository; refuses a public one.
pub(super) async fn plan(p: &Parsed, paths: &Paths) -> Result<Home, Failure> {
    let user = gh_json(&["api", "user"])
        .await
        .map_err(Failure::Refused)?
        .map_err(|e| {
            Failure::Refused(format!(
                "gh is not logged in ({}); run `gh auth login` first",
                first_line(&e)
            ))
        })?;
    let login = user
        .get("login")
        .and_then(Value::as_str)
        .filter(|l| valid_repo(&format!("{l}/{REPO}")))
        .ok_or_else(|| Failure::Refused("gh did not report a usable login".into()))?
        .to_string();
    let repo = p
        .value("--backup-repo")
        .map_or_else(|| format!("{login}/{REPO}"), str::to_string);
    if !valid_repo(&repo) {
        return Err(Failure::Usage("--backup-repo: expected owner/name".into()));
    }
    let visibility = visibility(&repo).await.map_err(Failure::Refused)?;
    let name = user
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&login)
        .to_string();
    let email = match user.get("id").and_then(Value::as_u64) {
        Some(id) => format!("{id}+{login}@users.noreply.github.com"),
        None => format!("{login}@users.noreply.github.com"),
    };
    Ok(Home {
        repo,
        visibility,
        dir: paths.state_dir.clone(),
        name,
        email,
    })
}

impl Home {
    fn url(&self) -> String {
        format!("https://github.com/{}.git", self.repo)
    }

    /// The config with the backup job set; other keys are kept.
    pub(super) fn config(&self, text: &str) -> String {
        let section = format!("jobs.{JOB}");
        let mut text = super::setup::set_key(text, &section, "kind", "backup");
        for (key, value) in [
            ("dir", self.dir.display().to_string()),
            ("remote", "origin".to_string()),
            ("branch", "main".to_string()),
            ("name", self.name.clone()),
            ("email", self.email.clone()),
        ] {
            text = super::setup::set_key(&text, &section, key, &value);
        }
        super::setup::set_rendered(&text, &section, "include_cataly", "true")
    }

    pub(super) fn report(&self) -> Value {
        json!({
            "repository": self.repo,
            "remote": self.url(),
            "state": match self.visibility {
                Visibility::Private => "private",
                Visibility::Absent => "absent",
            },
            "dir": self.dir.display().to_string(),
            "job": JOB,
            "ignored": IGNORE,
        })
    }

    pub(super) fn lines(&self, apply: bool) -> Vec<String> {
        let mut out = vec![format!(
            "  home backup: {} to {} (job {JOB}, config copied in, store as a snapshot)",
            self.dir.display(),
            self.repo
        )];
        out.push(match (self.visibility, apply) {
            (Visibility::Private, _) => format!("  {} exists and is private", self.repo),
            (Visibility::Absent, true) => format!("  {} was created private", self.repo),
            (Visibility::Absent, false) => format!(
                "  {} does not exist; confirm with --create to create it private (gh repo create --private)",
                self.repo
            ),
        });
        out.push(format!("  ignored: {}", IGNORE.join(" ")));
        out
    }

    /// Creates the repository private when confirmed, then prepares the working tree.
    pub(super) async fn apply(&self, create: bool) -> Result<(), String> {
        if self.visibility == Visibility::Absent {
            if !create {
                return Err(format!(
                    "{} does not exist; the preview offers to create it private: add --create",
                    self.repo
                ));
            }
            let argv = words(&["gh", "repo", "create", &self.repo, "--private"]);
            let (ok, _, stderr) = command(&argv, None, GH_TIMEOUT).await?;
            if !ok {
                return Err(format!("gh repo create failed: {}", first_line(&stderr)));
            }
            // Fail closed: the repository must read back as private.
            if visibility(&self.repo).await? != Visibility::Private {
                return Err(format!("{} did not read back as private", self.repo));
            }
        }
        crate::sys::fs::ensure_private_dir(&self.dir).map_err(|e| e.to_string())?;
        if !self.dir.join(".git").exists() {
            self.git(&["init", "--quiet", "--initial-branch=main"])
                .await?;
        }
        self.ignore()?;
        let url = self.url();
        let verb = if self.git(&["remote", "get-url", "origin"]).await.is_ok() {
            "set-url"
        } else {
            "add"
        };
        self.git(&["remote", verb, "origin", &url]).await?;
        Ok(())
    }

    async fn git(&self, rest: &[&str]) -> Result<String, String> {
        let mut line = words(&["git", "-C", &self.dir.display().to_string()]);
        line.extend(words(rest));
        let (ok, stdout, stderr) = command(&line, None, GIT_TIMEOUT).await?;
        if ok {
            Ok(stdout)
        } else {
            Err(format!(
                "git {} failed: {}",
                rest.first().unwrap_or(&""),
                first_line(&stderr)
            ))
        }
    }

    /// Adds the missing ignore lines; the owner's own lines are kept.
    fn ignore(&self) -> Result<(), String> {
        let path = self.dir.join(".gitignore");
        let mut text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {}", path.display(), e.kind())),
        };
        let missing: Vec<&str> = IGNORE
            .iter()
            .copied()
            .filter(|l| !text.lines().any(|have| have.trim() == *l))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.contains(IGNORE_HEAD) {
            text.push_str(IGNORE_HEAD);
            text.push('\n');
        }
        for l in missing {
            text.push_str(l);
            text.push('\n');
        }
        crate::sys::fs::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_names() {
        assert!(valid_repo("someone/cataly-home"));
        assert!(!valid_repo("someone"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("../x"));
        assert!(!valid_repo("a/ b"));
    }
}
