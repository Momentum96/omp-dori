//! Backup of a git working tree to a private repository with a secret scan (OJ-15): stage every
//! change, scan what is staged with the built-in token shapes and the owner's patterns, refuse files
//! over the size limit, and only then commit with the configured identity and push to the configured
//! branch. Never force-pushes, rewrites history or pulls. A refusal unstages everything and alerts
//! once per distinct set of files, naming files and rule but never the matched text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Tick;
use crate::config::owner::Backup;
use crate::jobs::event::{Emit, EventKind, Severity, one_line};
use crate::sys::secrets;

const LOCAL: Duration = Duration::from_secs(60);
const PUSH: Duration = Duration::from_secs(300);
const GIT_CAP: u64 = 4 * 1024 * 1024;
const SCAN: &str = "scan";
const PUSHED: &str = "push";

struct Git<'t, 'a> {
    t: &'t Tick<'a>,
    dir: &'t Path,
    index: Option<PathBuf>,
}

impl Git<'_, '_> {
    async fn run(
        &self,
        args: &[&str],
        limit: Duration,
    ) -> Result<crate::sys::command::Finished, String> {
        let mut line = vec![
            "git".to_string(),
            "-C".to_string(),
            self.dir.display().to_string(),
        ];
        line.extend(args.iter().map(|a| (*a).to_string()));
        let index = self.index.as_ref().map(|p| p.display().to_string());
        let mut env = vec![("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0")];
        if let Some(index) = &index {
            env.push(("GIT_INDEX_FILE", index.as_str()));
        }
        self.t.command(&line, None, &env, limit, GIT_CAP).await
    }

    async fn ok(&self, args: &[&str], limit: Duration) -> Result<Vec<u8>, String> {
        let done = self.run(args, limit).await?;
        if !done.success() {
            return Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                super::said(&done)
            ));
        }
        Ok(done.stdout)
    }
}

/// Paths in `-z` output.
fn nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

pub(super) async fn tick(t: &mut Tick<'_>, b: &Backup) -> Result<(), String> {
    let git_dir = b.dir.join(".git");
    if git_dir.join("index.lock").exists() {
        return Err("a git lock is held by another process; skipped".into());
    }
    if b.include_cataly && !t.dry {
        cataly_files(t, b).await?;
    }
    // A dry tick stages into a private copy of the index, so the real index is never touched.
    let index = if t.dry {
        let copy = t.ctx.files_dir.join(format!("backup-{}.index", t.job.name));
        let _ = std::fs::remove_file(&copy);
        if git_dir.join("index").exists() {
            std::fs::copy(git_dir.join("index"), &copy).map_err(|e| format!("index copy: {e}"))?;
        }
        Some(copy)
    } else {
        None
    };
    let result = run(
        t,
        b,
        &Git {
            t,
            dir: &b.dir,
            index: index.clone(),
        },
    )
    .await;
    if let Some(copy) = index {
        let _ = std::fs::remove_file(copy);
    }
    let outcome = result?;
    apply(t, outcome);
    Ok(())
}

/// The Cataly home backup (owner decision 16): the config file copied to `config/cataly.toml` and a
/// consistent store snapshot at `store/cataly.db`, both scanned like every other file. The live
/// store, its WAL and journal stay ignored.
async fn cataly_files(t: &Tick<'_>, b: &Backup) -> Result<(), String> {
    let config = b.dir.join("config");
    std::fs::create_dir_all(&config).map_err(|e| format!("config copy: {}", e.kind()))?;
    if let Ok(Some(bytes)) = crate::sys::fs::read_private(&t.ctx.config_file, 1024 * 1024) {
        crate::sys::fs::write_atomic(&config.join("cataly.toml"), &bytes)
            .map_err(|e| e.to_string())?;
    }
    let store = b.dir.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("store snapshot: {}", e.kind()))?;
    let tmp = store.join(".cataly.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    let target = tmp.clone();
    t.ctx
        .store
        .read(move |r| r.snapshot_into(&target))
        .await
        .map_err(|e| format!("store snapshot: {e}"))?;
    std::fs::rename(&tmp, store.join("cataly.db"))
        .map_err(|e| format!("store snapshot: {}", e.kind()))
}

/// What a tick found, applied to memory and events after git is done.
enum Found {
    Nothing {
        push: Option<Result<(), String>>,
    },
    Refused {
        files: Vec<String>,
        rule: String,
    },
    Committed {
        commit: String,
        push: Option<Result<(), String>>,
    },
}

async fn run(t: &Tick<'_>, b: &Backup, git: &Git<'_, '_>) -> Result<Found, String> {
    let own = own_files_not_ignored(t, b, git).await?;
    git.ok(&["add", "-A"], LOCAL).await?;
    let staged = nul_list(
        &git.ok(&["diff", "--cached", "--name-only", "-z"], LOCAL)
            .await?,
    );
    if staged.is_empty() && own.is_empty() {
        let push = if t.dry || t.memory.get(PUSHED, "pending").is_none() {
            None
        } else {
            Some(push(git, b).await)
        };
        return Ok(Found::Nothing { push });
    }
    let mut hits: Vec<(String, String)> = own
        .into_iter()
        .map(|f| {
            (
                f,
                "the hub's own config or secrets, not ignored".to_string(),
            )
        })
        .collect();
    for path in &staged {
        let full = b.dir.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue; // a staged deletion
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > b.max_file_bytes {
            hits.push((
                path.clone(),
                format!("file over {} MiB", b.max_file_bytes / 1024 / 1024),
            ));
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|e| format!("reading {path}: {}", e.kind()))?;
        if let Some(rule) = secrets::find_rule(&bytes) {
            hits.push((path.clone(), rule.to_string()));
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        if let Some(re) = b.patterns.iter().find(|re| re.is_match(&text)) {
            hits.push((path.clone(), format!("owner pattern {}", re.as_str())));
        }
    }
    if !hits.is_empty() {
        git.ok(&["reset", "-q"], LOCAL).await?;
        hits.sort();
        let mut rules: Vec<String> = hits.iter().map(|(_, r)| r.clone()).collect();
        rules.sort();
        rules.dedup();
        return Ok(Found::Refused {
            files: hits.into_iter().map(|(f, _)| f).collect(),
            rule: rules.join("; "),
        });
    }
    if t.dry {
        return Ok(Found::Committed {
            commit: "dry run, not committed".into(),
            push: None,
        });
    }
    let message = format!(
        "backup {}",
        jiff::Timestamp::from_millisecond(t.now)
            .map_or_else(|_| t.now.to_string(), |s| s.to_string())
    );
    let name = format!("user.name={}", b.name);
    let email = format!("user.email={}", b.email);
    git.ok(
        &[
            "-c",
            &name,
            "-c",
            &email,
            "commit",
            "-q",
            "--no-verify",
            "--no-gpg-sign",
            "-m",
            &message,
        ],
        LOCAL,
    )
    .await?;
    let head = git.ok(&["rev-parse", "--short", "HEAD"], LOCAL).await?;
    Ok(Found::Committed {
        commit: String::from_utf8_lossy(&head).trim().to_string(),
        push: Some(push(git, b).await),
    })
}

/// The hub's config file or secrets directory inside the tree and not ignored (OJ req. 40).
async fn own_files_not_ignored(
    t: &Tick<'_>,
    b: &Backup,
    git: &Git<'_, '_>,
) -> Result<Vec<String>, String> {
    let config = t.ctx.config_file.clone();
    let secrets = config.parent().map(|d| d.join("secrets"));
    let mut found = Vec::new();
    for path in [Some(config), secrets].into_iter().flatten() {
        let Ok(rel) = path.strip_prefix(&b.dir) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let rel = rel.display().to_string();
        let done = git.run(&["check-ignore", "-q", &rel], LOCAL).await?;
        match done.exit {
            crate::sys::command::Exit::Code(0) => {}
            crate::sys::command::Exit::Code(1) => found.push(rel),
            _ => return Err(format!("git check-ignore failed: {}", super::said(&done))),
        }
    }
    Ok(found)
}

/// Pushes HEAD to the configured branch. Never forced.
async fn push(git: &Git<'_, '_>, b: &Backup) -> Result<(), String> {
    let target = format!("HEAD:refs/heads/{}", b.branch);
    let done = git
        .run(&["push", "--quiet", &b.remote, &target], PUSH)
        .await?;
    if done.success() {
        Ok(())
    } else {
        Err(super::said(&done))
    }
}

fn apply(t: &mut Tick<'_>, found: Found) {
    let push = match found {
        Found::Nothing { push } => push,
        Found::Refused { files, rule } => {
            let list = files.join(", ");
            // The alert repeats only when the set of files changes.
            if t.memory.transition(SCAN, "refused", &list) {
                t.emit(
                    Emit::new(EventKind::BackupRefused, Severity::Error, true, SCAN)
                        .with("files", one_line(&list, 300))
                        .with("rule", rule),
                );
            }
            None
        }
        Found::Committed { commit, push } => {
            if t.memory.clear(SCAN, "refused").is_some() {
                t.emit(
                    Emit::new(EventKind::BackupClean, Severity::Info, false, SCAN)
                        .with("commit", &commit),
                );
            }
            if !t.dry {
                t.memory.transition(PUSHED, "pending", &commit);
            }
            push
        }
    };
    match push {
        Some(Ok(())) => {
            t.memory.clear(PUSHED, "pending");
            t.memory.clear(PUSHED, "error");
        }
        Some(Err(reason)) if t.memory.transition(PUSHED, "error", &reason) => {
            t.emit(
                Emit::new(EventKind::BackupPushFailed, Severity::Warning, true, PUSHED)
                    .with("reason", reason),
            );
        }
        Some(Err(_)) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::jobs::owner::tests::live_tick;
    use crate::jobs::tests::ctx_with;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// A bare remote and a work tree whose `origin` is that remote.
    fn repos(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let remote = root.join("remote.git");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let remote_arg = remote.to_str().unwrap();
        git(root, &["init", "-q", "--bare", "-b", "main", remote_arg]);
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", remote_arg]);
        (remote, work)
    }

    /// Moves the remote on with a commit made in a separate clone.
    fn push_from_another_clone(root: &Path, remote: &Path) {
        let other = root.join("other");
        let (remote_arg, other_arg) = (remote.to_str().unwrap(), other.to_str().unwrap());
        git(root, &["clone", "-q", remote_arg, other_arg]);
        std::fs::write(other.join("theirs"), "x").unwrap();
        git(&other, &["add", "theirs"]);
        let identity = ["-c", "user.name=o", "-c", "user.email=o@x"];
        let commit = ["commit", "-q", "-m", "theirs"];
        git(&other, &[&identity[..], &commit[..]].concat());
        git(&other, &["push", "-q", "origin", "main"]);
    }

    #[tokio::test]
    async fn refuses_tokens_and_big_files_then_commits_and_pushes_without_force() {
        let root = crate::sys::fs::tests::scratch("backup");
        let (remote, work) = repos(&root);
        let cfg = format!(
            "[jobs.bk]\nkind = \"backup\"\ndir = \"{}\"\nname = \"Backup\"\nemail = \"b@example.org\"\npatterns = [\"PRIVATE-[0-9]{{4}}\"]\n",
            work.display()
        );
        let (ctx, _stop, _dir) = ctx_with(&cfg, "backup-ctx");
        std::fs::write(work.join("notes.md"), "plain notes\n").unwrap();
        let token = format!("ghp_{}", "Ab3".repeat(12));
        std::fs::write(work.join("env.sh"), format!("export T={token}\n")).unwrap();
        assert_eq!(live_tick(&ctx, "bk").await, ["backup.refused"]);
        let events = ctx.store.read(|r| r.events_after(0, 10)).await.unwrap();
        assert!(
            events[0].text.contains("env.sh") && events[0].text.contains("GitHub token"),
            "{}",
            events[0].text
        );
        assert!(!events[0].text.contains(&token) && !events[0].text.contains("notes.md"));
        assert_eq!(
            git(&work, &["diff", "--cached", "--name-only"]),
            "",
            "unstaged again"
        );
        assert!(
            live_tick(&ctx, "bk").await.is_empty(),
            "same files: no second alert"
        );
        // A 60 MiB file joins the set: alert again.
        let big = std::fs::File::create(work.join("big.bin")).unwrap();
        big.set_len(60 * 1024 * 1024).unwrap();
        assert_eq!(live_tick(&ctx, "bk").await, ["backup.refused"]);
        let events = ctx.store.read(|r| r.events_after(0, 10)).await.unwrap();
        assert!(
            events[1].text.contains("big.bin") && events[1].text.contains("over 50 MiB"),
            "{}",
            events[1].text
        );
        std::fs::remove_file(work.join("big.bin")).unwrap();
        std::fs::write(work.join("env.sh"), "PRIVATE-1234\n").unwrap();
        assert!(
            live_tick(&ctx, "bk")
                .await
                .contains(&"backup.refused".to_string()),
            "owner pattern"
        );
        std::fs::remove_file(work.join("env.sh")).unwrap();
        assert_eq!(live_tick(&ctx, "bk").await, ["backup.clean"]);
        assert_eq!(
            git(&remote, &["log", "--format=%an", "main"]).trim(),
            "Backup"
        );
        // A remote that moved on is never overwritten: the push fails and is reported once.
        push_from_another_clone(&root, &remote);
        std::fs::write(work.join("notes.md"), "more\n").unwrap();
        assert_eq!(live_tick(&ctx, "bk").await, ["backup.push_failed"]);
        std::fs::write(work.join("notes.md"), "more and more\n").unwrap();
        assert!(
            live_tick(&ctx, "bk").await.is_empty(),
            "same reason: no repeat"
        );
        assert!(
            git(&remote, &["log", "--format=%s", "main"]).starts_with("theirs"),
            "never forced"
        );
    }
}
