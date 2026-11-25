//! The board post (EVR-30; EVR req. 51-53): one status message per configured destination, rebuilt
//! when lanes change, edited at most once a minute and only when its body changed. The creation is
//! recorded as pending first; an uncertain creation is never repeated, and the destination stays in
//! doubt until the lead supplies the message ID it found.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::docs::{self, BOARD};
use super::record::Record;
use super::{BoxFut, Lanes, need};
use crate::host::{Control, Runtime};
use crate::sys::now_ms;

/// Discord's 2000 characters with a margin (EVR req. 51).
const BODY_MAX: usize = 1900;
const REPORT_MAX: usize = 240;
pub(crate) const EVERY_MS: i64 = 60_000;

/// What the board needs from a messenger.
pub(crate) trait Board: Send + Sync {
    /// Creates a message; `Err((uncertain, why))`.
    fn board_create<'a>(
        &'a self,
        channel: &'a str,
        text: &'a str,
        nonce: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>>;
    fn board_edit<'a>(
        &'a self,
        channel: &'a str,
        message: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Post {
    pub(crate) message: Option<String>,
    /// `pending` while a create is in flight, `doubt` after an uncertain one.
    pub(crate) state: Option<String>,
    pub(crate) body: String,
    pub(crate) at_ms: i64,
    pub(crate) attempts: u32,
}

/// One step for one destination; returns the post to store, or `None` when nothing is due. `write`
/// stores the pending mark before the create.
pub(crate) async fn step<B: Board + ?Sized, F, Fut>(
    board: &B,
    channel: &str,
    post: &Post,
    body: &str,
    now: i64,
    write: F,
) -> Option<Post>
where
    F: FnOnce(Post) -> Fut,
    Fut: Future<Output = bool>,
{
    if post.state.is_some() || post.body == body || now - post.at_ms < EVERY_MS {
        return None;
    }
    let mut next = Post {
        body: body.to_string(),
        at_ms: now,
        ..post.clone()
    };
    if let Some(id) = &post.message {
        if let Err(e) = board.board_edit(channel, id, body).await {
            crate::log::warn("board edit failed", &[("channel", &channel), ("error", &e)]);
            return Some(Post {
                at_ms: now,
                ..post.clone()
            });
        }
        return Some(next);
    }
    next.attempts += 1;
    let pending = Post {
        state: Some("pending".into()),
        ..next.clone()
    };
    if !write(pending).await {
        return None;
    }
    let nonce = format!("board:{channel}:{}", next.attempts);
    Some(match board.board_create(channel, body, &nonce).await {
        Ok(id) => Post {
            message: Some(id),
            state: None,
            ..next
        },
        Err((true, why)) => {
            crate::log::warn(
                "board creation uncertain",
                &[("channel", &channel), ("error", &why)],
            );
            Post {
                state: Some("doubt".into()),
                ..next
            }
        }
        Err((false, why)) => {
            crate::log::warn(
                "board creation refused",
                &[("channel", &channel), ("error", &why)],
            );
            Post {
                state: None,
                body: String::new(),
                ..next
            }
        }
    })
}

fn cut(text: &str, max: usize) -> String {
    super::evidence::cut(&super::evidence::display(text), max)
}

impl<R: Runtime> Lanes<R> {
    /// The body (EVR req. 51), without the update time when `stamp` is false.
    async fn board_body(&self, now: i64) -> String {
        let rows = self
            .env
            .store
            .read(|r| r.lanes(true))
            .await
            .unwrap_or_default();
        let questions = self
            .env
            .store
            .read(|r| r.questions(true))
            .await
            .map_or(0, |q| q.len());
        let mut lines = Vec::new();
        for row in &rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                lines.push(format!("{}: record damaged", row.key));
                continue;
            };
            let snap = self.session(&rec.key).map(|s| s.snapshot());
            let agent = snap.as_ref().map_or("gone".to_string(), |s| {
                format!("{:?}", s.status).to_lowercase()
            });
            let approvals = snap.as_ref().map_or(0, |s| s.dialogs.len());
            let (k, run) = (rec.key.clone(), rec.run_id.clone());
            let last = self
                .env
                .store
                .read(move |r| r.reports(&k, Some(&run), 1))
                .await
                .ok()
                .and_then(|v| v.into_iter().next());
            let mut line = format!(
                "{} | {} | {agent}{}",
                rec.key,
                rec.status(),
                if approvals > 0 {
                    " | approval pending"
                } else {
                    ""
                }
            );
            if let Some(r) = last {
                let _ = write!(line, "\n  {}: {}", r.kind, cut(&r.text, REPORT_MAX));
            }
            lines.push(line);
        }
        let mut body = format!("{} lane(s), {questions} open question(s)\n", rows.len());
        let stamp = format!("updated {}", crate::jobs::instant_text(now));
        let mut shown = 0;
        for line in &lines {
            if body.len() + line.len() + stamp.len() + 24 > BODY_MAX {
                break;
            }
            body.push_str(line);
            body.push('\n');
            shown += 1;
        }
        if shown < lines.len() {
            let _ = writeln!(body, "{} more", lines.len() - shown);
        }
        body.push_str(&stamp);
        body
    }

    /// One board pass over every destination; returns the next time one is due.
    pub(super) async fn board_tick(&self) -> Option<i64> {
        let destinations = self.cfg().coord.board.clone();
        if destinations.is_empty() {
            return None;
        }
        let now = now_ms();
        let body = self.board_body(now).await;
        // The update time alone is no change.
        let content = |b: &str| {
            b.rsplit_once("\nupdated ")
                .map_or(b, |(c, _)| c)
                .to_string()
        };
        let mut next: Option<i64> = None;
        for dest in destinations {
            let Some(channel) = dest.strip_prefix("discord:") else {
                continue;
            };
            let (post, version) = match docs::get::<Post>(&self.env.store, BOARD, &dest).await {
                Ok(Some(p)) => p,
                Ok(None) => (Post::default(), 0),
                Err(e) => {
                    crate::log::warn("board record unreadable", &[("error", &e)]);
                    continue;
                }
            };
            if post.state.is_some() || content(&post.body) == content(&body) {
                continue;
            }
            if now - post.at_ms < EVERY_MS {
                let due = post.at_ms + EVERY_MS;
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            let store = self.env.store.clone();
            let d = dest.clone();
            let mut stored = version;
            let write = |pending: Post| async move {
                docs::put(&store, BOARD, &d, &pending, Some(version))
                    .await
                    .is_ok()
            };
            if post.message.is_none() {
                stored += 1;
            }
            let posted = step(self.env.threads.as_ref(), channel, &post, &body, now, write).await;
            if let Some(p) = posted
                && let Err(e) = docs::put(&self.env.store, BOARD, &dest, &p, Some(stored)).await
            {
                crate::log::warn("board record not saved", &[("error", &e)]);
            }
        }
        next
    }

    /// The lead found the message of an uncertain creation (EVR req. 53).
    pub(super) async fn board_found(&self, input: &Value) -> Result<Value, String> {
        let dest = need(input, "destination")?;
        let message = need(input, "message")?;
        if !message.chars().all(|c| c.is_ascii_digit()) || !(17..=20).contains(&message.len()) {
            return Err("message must be a Discord message ID".into());
        }
        let (mut post, v) = docs::get::<Post>(&self.env.store, BOARD, dest)
            .await?
            .ok_or_else(|| format!("no board post for {dest}"))?;
        if post.state.is_none() {
            return Err("that board post is not in doubt".into());
        }
        post.state = None;
        post.message = Some(message.to_string());
        docs::put(&self.env.store, BOARD, dest, &post, Some(v)).await?;
        // The body may have changed while in doubt: the next pass edits the found message.
        self.nudge.notify_one();
        Ok(json!(format!(
            "board post for {dest} is message {message}; later updates edit it"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        answer: Mutex<Option<Result<String, (bool, String)>>>,
    }

    impl Board for Fake {
        fn board_create<'a>(
            &'a self,
            _c: &'a str,
            text: &'a str,
            _n: &'a str,
        ) -> BoxFut<'a, Result<String, (bool, String)>> {
            self.calls.lock().unwrap().push(format!("create {text}"));
            let a = self
                .answer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok("700000000000000001".into()));
            Box::pin(async move { a })
        }
        fn board_edit<'a>(
            &'a self,
            _c: &'a str,
            m: &'a str,
            text: &'a str,
        ) -> BoxFut<'a, Result<(), String>> {
            self.calls.lock().unwrap().push(format!("edit {m} {text}"));
            Box::pin(async { Ok(()) })
        }
    }

    async fn ok(_: Post) -> bool {
        true
    }

    /// Acceptance check 6: created once, edited on change at most once a minute, never recreated
    /// after an uncertain create.
    #[tokio::test]
    async fn created_once_edited_per_minute_never_recreated_in_doubt() {
        let fake = Fake::default();
        let p0 = Post::default();
        let p1 = step(&fake, "c", &p0, "one", 100_000, ok).await.unwrap();
        assert_eq!(p1.message.as_deref(), Some("700000000000000001"));
        assert!(
            step(&fake, "c", &p1, "one", 400_000, ok).await.is_none(),
            "unchanged body"
        );
        assert!(
            step(&fake, "c", &p1, "two", 130_000, ok).await.is_none(),
            "within the minute"
        );
        let p2 = step(&fake, "c", &p1, "two", 160_001, ok).await.unwrap();
        assert_eq!(p2.body, "two");
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
        assert!(fake.calls.lock().unwrap()[1].starts_with("edit 700000000000000001"));
        // An uncertain create leaves the destination in doubt; nothing is created again.
        let fake = Fake::default();
        *fake.answer.lock().unwrap() = Some(Err((true, "timeout".into())));
        let d = step(&fake, "c", &Post::default(), "one", 100_000, ok)
            .await
            .unwrap();
        assert_eq!(d.state.as_deref(), Some("doubt"));
        assert!(step(&fake, "c", &d, "three", 900_000, ok).await.is_none());
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
        // A clear refusal clears the pending mark, so a later pass may try again.
        let fake = Fake::default();
        *fake.answer.lock().unwrap() = Some(Err((false, "403".into())));
        let r = step(&fake, "c", &Post::default(), "one", 100_000, ok)
            .await
            .unwrap();
        assert!(r.state.is_none() && r.message.is_none());
        // A pending mark that cannot be stored stops before the create.
        let fake = Fake::default();
        assert!(
            step(&fake, "c", &Post::default(), "one", 100_000, |_| async {
                false
            })
            .await
            .is_none()
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}
