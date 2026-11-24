//! The GitHub watch (OJ-05, OJ-06) through `gh`: discovers the newest 40 PRs per watched repository
//! and author and hands them to the PR follower, and reports participating notifications (mention,
//! team mention, review requested) for watched repositories once per identity. Discovery and
//! notifications are read before anything changes; a failure of either leaves the tick's state as
//! it was.

use serde_json::Value;

use super::Tick;
use super::pr::{self, FOLLOW, Gh};
use crate::config::owner::Github;
use crate::jobs::event::{Emit, EventKind, Severity, one_line};

const LIST_LIMIT: &str = "40";
const TITLE_MAX: usize = 60;
const NOTICE_TITLE_MAX: usize = 80;
const REASONS: [&str; 3] = ["mention", "team_mention", "review_requested"];
/// Remembered notification identities are forgotten after 30 days.
const SEEN_FOR_MS: i64 = 30 * 86_400_000;
const NOTICE: &str = "notice";

struct Listed {
    key: String,
    repo: String,
    number: u64,
    title: String,
    state: String,
    author: String,
    url: String,
}

pub(super) async fn tick(t: &mut Tick<'_>, g: &Github) -> Result<(), String> {
    let mut listed = Vec::new();
    let mut notices = Vec::new();
    {
        let gh = Gh { t, program: &g.gh };
        for watched in &g.repos {
            for author in &watched.authors {
                let answer = gh
                    .json(&[
                        "pr",
                        "list",
                        "--repo",
                        &watched.repo,
                        "--author",
                        author,
                        "--state",
                        "all",
                        "--limit",
                        LIST_LIMIT,
                        "--json",
                        "number,title,state,url,author",
                    ])
                    .await?;
                let rows = answer.as_array().ok_or("gh pr list printed no list")?;
                for row in rows {
                    let number = row
                        .get("number")
                        .and_then(Value::as_u64)
                        .ok_or("a listed PR has no number")?;
                    let text = |k: &str| {
                        row.get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    listed.push(Listed {
                        key: format!("{}#{number}", watched.repo),
                        repo: watched.repo.clone(),
                        number,
                        title: text("title"),
                        state: text("state").to_ascii_lowercase(),
                        author: row
                            .pointer("/author/login")
                            .and_then(Value::as_str)
                            .unwrap_or(author)
                            .to_string(),
                        url: text("url"),
                    });
                }
            }
        }
        if g.notifications {
            let answer = gh
                .json(&["api", "/notifications?participating=true&per_page=50"])
                .await?;
            notices = answer
                .as_array()
                .cloned()
                .ok_or("gh api /notifications printed no list")?;
        }
    }
    discover(t, &listed);
    notify(t, g, &notices);
    pr::follow_all(t, &g.gh, &g.own, false).await
}

fn discover(t: &mut Tick<'_>, listed: &[Listed]) {
    let now = t.now.to_string();
    for pr in listed {
        if t.memory.get(&pr.key, FOLLOW).is_some() {
            continue;
        }
        t.memory.transition(&pr.key, FOLLOW, &now);
        if pr.state == "merged" {
            // Never fetched: nothing can happen to a merged PR.
            let cursor = pr::Cursor {
                state: "merged".into(),
                ..pr::Cursor::default()
            };
            if let Ok(v) = serde_json::to_string(&cursor) {
                t.memory.transition(&pr.key, "cursor", &v);
            }
        }
        if !t.first {
            t.emit(
                Emit::new(EventKind::GithubPr, Severity::Attention, true, &pr.key)
                    .with("repo", &pr.repo)
                    .with("number", pr.number)
                    .with("title", one_line(&pr.title, TITLE_MAX))
                    .with("state", &pr.state)
                    .with("author", one_line(&pr.author, 40))
                    .with(
                        "url",
                        if pr.url.starts_with("https://") {
                            &pr.url
                        } else {
                            "-"
                        },
                    ),
            );
        }
    }
    // A finished PR that left the newest-40 lists is forgotten.
    for (key, _) in t.memory.with_condition(FOLLOW) {
        let finished = t
            .memory
            .get(&key, "cursor")
            .and_then(|v| serde_json::from_str::<pr::Cursor>(v).ok())
            .is_some_and(|c| c.state != "open");
        if finished && !listed.iter().any(|p| p.key == key) {
            t.memory.clear(&key, FOLLOW);
            t.memory.clear(&key, "cursor");
        }
    }
}

fn notify(t: &mut Tick<'_>, g: &Github, notices: &[Value]) {
    for n in notices {
        let text = |p: &str| n.pointer(p).and_then(Value::as_str).unwrap_or_default();
        let repo = text("/repository/full_name").to_ascii_lowercase();
        let reason = text("/reason");
        if !REASONS.contains(&reason) || !g.repos.iter().any(|w| w.repo == repo) {
            continue;
        }
        let identity = format!("{}@{}", text("/id"), text("/updated_at"));
        if t.memory.get(&identity, NOTICE).is_some() {
            continue;
        }
        t.memory.transition(&identity, NOTICE, &t.now.to_string());
        if !t.first {
            t.emit(
                Emit::new(
                    EventKind::GithubNotification,
                    Severity::Attention,
                    true,
                    &identity,
                )
                .with("repo", &repo)
                .with("reason", reason.replace('_', " "))
                .with("title", one_line(text("/subject/title"), NOTICE_TITLE_MAX))
                .with("kind", one_line(text("/subject/type"), 40)),
            );
        }
    }
    for (identity, at) in t.memory.with_condition(NOTICE) {
        if at.parse::<i64>().is_ok_and(|at| t.now - at > SEEN_FOR_MS) {
            t.memory.clear(&identity, NOTICE);
        }
    }
}
