//! The pull-request follower (WS-24, WS-25) through `gh api`: the first read records a starting
//! point; later reads report new feedback from someone else, newly failed checks on the current
//! head, a head change bringing someone else's commits (or none), merge, close and reopen. All reads
//! of one PR finish before its state changes; a failure leaves that PR untouched and warns once per
//! distinct reason. A merged PR is never fetched again. Used by `pr` jobs (PRs the owner follows)
//! and `github` jobs (the PRs they discover).

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Tick;
use crate::config::owner::{Own, Pr};
use crate::jobs::event::{Emit, EventKind, Severity, one_line};

const GH_TIMEOUT: Duration = Duration::from_secs(60);
const GH_CAP: u64 = 8 * 1024 * 1024;
const PAGE: usize = 100;
const PAGES_MAX: usize = 30;
const EXCERPT: usize = 160;
/// Memory condition of a followed PR (value: start time).
pub(crate) const FOLLOW: &str = "follow";
const CURSOR: &str = "cursor";
const ERROR: &str = "error";
/// Failures in a row of one followed PR.
const FAILURES: &str = "failures";
/// Memory subject caching the authenticated `gh` login.
const OWN: &str = "_own";
const FAILED_CHECKS: [&str; 5] = [
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
];

/// What the follower remembers of one PR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub(crate) state: String,
    pub(crate) head: String,
    pub(crate) commits: BTreeSet<String>,
    pub(crate) seen: BTreeSet<String>,
    pub(crate) checks: BTreeSet<String>,
}

/// `owner/name#N` for a PR reference or a GitHub PR URL (WS req. 40); owner and name lower case.
pub(crate) fn parse_ref(text: &str) -> Option<String> {
    let text = text.trim();
    let (repo, number) = if let Some(rest) = text.strip_prefix("https://github.com/") {
        let mut parts = rest.trim_end_matches('/').split('/');
        let (owner, name, kind, n) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if kind != "pull" || parts.next().is_some() {
            return None;
        }
        (format!("{owner}/{name}"), n)
    } else {
        let (repo, n) = text.split_once('#')?;
        (repo.to_string(), n)
    };
    let n: u64 = number.parse().ok().filter(|n| *n > 0)?;
    let (owner, name) = repo.split_once('/')?;
    let ok = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    (ok(owner) && ok(name) && !name.contains('/'))
        .then(|| format!("{}#{n}", repo.to_ascii_lowercase()))
}

/// GitHub text is untrusted: one line, no control or direction characters, cut.
fn clean(text: &str) -> String {
    one_line(text.trim(), EXCERPT)
}

/// Links are shown only when they are https and contain no spaces.
fn link(url: Option<&str>) -> String {
    url.filter(|u| u.starts_with("https://") && !u.contains(char::is_whitespace))
        .unwrap_or("-")
        .to_string()
}

pub(super) struct Gh<'t, 'a> {
    pub(super) t: &'t Tick<'a>,
    pub(super) program: &'t str,
}

impl Gh<'_, '_> {
    /// Runs `gh` with arguments and returns its stdout as JSON.
    pub(super) async fn json(&self, args: &[&str]) -> Result<Value, String> {
        let mut line = vec![self.program.to_string()];
        line.extend(args.iter().map(|a| (*a).to_string()));
        let done = self
            .t
            .command(&line, None, &super::GH_ENV, GH_TIMEOUT, GH_CAP)
            .await?;
        super::gh_answer(args, &done)
    }

    async fn api(&self, path: &str) -> Result<Value, String> {
        self.json(&super::gh_api(path)).await
    }

    /// Every row of a list endpoint, 100 per page, at most 30 pages (WS req. 46). `key` names the
    /// array inside an object answer.
    async fn list(&self, path: &str, key: Option<&str>) -> Result<Vec<Value>, String> {
        let mut rows = Vec::new();
        for page in 1..=PAGES_MAX {
            let sep = if path.contains('?') { '&' } else { '?' };
            let answer = self
                .api(&format!("{path}{sep}per_page={PAGE}&page={page}"))
                .await?;
            let found = match key {
                Some(k) => answer.get(k).cloned().unwrap_or(Value::Null),
                None => answer,
            };
            let Value::Array(batch) = found else {
                return Err(format!("{path}: not a list"));
            };
            let n = batch.len();
            rows.extend(batch);
            if n < PAGE {
                return Ok(rows);
            }
        }
        Err(format!("{path}: more than {} rows", PAGE * PAGES_MAX))
    }
}

/// Own identity: configured login, else the authenticated user, read once and kept.
pub(super) async fn own_login(t: &mut Tick<'_>, gh: &str, own: &Own) -> Result<String, String> {
    if let Some(login) = &own.login {
        return Ok(login.to_ascii_lowercase());
    }
    if let Some(login) = t.memory.get(OWN, "login") {
        return Ok(login.to_string());
    }
    let user = Gh { t, program: gh }.api("/user").await?;
    let login = user
        .get("login")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .ok_or("the authenticated gh user has no login; set login in the job")?;
    t.memory.transition(OWN, "login", &login);
    Ok(login)
}

fn login_of(v: &Value) -> String {
    v.get("user")
        .or_else(|| v.get("author"))
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

struct Who<'a> {
    login: &'a str,
    own: &'a Own,
}

impl Who<'_> {
    fn is_own(&self, login: &str) -> bool {
        login == self.login || self.own.bots.iter().any(|b| b.eq_ignore_ascii_case(login))
    }

    fn commit_is_own(&self, c: &Value) -> bool {
        let email = c
            .pointer("/commit/author/email")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.is_own(&login_of(c))
            || self
                .own
                .emails
                .iter()
                .any(|e| e.eq_ignore_ascii_case(email))
    }
}

/// One PR's answers, all read before anything changes.
struct Read {
    pull: Value,
    issue_comments: Vec<Value>,
    review_comments: Vec<Value>,
    reviews: Vec<Value>,
    check_runs: Vec<Value>,
    statuses: Vec<Value>,
    commits: Option<Vec<Value>>,
}

fn state_of(pull: &Value) -> &'static str {
    if pull.get("merged").and_then(Value::as_bool) == Some(true)
        || pull.get("merged_at").is_some_and(|v| !v.is_null())
    {
        "merged"
    } else if pull.get("state").and_then(Value::as_str) == Some("closed") {
        "closed"
    } else {
        "open"
    }
}

async fn read(
    gh: &Gh<'_, '_>,
    pr: &str,
    cursor: Option<&Cursor>,
    pull: Value,
) -> Result<Read, String> {
    let (repo, n) = pr.split_once('#').ok_or("bad PR reference")?;
    let head = pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .ok_or("the PR has no head")?
        .to_string();
    let base = format!("/repos/{repo}");
    let moved = cursor.is_none_or(|c| c.head != head);
    Ok(Read {
        issue_comments: gh
            .list(&format!("{base}/issues/{n}/comments"), None)
            .await?,
        review_comments: gh.list(&format!("{base}/pulls/{n}/comments"), None).await?,
        reviews: gh.list(&format!("{base}/pulls/{n}/reviews"), None).await?,
        check_runs: gh
            .list(
                &format!("{base}/commits/{head}/check-runs"),
                Some("check_runs"),
            )
            .await?,
        statuses: gh
            .api(&format!("{base}/commits/{head}/status"))
            .await?
            .get("statuses")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        commits: if moved {
            Some(gh.list(&format!("{base}/pulls/{n}/commits"), None).await?)
        } else {
            None
        },
        pull,
    })
}

fn id_of(v: &Value) -> String {
    v.get("id").map_or_else(String::new, ToString::to_string)
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Feedback keys with their event: issue comments, review comments, and reviews except drafts and
/// empty plain-comment reviews; a review whose state changes is new.
fn feedback(r: &Read) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for c in &r.issue_comments {
        out.push((format!("ic:{}", id_of(c)), "comment".to_string(), c.clone()));
    }
    for c in &r.review_comments {
        out.push((
            format!("rc:{}", id_of(c)),
            "review comment".to_string(),
            c.clone(),
        ));
    }
    for v in &r.reviews {
        let state = v.get("state").and_then(Value::as_str).unwrap_or_default();
        let body = v.get("body").and_then(Value::as_str).unwrap_or_default();
        if state == "PENDING" || (state == "COMMENTED" && body.trim().is_empty()) {
            continue;
        }
        let what = format!("review: {}", state.to_ascii_lowercase().replace('_', " "));
        out.push((format!("rv:{}:{state}", id_of(v)), what, v.clone()));
    }
    out
}

fn failed_checks(r: &Read, head: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for c in &r.check_runs {
        let conclusion = c
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if FAILED_CHECKS.contains(&conclusion) {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("check");
            out.push((
                format!("cr:{}", id_of(c)),
                clean(name),
                conclusion.to_string(),
            ));
        }
    }
    for s in &r.statuses {
        let state = s.get("state").and_then(Value::as_str).unwrap_or_default();
        if state == "failure" || state == "error" {
            let context = s.get("context").and_then(Value::as_str).unwrap_or("status");
            out.push((
                format!("st:{context}:{head}"),
                clean(context),
                state.to_string(),
            ));
        }
    }
    out
}

/// Follows one PR; `announce` emits the non-urgent starting-point event on its first read.
pub(super) async fn follow_one(
    t: &mut Tick<'_>,
    gh: &str,
    who_login: &str,
    own: &Own,
    pr: &str,
    announce: bool,
) -> Result<(), String> {
    let cursor: Option<Cursor> = t
        .memory
        .get(pr, CURSOR)
        .and_then(|v| serde_json::from_str(v).ok());
    if cursor.as_ref().is_some_and(|c| c.state == "merged") {
        return Ok(());
    }
    let (repo, number) = pr.split_once('#').ok_or("bad PR reference")?;
    let client = Gh { t, program: gh };
    let pull = client.api(&format!("/repos/{repo}/pulls/{number}")).await?;
    let state = state_of(&pull);
    if let Some(c) = &cursor
        && c.state == "closed"
        && state == "closed"
    {
        return Ok(());
    }
    let got = read(&client, pr, cursor.as_ref(), pull).await?;
    let head = got
        .pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let who = Who {
        login: who_login,
        own,
    };
    let mut next = Cursor {
        state: state.to_string(),
        head: head.clone(),
        commits: cursor
            .as_ref()
            .map_or_else(Default::default, |c| c.commits.clone()),
        seen: BTreeSet::new(),
        checks: cursor
            .as_ref()
            .map_or_else(Default::default, |c| c.checks.clone()),
    };
    let mut emits = Vec::new();
    let feedback = feedback(&got);
    let failed = failed_checks(&got, &head);
    if let Some(commits) = &got.commits {
        next.commits = commits
            .iter()
            .filter_map(|c| c.get("sha").and_then(Value::as_str).map(str::to_string))
            .collect();
    }
    match &cursor {
        None => {
            if announce {
                let title = got
                    .pull
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                emits.push(
                    Emit::new(EventKind::PrBaseline, Severity::Info, false, pr)
                        .with("pr", pr)
                        .with("title", clean(title))
                        .with("state", state),
                );
            }
        }
        Some(c) => {
            emits.extend(news(pr, c, &who, &head, &feedback, &failed));
            if c.head != head
                && let Some(commits) = &got.commits
            {
                emits.extend(pushed(pr, c, commits, &who, &head, &next.commits));
            }
            emits.extend(moved(pr, &c.state, state, &got));
        }
    }
    next.seen = feedback.into_iter().map(|(k, _, _)| k).collect();
    next.checks.extend(failed.into_iter().map(|(k, _, _)| k));
    let value = serde_json::to_string(&next).map_err(|e| e.to_string())?;
    t.memory.transition(pr, CURSOR, &value);
    t.memory.clear(pr, ERROR);
    t.emits.extend(emits);
    Ok(())
}

/// Feedback by others and failed checks that the cursor has not seen.
fn news(
    pr: &str,
    before: &Cursor,
    who: &Who<'_>,
    head: &str,
    feedback: &[(String, String, Value)],
    failed: &[(String, String, String)],
) -> Vec<Emit> {
    let mut emits = Vec::new();
    for (key, what, item) in feedback {
        let author = login_of(item);
        if before.seen.contains(key) || who.is_own(&author) {
            continue;
        }
        let body = item.get("body").and_then(Value::as_str).unwrap_or_default();
        emits.push(
            Emit::new(EventKind::PrFeedback, Severity::Attention, true, pr)
                .with("pr", pr)
                .with("author", clean(&author))
                .with("what", what)
                .with("excerpt", clean(body))
                .with("url", link(item.get("html_url").and_then(Value::as_str))),
        );
    }
    for (key, check, conclusion) in failed {
        if !before.checks.contains(key) {
            emits.push(
                Emit::new(EventKind::PrCheckFailed, Severity::Warning, true, pr)
                    .with("pr", pr)
                    .with("check", check)
                    .with("conclusion", conclusion)
                    .with("head", short(head)),
            );
        }
    }
    emits
}

/// The event for a head that moved: new commits by others, or a push that added nothing known.
fn pushed(
    pr: &str,
    before: &Cursor,
    commits: &[Value],
    who: &Who<'_>,
    head: &str,
    now_commits: &BTreeSet<String>,
) -> Option<Emit> {
    let new: Vec<&Value> = commits
        .iter()
        .filter(|x| {
            x.get("sha")
                .and_then(Value::as_str)
                .is_some_and(|s| !before.commits.contains(s))
        })
        .collect();
    let rewritten = !now_commits.contains(&before.head);
    let foreign: BTreeSet<String> = new
        .iter()
        .filter(|x| !who.commit_is_own(x))
        .map(|x| {
            let login = login_of(x);
            if login.is_empty() {
                "unknown".to_string()
            } else {
                clean(&login)
            }
        })
        .collect();
    if !new.is_empty() && foreign.is_empty() {
        return None;
    }
    let authors: Vec<String> = foreign.into_iter().collect();
    Some(
        Emit::new(EventKind::PrPushed, Severity::Attention, true, pr)
            .with("pr", pr)
            .with(
                "authors",
                if authors.is_empty() {
                    "nobody".into()
                } else {
                    authors.join(", ")
                },
            )
            .with("count", new.len())
            .with("old", short(&before.head))
            .with("new", short(head))
            .with("rewritten", if rewritten { "yes" } else { "no" }),
    )
}

/// The event for a state change: merged, closed or reopened.
fn moved(pr: &str, before: &str, state: &str, got: &Read) -> Option<Emit> {
    match (before, state) {
        (s, "merged") if s != "merged" => {
            let commit = got
                .pull
                .get("merge_commit_sha")
                .and_then(Value::as_str)
                .map_or_else(|| "-".to_string(), short);
            Some(
                Emit::new(EventKind::PrMerged, Severity::Attention, true, pr)
                    .with("pr", pr)
                    .with("commit", commit),
            )
        }
        ("open", "closed") => {
            Some(Emit::new(EventKind::PrClosed, Severity::Attention, true, pr).with("pr", pr))
        }
        ("closed", "open") => {
            Some(Emit::new(EventKind::PrReopened, Severity::Attention, true, pr).with("pr", pr))
        }
        _ => None,
    }
}

/// Follows every PR this job follows; a PR that fails warns once per reason (a transient failure
/// only from the second in a row) and stays as it was.
pub(super) async fn follow_all(
    t: &mut Tick<'_>,
    gh: &str,
    own: &Own,
    announce: bool,
) -> Result<(), String> {
    let login = own_login(t, gh, own).await?;
    for (pr, _) in t.memory.with_condition(FOLLOW) {
        let Err(reason) = follow_one(t, gh, &login, own, &pr, announce).await else {
            t.memory.clear(&pr, FAILURES);
            continue;
        };
        let failures = t
            .memory
            .get(&pr, FAILURES)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        t.memory.transition(&pr, FAILURES, &failures.to_string());
        if super::reported(failures, &reason) && t.memory.transition(&pr, ERROR, &reason) {
            let name = t.job.name.clone();
            t.emit(
                Emit::new(EventKind::TickFailed, Severity::Warning, false, &pr)
                    .with("job", name)
                    .with("reason", format!("{pr}: {reason}")),
            );
        }
    }
    Ok(())
}

pub(super) async fn tick(t: &mut Tick<'_>, p: &Pr) -> Result<(), String> {
    follow_all(t, &p.gh, &p.own, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_and_urls() {
        assert_eq!(parse_ref("Me/Tool#12").as_deref(), Some("me/tool#12"));
        assert_eq!(
            parse_ref("https://github.com/Me/Tool/pull/7").as_deref(),
            Some("me/tool#7")
        );
        assert_eq!(parse_ref("https://github.com/Me/Tool/issues/7"), None);
        for bad in ["tool#1", "me/tool#0", "me/tool#x", "me/to ol#1", "a/b/c#1"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
        assert_eq!(link(Some("https://x/y")), "https://x/y");
        assert_eq!(link(Some("http://x")), "-");
        assert_eq!(link(Some("https://x y")), "-");
    }
}
