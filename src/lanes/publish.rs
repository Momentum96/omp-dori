//! Published files and learned rules (EVR-17, EVR-19, EVR-27; EVR req. 25-29, 45-47). A file is
//! registered with its hash, size, title, visibility and public link under a configured root; a
//! change in bytes, title or visibility adds a version. Nothing is uploaded or fetched. Learned rules
//! are one-line lessons scoped to a repository or global, put before the brief of new lanes.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::docs::{self, FILE, RULE};
use super::record::Record;
use super::{Lanes, need, s};
use crate::config::coord::PublishRoot;
use crate::host::Runtime;
use crate::sys::now_ms;

const ALLOWED: [&str; 31] = [
    "html", "htm", "css", "js", "mjs", "json", "png", "jpg", "jpeg", "gif", "webp", "svg", "ico",
    "woff", "woff2", "ttf", "otf", "md", "txt", "csv", "pdf", "mp3", "wav", "ogg", "m4a", "mp4",
    "webm", "mov", "avif", "flac", "markdown",
];
const DEPENDENCY_DIRS: [&str; 6] = [
    "node_modules",
    "vendor",
    "bower_components",
    "site-packages",
    "venv",
    "__pycache__",
];
const GLOBAL_RULES: usize = 12;
const REPO_RULES: usize = 8;
const FILE_MAX: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Version {
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) title: String,
    pub(crate) public: bool,
    pub(crate) link: Option<String>,
    pub(crate) at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Published {
    pub(crate) lane: String,
    pub(crate) path: String,
    pub(crate) versions: Vec<Version>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rule {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) evidence: String,
    pub(crate) lane: Option<String>,
    /// `global`, or the canonical directory of one repository.
    pub(crate) scope: String,
    pub(crate) at_ms: i64,
    pub(crate) active: bool,
}

fn one_line(text: &str, max: usize, what: &str) -> Result<String, String> {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > max || t.contains(['\n', '\r']) {
        return Err(format!("{what} must be one line of 1 to {max} characters"));
    }
    Ok(t.to_string())
}

/// The root holding `path`, longest first.
fn root_of<'a>(roots: &'a [PublishRoot], path: &Path) -> Option<&'a PublishRoot> {
    roots
        .iter()
        .filter(|r| path.starts_with(&r.dir))
        .max_by_key(|r| r.dir.as_os_str().len())
}

fn encode(rel: &str) -> String {
    let mut out = String::new();
    for b in rel.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The parts below the root that refuse a file (EVR req. 26).
fn part_refused(rel: &Path) -> Option<String> {
    rel.components().find_map(|c| {
        let part = c.as_os_str().to_string_lossy().to_string();
        let low = part.to_ascii_lowercase();
        let bad = part.starts_with('.')
            || ["secret", "credential", "token", "private"]
                .iter()
                .any(|w| low.contains(w))
            || DEPENDENCY_DIRS.contains(&low.as_str());
        bad.then_some(part)
    })
}

/// The hash and size of a regular file, refused when it changed while hashed.
fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let before = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !before.is_file() {
        return Err("not a regular file".into());
    }
    if before.len() > FILE_MAX {
        return Err("larger than 512 MiB".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let after = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
        || u64::try_from(bytes.len()).ok() != Some(after.len())
    {
        return Err("the file changed while it was hashed".into());
    }
    Ok((crate::config::sha256_hex(&bytes), after.len()))
}

/// The canonical file, its canonical root and the root it was named under, with every path rule of
/// EVR req. 26 checked before and after resolving links.
fn locate(roots: &[PublishRoot], given: &Path) -> Result<(PathBuf, PathBuf, PublishRoot), String> {
    let root = root_of(roots, given).ok_or("the file is not inside a publish root")?;
    let real = std::fs::canonicalize(given).map_err(|e| format!("{}: {e}", given.display()))?;
    let real_root =
        std::fs::canonicalize(&root.dir).map_err(|e| format!("{}: {e}", root.dir.display()))?;
    let real_roots: Vec<PublishRoot> = roots
        .iter()
        .filter_map(|r| {
            std::fs::canonicalize(&r.dir).ok().map(|dir| PublishRoot {
                dir,
                url: r.url.clone(),
            })
        })
        .collect();
    if root_of(&real_roots, &real).map(|r| &r.dir) != Some(&real_root) {
        return Err("the file leaves its publish root through a link".into());
    }
    for below in [given.strip_prefix(&root.dir), real.strip_prefix(&real_root)] {
        let below = below.map_err(|_| "the file is not inside its root")?;
        if let Some(part) = part_refused(below) {
            return Err(format!("the path part '{part}' may not be published"));
        }
    }
    let ext = real
        .extension()
        .and_then(|e| e.to_str())
        .map_or_default(str::to_ascii_lowercase);
    if !ALLOWED.contains(&ext.as_str()) {
        return Err(format!("the extension '{ext}' is not on the allow list"));
    }
    Ok((real, real_root, root.clone()))
}

impl<R: Runtime> Lanes<R> {
    /// Registers a file for a lane (EVR req. 25-27). `own` is the calling lane's key, if a lane calls.
    pub(super) async fn publish(&self, input: &Value, own: Option<&str>) -> Result<Value, String> {
        let config = self.cfg();
        let roots = &config.coord.publish;
        if roots.is_empty() {
            return Err("publishing is off: no [[coord.publish]] roots".into());
        }
        let key = match own {
            Some(k) => k.to_string(),
            None => need(input, "key")?.to_string(),
        };
        let (rec, _) = self.load(&key).await?;
        if !rec.open() {
            return Err(format!("lane {key} is not open"));
        }
        let given = PathBuf::from(need(input, "path")?);
        if !given.is_absolute() {
            return Err("path must be absolute".into());
        }
        let (real, real_root, root) = locate(roots, &given)?;
        let public = match s(input, "visibility").unwrap_or("private") {
            "public" => true,
            "private" => false,
            _ => return Err("visibility is private or public".into()),
        };
        if public && root.url.is_none() {
            return Err("public needs a root with a url".into());
        }
        let real_text = real.display().to_string();
        self.outside_other_worktrees(&key, &real).await?;
        let (sha256, size) = hash_file(&real)?;
        let default_title = real
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let title = one_line(
            s(input, "title").unwrap_or(&default_title),
            200,
            "the title",
        )?;
        let link = public.then(|| {
            let below = real
                .strip_prefix(&real_root)
                .unwrap_or(&real)
                .to_string_lossy()
                .into_owned();
            format!(
                "{}/{}",
                root.url
                    .as_deref()
                    .unwrap_or_default()
                    .trim_end_matches('/'),
                encode(&below)
            )
        });
        let version = Version {
            sha256,
            size,
            title,
            public,
            link,
            at_ms: now_ms(),
        };
        let earlier = docs::get::<Published>(&self.env.store, FILE, &real_text).await?;
        let (mut doc, expect) = match earlier {
            Some((d, v)) => (d, Some(v)),
            None => (
                Published {
                    lane: key.clone(),
                    path: real_text.clone(),
                    versions: Vec::new(),
                },
                Some(0),
            ),
        };
        let same = doc.versions.last().is_some_and(|l| {
            l.sha256 == version.sha256 && l.title == version.title && l.public == version.public
        });
        if same {
            return Ok(
                json!({ "path": real_text, "versions": doc.versions.len(), "added": false }),
            );
        }
        doc.lane.clone_from(&key);
        doc.versions.push(version);
        docs::put(&self.env.store, FILE, &real_text, &doc, expect).await?;
        Ok(
            json!({ "path": real_text, "versions": doc.versions.len(), "added": true, "link": doc.versions.last().and_then(|v| v.link.clone()) }),
        )
    }

    /// Refuses a file inside a worktree of another open lane (EVR req. 26).
    async fn outside_other_worktrees(&self, key: &str, real: &Path) -> Result<(), String> {
        let rows = self
            .env
            .store
            .read(|r| r.lanes(true))
            .await
            .map_err(|e| e.to_string())?;
        for other in rows {
            let Ok(o) = serde_json::from_str::<Record>(&other.record) else {
                continue;
            };
            if o.key != key && o.worktrees.iter().any(|w| real.starts_with(w)) {
                return Err(format!("the file lies in a worktree of lane {}", o.key));
            }
        }
        Ok(())
    }

    /// Each file with its latest version and live state (EVR req. 28); nothing is fetched.
    pub(super) async fn files(&self, key: Option<&str>) -> Result<Value, String> {
        let (all, damaged) = docs::all::<Published>(&self.env.store, FILE).await?;
        let out: Vec<Value> = all
            .into_iter()
            .filter(|(d, _)| key.is_none_or(|k| d.lane == k))
            .map(|(d, _)| {
                let last = d.versions.last().cloned();
                let state = match (&last, hash_file(Path::new(&d.path))) {
                    (Some(l), Ok((sha, _))) if sha == l.sha256 => "current".to_string(),
                    (_, Ok(_)) => "changed".into(),
                    (_, Err(e)) => format!("unavailable: {e}"),
                };
                json!({ "lane": d.lane, "path": d.path, "latest": last, "versions": d.versions.len(), "state": state })
            })
            .collect();
        Ok(json!({ "files": out, "damaged": damaged }))
    }

    /// Lines appended to a finished report (EVR req. 29).
    pub(super) async fn published_lines(&self, key: &str) -> String {
        let Ok(v) = self.files(Some(key)).await else {
            return String::new();
        };
        let mut text = String::new();
        let mut private = 0;
        for f in v
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let latest = &f["latest"];
            if latest["public"] != true {
                private += 1;
                continue;
            }
            let title = latest["title"].as_str().unwrap_or_default();
            if f["state"] == "current" {
                let _ = write!(
                    text,
                    "\npublished: {title} {} sha256 {}",
                    latest["link"].as_str().unwrap_or_default(),
                    latest["sha256"].as_str().unwrap_or_default()
                );
            } else {
                let _ = write!(text, "\nwithheld (not current): {title}");
            }
        }
        if private > 0 {
            let _ = write!(text, "\n{private} private file(s) registered");
        }
        text
    }

    /// Whether a worktree holds a published file (EVR-19: kept at close).
    pub(super) async fn holds_published(&self, worktree: &str) -> bool {
        docs::all::<Published>(&self.env.store, FILE)
            .await
            .is_ok_and(|(all, _)| {
                all.iter()
                    .any(|(d, _)| Path::new(&d.path).starts_with(worktree))
            })
    }

    /// EVR req. 45-46: lanes add and retire rules of their own repository; only the lead adds global
    /// rules or changes their state. Authority comes from the connection (`own`).
    pub(super) async fn rule_op(
        &self,
        op: &str,
        input: &Value,
        own: Option<&Record>,
    ) -> Result<Value, String> {
        match op {
            "rule-add" => {
                let text = one_line(need(input, "text")?, 400, "the rule")?;
                let evidence = one_line(need(input, "evidence")?, 2000, "the evidence")?;
                let (scope, lane) = match (own, s(input, "scope").unwrap_or("repo")) {
                    (Some(_), "global") => return Err("only the lead adds global rules".into()),
                    (Some(rec), _) => (rec.cwd.clone(), Some(rec.key.clone())),
                    (None, "global") => ("global".to_string(), s(input, "key").map(str::to_string)),
                    (None, _) => {
                        let (rec, _) = self.load(need(input, "key")?).await?;
                        (rec.cwd.clone(), Some(rec.key))
                    }
                };
                let at = now_ms();
                let id = format!("{at:016}-{}", crate::host::random_hex(3));
                let rule = Rule {
                    id: id.clone(),
                    text,
                    evidence,
                    lane,
                    scope,
                    at_ms: at,
                    active: true,
                };
                docs::put(&self.env.store, RULE, &id, &rule, Some(0)).await?;
                Ok(json!(format!("rule {id} added for {}", rule.scope)))
            }
            "rule-retire" => {
                let id = need(input, "id")?;
                let (mut rule, v) = docs::get::<Rule>(&self.env.store, RULE, id)
                    .await?
                    .ok_or_else(|| format!("no rule {id}"))?;
                if let Some(rec) = own
                    && rule.scope != rec.cwd
                {
                    return Err("a lane retires only rules of its own repository".into());
                }
                rule.active = false;
                docs::put(&self.env.store, RULE, id, &rule, Some(v)).await?;
                Ok(json!(format!("rule {id} retired")))
            }
            _ => {
                let (all, damaged) = docs::all::<Rule>(&self.env.store, RULE).await?;
                let rules: Vec<Rule> = all.into_iter().map(|(r, _)| r).collect();
                Ok(json!({ "rules": rules, "damaged": damaged }))
            }
        }
    }

    /// The rules put before a new lane's brief, and those left out (EVR req. 47).
    pub(super) async fn rules_for(&self, rec: &Record) -> (String, Vec<String>) {
        let Ok((all, _)) = docs::all::<Rule>(&self.env.store, RULE).await else {
            return (String::new(), Vec::new());
        };
        let mut global: Vec<&Rule> = all
            .iter()
            .map(|(r, _)| r)
            .filter(|r| r.active && r.scope == "global")
            .collect();
        let mut repo: Vec<&Rule> = all
            .iter()
            .map(|(r, _)| r)
            .filter(|r| {
                r.active && r.scope != "global" && Path::new(&rec.cwd).starts_with(&r.scope)
            })
            .collect();
        global.sort_by_key(|r| std::cmp::Reverse(r.at_ms));
        repo.sort_by_key(|r| std::cmp::Reverse(r.at_ms));
        let left: Vec<String> = global
            .iter()
            .skip(GLOBAL_RULES)
            .chain(repo.iter().skip(REPO_RULES))
            .map(|r| r.id.clone())
            .collect();
        let mut text = String::new();
        for (head, list, max) in [
            ("Learned rules (global)", &global, GLOBAL_RULES),
            ("Learned rules (this repository)", &repo, REPO_RULES),
        ] {
            if list.is_empty() {
                continue;
            }
            let _ = writeln!(text, "{head}:");
            for r in list.iter().take(max) {
                let _ = writeln!(text, "- {}", r.text);
            }
            text.push('\n');
        }
        (text, left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_roots_and_links() {
        let roots = [
            PublishRoot {
                dir: "/srv".into(),
                url: None,
            },
            PublishRoot {
                dir: "/srv/pub".into(),
                url: Some("https://x.test".into()),
            },
        ];
        assert_eq!(
            root_of(&roots, Path::new("/srv/pub/a.md")).unwrap().dir,
            PathBuf::from("/srv/pub")
        );
        assert!(root_of(&roots, Path::new("/etc/a.md")).is_none());
        assert!(part_refused(Path::new("a/.git/x.md")).is_some());
        assert!(part_refused(Path::new("My-Tokens/x.md")).is_some());
        assert!(part_refused(Path::new("node_modules/x.js")).is_some());
        assert!(part_refused(Path::new("docs/x.md")).is_none());
        assert_eq!(encode("a b/ü.md"), "a%20b/%C3%BC.md");
    }
}
